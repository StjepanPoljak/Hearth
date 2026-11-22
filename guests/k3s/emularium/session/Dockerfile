# An Emularium session: Emularium, its boards and schematic editor, noVNC
# and websockify from Alpine's packages, and the session's own front
# (front.py, page.html) and emulator loop. One image for all the
# containers of a session pod (session.yaml) and for the hub (../hub).
#
# Its build context holds Emularium's committed sources in src/ and this
# directory in session/ and the hub in hub/. From the repository root:
#
#   rm -rf build/emularium-session && mkdir -p build/emularium-session/src
#   git -C ~/Projects/Emularium archive HEAD | tar -x -C build/emularium-session/src
#   cp -r guests/k3s/emularium/session guests/k3s/emularium/hub build/emularium-session/
#   docker build -t emularium-session:1 -f guests/k3s/emularium/session/Dockerfile build/emularium-session
#
# (DOCKER_BUILDKIT=0 without buildx.) Then save it into the k3s image with
# nginx (see guests/k3s/Dockerfile).

FROM rust:1.99-alpine3.22 AS build
WORKDIR /src
COPY src/ .
RUN cargo build --release --locked

FROM alpine:3.22
RUN apk add --no-cache python3 websockify novnc
COPY --from=build /src/target/release/emularium /usr/local/bin/emularium
COPY --from=build /src/boards /opt/emularium/boards
COPY --from=build /src/tools/schematic.py /opt/emularium/tools/schematic.py
COPY --from=build /src/tools/schematic /opt/emularium/tools/schematic
COPY session/front.py session/page.html /opt/session/
COPY session/emulator-loop /usr/local/bin/emulator-loop
COPY hub/hub.py hub/hub.html /opt/hub/
RUN chmod 755 /usr/local/bin/emulator-loop /opt/session/front.py /opt/hub/hub.py && \
    chmod -R go-w /opt
CMD ["python3", "/opt/session/front.py"]
