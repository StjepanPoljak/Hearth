#!/bin/sh
#
# Build an Alpine Linux ext4 rootfs image for running containers (k3s) in
# the guest: OpenRC, cgroup v2, iptables-nft, a root shell on ttyS0 and a
# static eth0 address matching the make-tap.sh defaults.
#
# Usage: ./make-alpine-rootfs.sh [output.img] [size]
#   PACKAGES  more Alpine packages to install
#   OVERLAY   a directory whose contents are copied over the finished
#             root, owned by root (make-k3s-rootfs.sh adds k3s this way)
#
# Needs docker and network access (to pull alpine and its packages), and
# mke2fs >= 1.47.1, which can populate an image from a tarball. No root
# privileges are needed: file ownership comes from the tarball.

set -e

OUT=${1:-build/alpine.img}
SIZE=${2:-8G}
ALPINE=${ALPINE:-alpine:3.22}
TAR=${OUT%.img}.tar

mkdir -p "$(dirname "$OUT")"

set --
if [ -n "$OVERLAY" ]; then
    set -- -v "$(realpath "$OVERLAY")":/overlay:ro
fi

# Docker bind-mounts /etc/hosts, /etc/hostname and /etc/resolv.conf into the
# container, so the guest root is built in /rootfs instead of / and streamed
# out as a tarball.
docker run --rm -i -e PACKAGES="$PACKAGES" "$@" "$ALPINE" sh -s > "$TAR" <<'EOF'
set -e
exec 3>&1 1>&2

R=/rootfs
mkdir -p $R/etc/apk
cp -r /etc/apk/keys /etc/apk/repositories $R/etc/apk/
apk add --root $R --initdb --no-cache \
    alpine-base ca-certificates e2fsprogs iptables ip6tables $PACKAGES

cat > $R/etc/inittab <<'INITTAB'
::sysinit:/sbin/openrc sysinit
::sysinit:/sbin/openrc boot
::wait:/sbin/openrc default
ttyS0::respawn:/bin/login -f root
::ctrlaltdel:/sbin/reboot
::shutdown:/sbin/openrc shutdown
INITTAB
echo ttyS0 >> $R/etc/securetty

echo "/dev/vda / ext4 rw,relatime 0 1" > $R/etc/fstab
echo k8s-node > $R/etc/hostname
cat > $R/etc/hosts <<'HOSTS'
127.0.0.1 localhost
::1       localhost
127.0.1.1 k8s-node
HOSTS
cat > $R/etc/network/interfaces <<'IFACES'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 192.168.100.2/24
    gateway 192.168.100.1
IFACES
: > $R/etc/resolv.conf

# kubelet and k3s want cgroup v2 only, which the cgroups service mounts.
sed -i 's/^#\?rc_cgroup_mode=.*/rc_cgroup_mode="unified"/' $R/etc/rc.conf

# rc-update needs to run inside the root, so link the services directly.
add_service() {
    mkdir -p $R/etc/runlevels/$1
    ln -sf /etc/init.d/$2 $R/etc/runlevels/$1/$2
}
for s in devfs dmesg mdev hwdrivers; do add_service sysinit $s; done
for s in fsck root localmount hostname bootmisc sysctl seedrng cgroups networking; do
    add_service boot $s
done
for s in mount-ro killprocs savecache; do add_service shutdown $s; done

# Without -p, cp gives what it creates to root; symlinks stay symlinks.
if [ -d /overlay ]; then
    cp -R /overlay/. $R/
fi

tar -C $R --numeric-owner -cf - . >&3
EOF

rm -f "$OUT"
mkfs.ext4 -q -F -L rootfs -d "$TAR" "$OUT" "$SIZE"
rm -f "$TAR"

echo "Created $OUT ($SIZE, sparse)."
