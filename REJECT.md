# Why one missing iptables target left k3s with no Service routing

On the first k3s boots in Hearth, the cluster looked up: the node was
`Ready`, and the API server answered `kubectl` on the host. But no pod
could reach a Service. CoreDNS never became ready, and the local-path
provisioner and metrics-server crash-looped. The cause was two kernel
options missing from `guest-kernel.config`:

```
CONFIG_IP_NF_TARGET_REJECT=y
CONFIG_IP6_NF_TARGET_REJECT=y
```

Commit `00060c5` adds them. This file explains how a missing **REJECT**
target, which only matters for Services with *no* endpoints, stopped
*every* Service from working.

## What we saw

The pods' logs all said the same thing: they could not reach the API
server through its Service address, `10.43.0.1:443`.

```
local-path-provisioner: level=fatal msg="Error starting daemon: invalid empty
  flag helper-pod-file and it also does not exist at ConfigMap
  kube-system/local-path-config with err: Get "https://10.43.0.1:443/api/v1/na...
```

The k3s log (`/var/log/k3s.log` in the guest) had the reason, every 30
seconds, 25 times over:

```
E1113 21:51:13.268746  804 proxier.go:1390] "Failed to execute iptables-restore" err=<
    exit status 4: Warning: Extension REJECT revision 0 not supported, missing kernel module?
    iptables-restore v1.8.11 (nf_tables):
    line 9: RULE_APPEND failed (No such file or directory): rule in chain KUBE-SERVICES
    line 10: RULE_APPEND failed (No such file or directory): rule in chain KUBE-SERVICES
    line 11: RULE_APPEND failed (No such file or directory): rule in chain KUBE-SERVICES
    line 12: RULE_APPEND failed (No such file or directory): rule in chain KUBE-SERVICES
 > ipFamily="IPv4"
I1113 21:51:13.268930  804 proxier.go:660] "Sync failed" ipFamily="IPv4" retryingTime="30s"
```

In the guest, the `nat` table's `KUBE-SERVICES` chain, where every
Service's DNAT rules should hang, was empty:

```
# iptables -t nat -S KUBE-SERVICES
-N KUBE-SERVICES
```

## How kube-proxy routes Services

k3s runs kube-proxy inside the `k3s server` process, in its default
iptables mode. A Service is a virtual IP (the ClusterIP, here from
10.43.0.0/16) that no interface has. kube-proxy makes it work with
netfilter rules:

- in the **nat** table, a rule in `KUBE-SERVICES` for each Service port
  jumps to a `KUBE-SVC-*` chain, which picks an endpoint and DNATs the
  packet to a pod (or, for `kubernetes`, to the API server at
  192.168.100.2:6443);
- in the **filter** table, a rule in `KUBE-SERVICES` for each Service
  port *with no endpoints* rejects the packet, so a client gets an ICMP
  port-unreachable at once instead of waiting for a timeout.

The second kind is the only place kube-proxy uses REJECT. In
`pkg/proxy/iptables/proxier.go` (Kubernetes v1.37.2, which k3s
v1.37.2+k3s1 embeds):

```go
if !hasEndpoints {
    // The service has no endpoints at all; hasInternalEndpoints and
    // hasExternalEndpoints will also be false, and we will not
    // generate any chains in the "nat" table for the service; only
    // rules in the "filter" table rejecting incoming packets for
    // the service's IPs.
    internalTrafficFilterTarget = "REJECT"
    internalTrafficFilterComment = fmt.Sprintf(`"%s has no endpoints"`, svcPortNameString)
```

The rules it wrote looked like this, in the dump of a later failed sync:

```
-A KUBE-SERVICES -d 10.43.0.10/32 -p udp -m comment --comment "kube-system/kube-dns:dns has no endpoints" -m udp --dport 53 -j REJECT --reject-with icmp-port-unreachable
-A KUBE-SERVICES -d 10.43.0.10/32 -p tcp -m comment --comment "kube-system/kube-dns:dns-tcp has no endpoints" -m tcp --dport 53 -j REJECT --reject-with icmp-port-unreachable
-A KUBE-SERVICES -d 10.43.222.111/32 -p tcp -m comment --comment "kube-system/metrics-server:https has no endpoints" -m tcp --dport 443 -j REJECT --reject-with icmp-port-unreachable
```

## Why the kernel could not take a REJECT rule

The guest is Alpine, whose `iptables` is iptables-nft (v1.8.11): it
speaks the iptables language, but loads the rules into the kernel as
nftables. It does this in one of two ways for each match or target:

1. **Translated** into native nftables expressions, where nftables has
   an equivalent and iptables-nft knows the translation.
2. **Through the compatibility layer**, `NFT_COMPAT`: the rule holds an
   nftables expression that calls the kernel's *xtables* module for
   that match or target, the same code legacy iptables would use.

For `-j REJECT`, iptables-nft uses the second way. Before adding the
rule, it asks the kernel which revisions of the xtables target "REJECT"
it knows, which is where the warning comes from:

```
Warning: Extension REJECT revision 0 not supported, missing kernel module?
```

The xtables REJECT target for IPv4 is `ipt_REJECT`, built by
`CONFIG_IP_NF_TARGET_REJECT` (`ip6t_REJECT` and
`CONFIG_IP6_NF_TARGET_REJECT` for IPv6). Neither was in the kernel.
The kernel could not load a module either, as the guest kernel has
everything built in and the rootfs has no modules. So when the rule went
in, the kernel had no target of that name, and refused it with ENOENT:

```
line 9: RULE_APPEND failed (No such file or directory): rule in chain KUBE-SERVICES
```

`guest-kernel.config` did have `CONFIG_NFT_REJECT`, nftables' *own*
reject expression. That is what `nft add rule ... reject` uses, but not
what iptables-nft asks for when it is handed `-j REJECT`, so it did not
help: native nftables tools use one, the compatibility path the other.
Kconfig allows the xtables target without legacy iptables
(`depends on IP_NF_FILTER || NFT_COMPAT`).

## Why one bad rule broke every Service

kube-proxy does not add rules one by one. Each sync builds the complete
rule set for both tables and loads it with a single `iptables-restore`,
`filter` first, then `nat`:

```go
proxier.iptablesData.WriteString("*filter\n")
proxier.iptablesData.Write(proxier.filterChains.Bytes())
proxier.iptablesData.Write(proxier.filterRules.Bytes())
proxier.iptablesData.WriteString("COMMIT\n")
proxier.iptablesData.WriteString("*nat\n")
proxier.iptablesData.Write(proxier.natChains.Bytes())
proxier.iptablesData.Write(proxier.natRules.Bytes())
proxier.iptablesData.WriteString("COMMIT\n")
...
err := proxier.iptables.RestoreAll(proxier.iptablesData.Bytes(), utiliptables.NoFlushTables, utiliptables.RestoreCounters)
```

iptables-nft-restore sends the whole input to the kernel as one nftables
transaction, and the kernel applies all of it or none of it. One REJECT
rule in the `filter` section failed, so the transaction was rolled back
with the `nat` section in it: no DNAT rule for any Service was ever
loaded, the `kubernetes` Service's included. kube-proxy logged
`Sync failed`, waited 30 seconds and tried again, with the same input.

## Why it never recovered on its own

The rules that failed exist only for Services without endpoints, and
that is exactly the state the failure kept the cluster in:

1. k3s starts; CoreDNS, metrics-server and the local-path provisioner
   start, and none is ready yet, so `kube-dns` and `metrics-server` have
   no ready endpoints.
2. kube-proxy writes REJECT rules for them. The restore fails, and with
   it every DNAT rule.
3. The pods reach for the API server at 10.43.0.1, which nothing
   translates any more. Their connections time out; they fail their
   readiness checks, or exit and restart.
4. So their Services still have no ready endpoints, and the next sync
   writes the same REJECT rules again.

A cluster whose Services all had ready endpoints would have had no
REJECT rules, and would have synced. A cluster starting from nothing
always has Services without endpoints for a while, so it could never
get there.

## Why the cluster still looked healthy

Nothing on the node's own control path goes through a ClusterIP:
`kubectl` on the host talks to 192.168.100.2:6443, the kubelet and the
controllers inside `k3s server` use 127.0.0.1:6443 and :6444, and the
node's `Ready` condition comes from the kubelet, not from pods. So the
first test run passed its first five checks (console, kubeconfig, API
server, node Ready, CPU count) before it was killed for low memory; the
checks on system pods and Services would have shown the problem.
Measuring memory on the next boot found pods in `CrashLoopBackOff`, and
their logs led here.

## The fix, and how it was checked

`guest-kernel.config` now has, next to `CONFIG_IP_NF_IPTABLES`:

```
# kube-proxy rejects traffic to Services without endpoints with -j REJECT,
# which iptables-nft runs through NFT_COMPAT; NFT_REJECT is not that.
CONFIG_IP_NF_TARGET_REJECT=y
CONFIG_IP6_NF_TARGET_REJECT=y
```

With the rebuilt kernel, `iptables -A HT -j REJECT` succeeds, k3s's log
has no `Failed to execute iptables-restore`, and CoreDNS, metrics-server
and the local-path provisioner run. `guests/k3s/run.sh` checks Services
from inside: a pod reaches nginx through its ClusterIP by DNS name,
which needs both the DNAT rules and CoreDNS's Service.

## The same mistake, twice more: NFLOG and limit

Probing the targets by hand in the guest found another one:

```
ok   -j REJECT
FAIL -j NFLOG --nflog-group 100: iptables v1.8.11 (nf_tables):  RULE_APPEND failed (No such file or directory): rule in chain HT
ok   -j MARK --set-xmark 0x10000/0x10000
FAIL -j LOG: iptables v1.8.11 (nf_tables):  RULE_APPEND failed (No such file or directory): rule in chain HT
```

k3s's network policy controller (kube-router's, embedded in k3s) puts a
`-j NFLOG` rule in every pod's firewall chain, to log the packets a
NetworkPolicy drops. That failed too, and the controller loads its rules
in one `iptables-restore` as well, so each of its syncs was aborted, 32
times in that log:

```
E1114 19:41:54.478657  687 npc_iptables.go:221] Aborting sync. Failed to run iptables-restore: failed to call iptables-restore: exit status 4 (iptables-restore v1.8.11 (nf_tables):
line 68: RULE_APPEND failed (No such file or directory): rule in chain KUBE-POD-FW-DPKHTC6NZ6WOKN4J
```

Unlike kube-proxy's failure, this one hides: with no rules loaded, pods
can reach each other as if there were no policy, so nothing visibly
breaks, and NetworkPolicies are silently not enforced.

Here it is the other way round from REJECT. `CONFIG_NETFILTER_XT_TARGET_NFLOG`
(the xtables target) was built in, but iptables-nft *translates* NFLOG
into nftables' own `log` expression, which needs `CONFIG_NFT_LOG`, and
that was not set.

Adding `CONFIG_NFT_LOG` was not enough. The full test still found
NetworkPolicies unenforced, and the controller still aborting, now on
two rules per sync instead of one. Its rule is

```
-A KUBE-POD-FW-... -m comment --comment "... log-drop" -m mark ! --mark 0x10000/0x10000 -j NFLOG --nflog-group 100 -m limit --limit 10/minute --limit-burst 10
```

and with the new kernel, plain `-j NFLOG --nflog-group 100` went in, but
the same rule with `-m limit` did not. iptables-nft translates the
`limit` match too, into nftables' `limit` expression: `CONFIG_NFT_LIMIT`,
also unset, while `CONFIG_NETFILTER_XT_MATCH_LIMIT`, the xtables match,
was built in. `guest-kernel.config` now has both nftables options:

```
# k3s's network policy controller logs dropped packets with -j NFLOG, at
# most 10 a minute with -m limit; iptables-nft turns both into nftables'
# own log and limit expressions (REJECT.md).
CONFIG_NFT_LOG=y
CONFIG_NFT_LIMIT=y
```

`guests/k3s/run.sh` now checks enforcement: it applies a NetworkPolicy
denying all ingress to nginx, checks that another pod can no longer
reach it, deletes the policy, and checks that the pod gets through
again.

`-j LOG` fails for a different reason: `CONFIG_NETFILTER_XT_TARGET_LOG`
and `CONFIG_NF_LOG_SYSLOG` are modules (`=m`), and the rootfs has no
modules. `-m pkttype` fails too. Nothing in k3s's rules here uses
either, so they stay as they are.

## Checking a kernel for this

Neither failure shows until a rule using the target is loaded, and
kube-proxy only writes REJECT rules while some Service has no
endpoints. To check a guest kernel ahead of time, add one rule per match
and target that k3s's components use to scratch chains, as root in the
guest. This is the list that found the three:

```sh
iptables -N HT; iptables -t nat -N HT
for r in '-j REJECT' '-j DROP' '-j NFLOG --nflog-group 100' \
         '-m limit --limit 10/minute --limit-burst 10 -j RETURN' \
         '-j MARK --set-xmark 0x10000/0x10000' '-m mark --mark 0x4000/0x4000 -j RETURN' \
         '-m conntrack --ctstate RELATED,ESTABLISHED -j RETURN' '-m comment --comment x -j RETURN' \
         '-m addrtype --dst-type LOCAL -j RETURN' '-m physdev --physdev-is-bridged -j RETURN' \
         '-p tcp -m multiport --dports 30000:32767 -j RETURN' \
         '-m statistic --mode random --probability 0.5 -j RETURN' \
         '-p icmp -m icmp --icmp-type 8 -j RETURN'; do
    iptables -A HT $r > /dev/null 2>&1 && echo "ok   $r" || echo "FAIL $r"
done
for r in '-j MASQUERADE --random-fully' '-p tcp -j DNAT --to-destination 10.0.0.1:80' \
         '-j SNAT --to-source 10.0.0.1'; do
    iptables -t nat -A HT $r > /dev/null 2>&1 && echo "ok   nat $r" || echo "FAIL nat $r"
done
iptables -F HT; iptables -X HT; iptables -t nat -F HT; iptables -t nat -X HT
```

Each `FAIL` is a missing kernel option. The error message alone
doesn't say whether iptables-nft translated the match or target or
went through `NFT_COMPAT`, so check both its `CONFIG_NETFILTER_XT_*` (or
`CONFIG_IP_NF_*`) option and its `CONFIG_NFT_*` one. Of the three here,
REJECT needed the xtables target; NFLOG and limit needed the nftables
expressions.
