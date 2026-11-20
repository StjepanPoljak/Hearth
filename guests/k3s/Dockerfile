# Alpine with Alpine's k3s package, as the root filesystem of a one-node
# Kubernetes cluster in Hearth. Everything the cluster runs is in it:
# k3s's air-gap images, and the workload images in workloads.tar.
# rootfs/ holds the files the image gets as they are, at their paths.
#
# From the repository root:
#
#   docker save nginx:1.30.6-alpine \
#       -o guests/k3s/rootfs/var/lib/rancher/k3s/agent/images/workloads.tar
#   docker build -t hearth-k3s guests/k3s     # DOCKER_BUILDKIT=0 without buildx
#   C=$(docker create hearth-k3s)
#   docker export $C > build/k3s.tar && docker rm $C
#   # docker export replaces /etc/hosts, /etc/hostname and /etc/resolv.conf
#   # with empty files of its own and adds /.dockerenv: put ours back.
#   tar --delete -f build/k3s.tar .dockerenv etc/hosts etc/hostname etc/resolv.conf
#   tar -rf build/k3s.tar --owner=0 --group=0 --mode=644 -C guests/k3s/rootfs \
#       etc/hosts etc/hostname etc/resolv.conf
#   mkfs.ext4 -q -F -L rootfs -d build/k3s.tar build/k3s.img 8G
#
# Boot it with a kernel from make-guest-kernel.sh, on the make-tap.sh tap:
#   hearth --kernel build/linux/vmlinux --disk build/k3s.img \
#          --cpus 2 --memory 2G --tap tap0 --balloon

FROM alpine:3.22

# k3s v1.33.1+k3s1, and the dbus service its OpenRC service needs.
RUN apk add --no-cache alpine-base ca-certificates e2fsprogs iptables ip6tables \
        k3s=1.33.1.1-r5 k3s-openrc=1.33.1.1-r5 dbus-openrc

# k3s's images (CoreDNS, local-path provisioner, metrics-server, pause) for
# the same version, which k3s imports when it starts.
ARG K3S_IMAGES=https://github.com/k3s-io/k3s/releases/download/v1.33.1%2Bk3s1/k3s-airgap-images-amd64.tar.zst
ARG K3S_IMAGES_SHA256=5fd0e18b7cd7457773d30e86270a4e1caed66d2e5c1380e65b3d0375227d241c
RUN mkdir -p /var/lib/rancher/k3s/agent/images && cd /var/lib/rancher/k3s/agent/images && \
    wget -q -O k3s-airgap-images-amd64.tar.zst "$K3S_IMAGES" && \
    echo "$K3S_IMAGES_SHA256  k3s-airgap-images-amd64.tar.zst" | sha256sum -c -

# The files in rootfs/, at the same paths: the node and the API server on
# eth0, without traefik or ServiceLB (see config.yaml); a root shell on
# ttyS0, Hearth's console; the disk is /dev/vda; eth0 on make-tap.sh's
# network; the workload images.
COPY rootfs/ /

# COPY keeps the modes rootfs/ has, which depend on the umask of whoever
# checked it out; a root login on the console; cgroup v2 only, as k3s wants.
RUN cd / && chmod go-w etc var var/lib && \
    chmod -R go-w etc/fstab etc/hosts etc/hostname etc/inittab etc/resolv.conf \
        etc/network etc/rancher var/lib/rancher && \
    echo ttyS0 >> /etc/securetty && \
    sed -i 's/^#\?rc_cgroup_mode=.*/rc_cgroup_mode="unified"/' /etc/rc.conf

RUN for s in devfs dmesg mdev hwdrivers; do rc-update add $s sysinit; done && \
    for s in fsck root localmount hostname bootmisc sysctl seedrng cgroups networking; do \
        rc-update add $s boot; done && \
    for s in mount-ro killprocs savecache; do rc-update add $s shutdown; done && \
    rc-update add k3s default
