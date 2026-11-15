# Why k3s's ServiceLB failed in Hearth, and why it is off

With stock k3s on Hearth's guest kernel, one pod in `kube-system` never
ran: `svclb-traefik-*`, ServiceLB's pod for traefik's Service, stuck in
`CrashLoopBackOff`. `make-k3s-rootfs.sh` now starts k3s without
ServiceLB, and without traefik, whose Service was the only user of it:

```yaml
# /etc/rancher/k3s/config.yaml
disable:
  - traefik
  - servicelb
```

This file explains the failure, the options, and why we chose this one.

## What ServiceLB does

Kubernetes leaves `type: LoadBalancer` Services to the platform: a cloud
provider normally gives each one an external address. k3s ships
ServiceLB (the `klipper-lb` image) so that such Services work on bare
nodes too. For each LoadBalancer Service it runs a DaemonSet,
`svclb-<service>`, with one container per Service port. Each container
claims the port as a `hostPort` on its node, and its entry script
forwards traffic arriving there to the Service's ClusterIP with
iptables rules made inside the pod's network namespace:

```sh
iptables -t filter -I FORWARD -s ${src_range} -p ${DEST_PROTO} --dport ${DEST_PORT} -j ACCEPT
iptables -t nat -I PREROUTING -p ${DEST_PROTO} --dport ${SRC_PORT} -j DNAT --to ${dest_ip}:${DEST_PORT}
iptables -t nat -I POSTROUTING -d ${dest_ip}/32 -p ${DEST_PROTO} -j MASQUERADE
```

The node's addresses then go into the Service's `status.loadBalancer`.

By default the only LoadBalancer Service in k3s is traefik's, the
ingress controller k3s installs with Helm, on ports 80 and 443; hence
the one DaemonSet, `svclb-traefik`, with two containers.

## What we saw

```
kube-system   svclb-traefik-d0689de0-dqwl2   0/2   CrashLoopBackOff   10 (89s ago)   4m52s
```

and in its containers' logs:

```
+ iptables -t filter -I FORWARD -s 0.0.0.0/0 -p TCP --dport 80 -j ACCEPT
modprobe: can't change directory to '/lib/modules': No such file or directory
iptables v1.8.11 (legacy): can't initialize iptables table `filter': Table does not exist (do you need to insmod?)
Perhaps iptables or your kernel needs to be upgraded.
```

The word to notice is **legacy**. Everything else in the guest (the
host's iptables, kube-proxy, flannel, the network policy controller)
uses iptables-nft, `iptables v1.8.11 (nf_tables)`.

## The cause: how klipper-lb picks iptables

The klipper-lb image (`rancher/klipper-lb:v0.4.17` in k3s v1.37.2+k3s1)
carries both iptables backends, and its `/usr/bin/entry` chooses one
when the container starts:

```sh
check_iptables_mode() {
    set +e
    lsmod | grep -qF nf_tables 2> /dev/null
    if [ $? = 0 ]; then
        mode=nft
    else
        mode=legacy
    fi
    set -e
    ...
}
```

It guesses from `lsmod`, which lists *loadable modules*: if
`nf_tables` is a loaded module, the node uses nftables, otherwise it
must be legacy. On most distributions' kernels that works, as netfilter
is built as modules.

Hearth's guest kernel (`make-guest-kernel.sh`) builds everything in, and
the rootfs has no modules:

```
# lsmod
Module                  Size  Used by    Not tainted
# wc -l < /proc/modules
0
```

`nf_tables` is there, but built in, so `lsmod` never lists it. The
script takes legacy mode and points `iptables` at
`xtables-legacy-multi`, which needs the legacy `filter` and `nat`
tables: `CONFIG_IP_NF_FILTER` and `CONFIG_IP_NF_NAT`, under
`CONFIG_NETFILTER_XTABLES_LEGACY`. `guest-kernel.config` leaves those
off on purpose:

```
# --- Netfilter for kube-proxy (iptables mode) and k3s, via iptables-nft.
# The legacy ip_tables/ip6_tables tables (NETFILTER_XTABLES_LEGACY) are
# only needed by iptables-legacy and stay off.
```

The legacy tables don't exist, so the first `iptables` command fails,
the container exits, and the kubelet restarts it, again and again. The
`modprobe` lines are legacy iptables trying to load `iptable_filter`.

## What it broke, and what it didn't

- traefik itself ran fine: its pod was `Running`, and its Service had a
  ClusterIP and NodePorts like any other.
- The Service never got an external address: ServiceLB only fills in
  `status.loadBalancer` once its pods are ready, so `kubectl get svc`
  showed `EXTERNAL-IP <pending>`.
- Nothing else depends on ServiceLB. kube-proxy, cluster DNS and pod
  networking are all separate, and worked once the REJECT target was in
  the kernel ([REJECT.md](REJECT.md)).

## The options

1. **Turn ServiceLB off** (chosen). One line of k3s configuration. With
   it off, a LoadBalancer Service stays `<pending>`; NodePort and
   ClusterIP Services work as before. On a single node reached over a
   tap, a NodePort is what a LoadBalancer would amount to anyway: the
   node's address and a port.

2. **Give the kernel legacy iptables tables** too: add
   `CONFIG_NETFILTER_XTABLES_LEGACY`, `CONFIG_IP_NF_FILTER`,
   `CONFIG_IP_NF_NAT`, `CONFIG_IP_NF_TARGET_MASQUERADE` and their IPv6
   twins. klipper-lb would then work in legacy mode, writing its rules
   to legacy tables while everything else uses nftables. The kernel
   runs both, but a node with two rule sets is harder to debug, and
   upstream Linux is moving away from the legacy tables.

3. **Build nf_tables as a module**, so that `lsmod` lists it: the
   kernel build would install modules into the rootfs, and something
   would have to load nf_tables early in boot. That works, but it adds
   module handling to an image built with none, just to satisfy a
   heuristic.

4. **Another load balancer**, such as MetalLB in L2 mode, answering ARP
   for addresses on the tap's 192.168.100.0/24. It needs more images in
   the rootfs and its own configuration, which is a lot for a one-node
   test cluster.

Turning off ServiceLB costs nothing that this setup uses, needs no
kernel or image changes, and leaves the guest kernel nftables-only. If a
LoadBalancer Service is ever needed here, option 2 is the quickest way
back: rebuild the kernel with the legacy tables and remove `servicelb`
from the `disable` list.

## Why traefik is off too

traefik was ServiceLB's only user. Without ServiceLB, its Service never
gets an address, and the guest only reaches the host through NodePorts
anyway, which any Service can have. It was also the largest add-on:
**99 MB** resident in the measurements in [MEMORY.md](MEMORY.md), against
41–63 MB for metrics-server and 28–54 MB for CoreDNS. A workload that
wants an ingress controller can install one, or k3s's traefik can come
back by removing it from the `disable` list.

## How it was checked

- The failure, its log and the entry script were read from a running
  guest (`k3s ctr images ls` for the image, the script from containerd's
  snapshot of it).
- With `disable: [traefik, servicelb]` (first as a drop-in in
  `/etc/rancher/k3s/config.yaml.d/`, then in `make-k3s-rootfs.sh`'s
  config), k3s removes both: the `svclb-traefik` DaemonSet is gone, a
  `helm-delete-traefik` job uninstalls traefik, and every other
  `kube-system` pod runs or has completed.
- `guests/k3s/run.sh` checks that on a fresh image, and that a NodePort
  Service answers the host.
