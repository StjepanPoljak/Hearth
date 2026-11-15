#!/bin/sh
# Checks Kubernetes in Hearth: boots a make-k3s-rootfs.sh image with
# several vcpus on the tap, takes the cluster's kubeconfig over the serial
# console, and from the host, with kubectl, checks that
# - the node is Ready, with the vcpus Hearth gave it;
# - CoreDNS, the metrics server and the local-path provisioner roll out,
#   and every kube-system pod runs or has completed: all of k3s's images
#   came from the rootfs, as the guest has no registry to pull from;
# - workload.yaml's nginx Deployment rolls out, its NodePort Service
#   answers the host, and a pod reaches the Service by its cluster DNS
#   name;
# - a NetworkPolicy denying ingress to nginx stops another pod reaching
#   it, and deleting it lets the pod through again;
# - a Job writes to a PersistentVolumeClaim on local-path storage;
# - the metrics server reports the node's usage;
# - with --balloon, the RAM the guest frees goes back to the host: after
#   the guest drops its page cache, Hearth's resident memory shrinks;
# - the guest powers off when asked, which stops Hearth.
# It prints an ok: or FAIL: line per check, and exits 1 if any failed.
#
# Usage: guests/k3s/run.sh KERNEL DISK
#   KERNEL from make-guest-kernel.sh, DISK from make-k3s-rootfs.sh. The
#   cluster's state stays on DISK, so a fresh copy gives a fresh cluster;
#   the first boot takes longest, as k3s imports its images.
#   Needs /dev/kvm, curl, and the tap from `sudo ./make-tap.sh up`.
#   HEARTH [target/release/hearth], CPUS [2], MEMORY [2G], TAP [tap0],
#   KUBECTL [the kubectl make-k3s-rootfs.sh downloaded].
#   KEEP=1 leaves the VM and the workload running at the end, with the
#   console on build/k3s/console.sock and the kubeconfig in
#   build/k3s/kubeconfig.

set -e
DIR=$(dirname "$0")
TOP=$DIR/../..
KERNEL=${1:?usage: $0 KERNEL DISK}
DISK=${2:?usage: $0 KERNEL DISK}
HEARTH=${HEARTH:-$TOP/target/release/hearth}
CPUS=${CPUS:-2}
MEMORY=${MEMORY:-2G}
TAP=${TAP:-tap0}
NODE_IP=192.168.100.2
if [ -z "$KUBECTL" ]; then
    KUBECTL=$(ls -d "$TOP"/build/k3s/v*/kubectl 2>/dev/null | tail -n 1)
    KUBECTL=${KUBECTL:?no KUBECTL, and no kubectl from make-k3s-rootfs.sh in build/k3s}
fi

if [ -n "$KEEP" ]; then
    STATE=$TOP/build/k3s
    mkdir -p "$STATE"
else
    STATE=$(mktemp -d)
    # Under set -e, a failing kill would end the trap before the rm,
    # and make the script fail; Hearth has usually stopped by then.
    trap 'kill $PID 2>/dev/null || true; rm -rf "$STATE"' EXIT
fi
SOCK=$STATE/console.sock
export KUBECONFIG=$STATE/kubeconfig

"$HEARTH" --name k3s --kernel "$KERNEL" --disk "$DISK" --cpus "$CPUS" --memory "$MEMORY" \
    --tap "$TAP" --balloon --serial "unix:$SOCK" --append quiet < /dev/null > "$STATE/hearth.out" 2>&1 &
PID=$!

FAIL=0
ok() { echo "ok:   $1"; }
fail() { echo "FAIL: $1"; FAIL=1; }
check() {
    msg=$1; shift
    if "$@" > "$STATE/check.out" 2>&1; then ok "$msg"; else fail "$msg"; sed 's/^/      /' "$STATE/check.out" | tail -n 20; fi
}
# retry SECONDS COMMAND...: until COMMAND succeeds, or SECONDS pass.
retry() {
    end=$(($(date +%s) + $1)); shift
    until "$@"; do
        [ "$(date +%s)" -lt "$end" ] || return 1
        sleep 5
    done
}
console() { python3 -I "$DIR/console.py" "$SOCK" "$@"; }
kubectl() { $KUBECTL "$@"; }

# Kernel messages would land in the middle of what the console returns.
check "the guest's shell answers on the console" console "dmesg -n 1" 300

# Checked against the guest's sha256sum: the console must not have lost
# or changed a byte of it.
get_kubeconfig() {
    sum=$(console "sha256sum /etc/rancher/k3s/k3s.yaml" 30) &&
        console "cat /etc/rancher/k3s/k3s.yaml" 30 > "$KUBECONFIG.tmp" &&
        [ "$(sha256sum < "$KUBECONFIG.tmp" | cut -d' ' -f1)" = "${sum%% *}" ] &&
        sed "s#https://127.0.0.1:6443#https://$NODE_IP:6443#" "$KUBECONFIG.tmp" > "$KUBECONFIG"
}
check "k3s wrote its kubeconfig" retry 600 get_kubeconfig

check "the API server answers the host on $NODE_IP:6443" retry 300 kubectl get --raw /readyz
check "the node is Ready" retry 300 kubectl wait node --all --for=condition=Ready --timeout=10s
cpus_match() { [ "$(kubectl get nodes -o 'jsonpath={.items[0].status.capacity.cpu}')" = "$CPUS" ]; }
check "the node has the $CPUS vcpus Hearth gave it" cpus_match

for d in coredns local-path-provisioner metrics-server; do
    check "$d rolls out" retry 600 kubectl -n kube-system rollout status deployment/$d --timeout=10s
done
system_pods_settled() {
    kubectl -n kube-system get pods --no-headers > "$STATE/pods" &&
        [ -s "$STATE/pods" ] && ! awk '$3 != "Running" && $3 != "Completed" { bad = 1; print } END { exit !bad }' "$STATE/pods"
}
check "every kube-system pod runs or has completed, with images from the rootfs" retry 600 system_pods_settled

check "workload.yaml applies" kubectl apply -f "$DIR/workload.yaml"
check "the web Deployment rolls out, 2 replicas" kubectl rollout status deployment/web --timeout=300s
nodeport() { curl -fsS --max-time 5 "http://$NODE_IP:30080/" | grep -q "Welcome to nginx"; }
check "its NodePort Service answers the host on $NODE_IP:30080" retry 60 nodeport
check "the client pod runs" kubectl wait pod/client --for=condition=Ready --timeout=120s
by_dns() { kubectl exec client -- wget -qO- -T 5 http://web.default.svc.cluster.local/ | grep -q "Welcome to nginx"; }
check "a pod reaches the Service by its cluster DNS name" retry 60 by_dns
check "network-policy.yaml applies" kubectl apply -f "$DIR/network-policy.yaml"
blocked() { ! kubectl exec client -- wget -qO- -T 3 http://web.default.svc.cluster.local/ > /dev/null 2>&1; }
check "the NetworkPolicy stops the pod reaching the Service" retry 60 blocked
check "network-policy.yaml is deleted" kubectl delete -f "$DIR/network-policy.yaml"
check "without it, the pod reaches the Service again" retry 60 by_dns
check "the Job writing to the volume completes" kubectl wait job/writer --for=condition=Complete --timeout=300s
pvc_bound() { [ "$(kubectl get pvc data -o 'jsonpath={.status.phase}')" = Bound ]; }
check "its PersistentVolumeClaim is bound on local-path" pvc_bound
volume_written() { kubectl logs job/writer | grep -qx written-to-a-volume; }
check "the Job read back what it wrote to the volume" volume_written
check "the metrics server reports the node's usage" retry 300 kubectl top node

# Free page reporting hands freed blocks back after a couple of seconds.
hearth_rss_kb() { awk '/^VmRSS/ { print $2 }' /proc/$PID/status; }
BEFORE=$(hearth_rss_kb)
console "sync; echo 3 > /proc/sys/vm/drop_caches" 60 > /dev/null || true
rss_shrank() { [ "$(hearth_rss_kb)" -lt $((BEFORE - 100 * 1024)) ]; }
check "dropping the guest's page cache gives at least 100M back to the host" retry 60 rss_shrank
echo "      Hearth's resident memory: $((BEFORE / 1024))M before, $(($(hearth_rss_kb) / 1024))M after"

if [ -n "$KEEP" ]; then
    echo "Left running (pid $PID): KUBECONFIG=$KUBECONFIG $KUBECTL get pods -A"
    echo "Console: socat -,raw,echo=0,escape=0x1d UNIX-CONNECT:$SOCK"
    exit $FAIL
fi

check "the workload is deleted" kubectl delete -f "$DIR/workload.yaml" --wait=true --timeout=120s --grace-period=5
# The console goes away with the guest, so its answer doesn't count.
console "poweroff" 30 > /dev/null 2>&1 || true
powered_off() { ! kill -0 $PID 2>/dev/null && grep -q "Guest powered off" "$STATE/hearth.out"; }
check "the guest powers off, and Hearth stops" retry 180 powered_off

if [ $FAIL -ne 0 ]; then
    echo "--- Hearth:"; tail -n 20 "$STATE/hearth.out"
fi
exit $FAIL
