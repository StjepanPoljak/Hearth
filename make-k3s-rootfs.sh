#!/bin/sh
#
# Build an Alpine rootfs (make-alpine-rootfs.sh) with k3s installed, for a
# single-node Kubernetes cluster in Hearth with no route to a registry:
#
# - the k3s binary and its air-gap images, from the k3s release, checked
#   against the release's sha256 sums; k3s imports the images from
#   /var/lib/rancher/k3s/agent/images when it starts;
# - the images in IMAGES, from the host's docker, saved next to them, for
#   workloads;
# - an OpenRC service starting the k3s server at boot, with its log in
#   /var/log/k3s.log, and /etc/rancher/k3s/config.yaml putting the node
#   and the API server on eth0's address, without k3s's traefik ingress
#   and ServiceLB load balancer (see SERVICELB.md).
#
# Usage: ./make-k3s-rootfs.sh [output.img] [size]
#   K3S_VERSION  the k3s release [default: v1.37.2+k3s1]
#   IMAGES       images to add, pulled on the host if missing
#                [default: nginx:1.30.6-alpine, which guests/k3s/run.sh runs]
#
# It also downloads kubectl for the host, of the same Kubernetes version,
# checked against its sha256 from dl.k8s.io. Downloads are kept in
# build/k3s/<version>/ for later runs. Needs what make-alpine-rootfs.sh
# needs, and curl. Boot the image with a kernel from
# make-guest-kernel.sh, at least 2 vcpus and 2G, on the make-tap.sh tap:
#   hearth --kernel build/linux/vmlinux --disk build/k3s.img \
#          --cpus 2 --memory 2G --tap tap0

set -e

OUT=${1:-build/k3s.img}
SIZE=${2:-8G}
K3S_VERSION=${K3S_VERSION:-v1.37.2+k3s1}
IMAGES=${IMAGES:-nginx:1.30.6-alpine}
# The node's address, from make-alpine-rootfs.sh's /etc/network/interfaces.
NODE_IP=192.168.100.2

DIR=$(dirname "$0")
DL=build/k3s/$K3S_VERSION
URL=https://github.com/k3s-io/k3s/releases/download/$(echo "$K3S_VERSION" | sed 's/+/%2B/')

mkdir -p "$DL"
for f in sha256sum-amd64.txt k3s k3s-airgap-images-amd64.tar.zst; do
    if [ ! -s "$DL/$f" ]; then
        echo "Downloading $f ($K3S_VERSION)..."
        curl -fL --retry 3 -o "$DL/$f.part" "$URL/$f"
        mv "$DL/$f.part" "$DL/$f"
    fi
done
(cd "$DL" && grep -E ' (k3s|k3s-airgap-images-amd64\.tar\.zst)$' sha256sum-amd64.txt | sha256sum -c -)

# v1.37.2+k3s1 is Kubernetes v1.37.2.
KUBE_VERSION=${K3S_VERSION%%+*}
if [ ! -x "$DL/kubectl" ]; then
    echo "Downloading kubectl $KUBE_VERSION..."
    curl -fL --retry 3 -o "$DL/kubectl.part" "https://dl.k8s.io/release/$KUBE_VERSION/bin/linux/amd64/kubectl"
    echo "$(curl -fsSL "https://dl.k8s.io/release/$KUBE_VERSION/bin/linux/amd64/kubectl.sha256")  $DL/kubectl.part" | sha256sum -c -
    chmod 755 "$DL/kubectl.part"
    mv "$DL/kubectl.part" "$DL/kubectl"
fi

OVERLAY=$(mktemp -d build/k3s-overlay.XXXXXX)
trap 'rm -rf "$OVERLAY"' EXIT

mkdir -p "$OVERLAY/usr/local/bin" "$OVERLAY/var/lib/rancher/k3s/agent/images" \
         "$OVERLAY/etc/rancher/k3s" "$OVERLAY/etc/init.d" "$OVERLAY/etc/runlevels/default"
install -m 755 "$DL/k3s" "$OVERLAY/usr/local/bin/k3s"
for tool in kubectl crictl ctr; do
    ln -s k3s "$OVERLAY/usr/local/bin/$tool"
done
cp "$DL/k3s-airgap-images-amd64.tar.zst" "$OVERLAY/var/lib/rancher/k3s/agent/images/"

if [ -n "$IMAGES" ]; then
    for image in $IMAGES; do
        docker image inspect "$image" > /dev/null 2>&1 || docker pull "$image"
    done
    echo "Saving $IMAGES..."
    # shellcheck disable=SC2086
    docker save $IMAGES -o "$OVERLAY/var/lib/rancher/k3s/agent/images/workloads.tar"
fi

cat > "$OVERLAY/etc/rancher/k3s/config.yaml" <<YAML
# Written by make-k3s-rootfs.sh: the node and the API server on eth0.
node-ip: $NODE_IP
advertise-address: $NODE_IP
tls-san:
  - $NODE_IP
# ServiceLB runs iptables-legacy in its pods, and the guest kernel has
# only nftables (SERVICELB.md); traefik's Service is its LoadBalancer.
# NodePort Services reach the node from the host as they are.
disable:
  - traefik
  - servicelb
YAML

# As the service k3s's install.sh writes for OpenRC.
cat > "$OVERLAY/etc/init.d/k3s" <<'SERVICE'
#!/sbin/openrc-run

description="k3s Kubernetes server"

depend() {
    need net
    want cgroups
}

start_pre() {
    rm -f /tmp/k3s.*
}

supervisor=supervise-daemon
name=k3s
command=/usr/local/bin/k3s
command_args="server"
output_log=/var/log/k3s.log
error_log=/var/log/k3s.log
pidfile=/var/run/k3s.pid
respawn_delay=5
respawn_max=0

set -o allexport
if [ -f /etc/environment ]; then . /etc/environment; fi
if [ -f /etc/rancher/k3s/k3s.env ]; then . /etc/rancher/k3s/k3s.env; fi
set +o allexport
SERVICE
chmod 755 "$OVERLAY/etc/init.d/k3s"
ln -s /etc/init.d/k3s "$OVERLAY/etc/runlevels/default/k3s"

OVERLAY=$OVERLAY "$DIR/make-alpine-rootfs.sh" "$OUT" "$SIZE"
