# Emularium (github.com/StjepanPoljak/Emularium) in a container, for the
# pods in emularium.yaml: the emularium binary, its boards, and socat for
# the sidecar that makes its VNC port reachable from outside the pod
# (Emularium listens on 127.0.0.1 only). Built from Emularium's committed
# sources, without its target directory:
#
#   mkdir -p build/emularium-src
#   git -C ~/Projects/Emularium archive HEAD | tar -x -C build/emularium-src
#   docker build -t emularium:1 -f guests/k3s/emularium/Dockerfile build/emularium-src
#
# (DOCKER_BUILDKIT=0 without buildx.) Then add it to the k3s image with
# docker save, next to nginx (see guests/k3s/Dockerfile).

FROM rust:1.99-alpine3.22 AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM alpine:3.22
RUN apk add --no-cache socat
COPY --from=build /src/target/release/emularium /usr/local/bin/emularium
COPY --from=build /src/boards /boards
WORKDIR /boards/tetris
CMD ["emularium", "tetris.toml"]
