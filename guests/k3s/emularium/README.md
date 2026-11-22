# Emularium in k3s

[Emularium](https://github.com/StjepanPoljak/Emularium) emulates whole
boards: a PIC16F876 with its firmware, LCDs, keypads, SPI memories. This
directory serves it from the k3s node in Hearth, in two ways:

- **Fixed boards** (`Dockerfile`, `emularium.yaml`): three Tetris boards,
  one per pod, each on its own NodePort for a VNC viewer.
- **Sessions** (`session/`, `hub/`): each user gets their own board in
  the browser. They draw its schematic in Emularium's editor, upload
  firmware for its PIC, run it and play it, then download what they made.

Neither has accounts or passwords: anyone who reaches the node can use
them. They are for a trusted network, such as Hearth's host-only tap.

## Sessions

```
 browser ── :30999 ──► hub (hub/hub.py)                one Deployment
    │                   creates and ends session pods and their Services
    │                   through the Kubernetes API (its own ServiceAccount)
    │
    └──── :3xxxx ─────► session pod (session/session.yaml, made by the hub)
                         front.py      the pod's only port: the page, uploads,
                                       Run, templates, download; passes the
                                       rest through to the two below
                         schematic.py  Emularium's editor, on 127.0.0.1:8001
                         emulator-loop Emularium on /work/board.toml, VNC on
                                       127.0.0.1:5900, restarted by Run
                         websockify    127.0.0.1:6080, VNC over a WebSocket
                                       for noVNC in the page
                         /work         a scratch volume they share: the
                                       board, its layout and firmware.hex
```

Emularium and its editor listen on 127.0.0.1 only, as they have no
password, so a session pod has one server on its address, the front, and
the others behind it, reached over the pod's shared network namespace. The
editor's page uses absolute paths (`/`, `/api/save`), so the editor is at
the root and the session's own pages are under `/session/`.

A user's day with it:

1. `http://192.168.100.2:30999/`, "New session" from a template (Tetris,
   or the SPI RAM test). The session is ready in about 3 s, and the hub
   opens it.
2. The session's page: the schematic editor on the left, the board's
   display (noVNC) and Emularium's output on the right. Keys typed in the
   display go to the board's keypad.
3. Change the board in the editor and Save (Ctrl+S); upload an Intel HEX
   file for the PIC ("Upload firmware", checked before it's used); Run
   restarts the board with both. A board Emularium can't load stops, with
   its error in the output.
4. Download takes the board file, its layout and firmware as a zip. Ending
   the session, from the hub, deletes it all.

The hub ends sessions no page has used for 30 minutes (`IDLE_MINUTES`), or
older than 8 hours (`MAX_HOURS`), and runs at most 4 at once
(`MAX_SESSIONS`): each takes about 0.35 of a core for its emulated PIC,
and the node has 2. It ends only pods and Services labelled as its
sessions, though its Role would let it delete any in the namespace.

## Building

Both images build Emularium from its committed sources. From the
repository root, with an Emularium checkout in `~/Projects/Emularium`:

```sh
rm -rf build/emularium-session && mkdir -p build/emularium-session/src
git -C ~/Projects/Emularium archive HEAD | tar -x -C build/emularium-session/src
cp -r guests/k3s/emularium/session guests/k3s/emularium/hub build/emularium-session/
docker build -t emularium-session:1 -f guests/k3s/emularium/session/Dockerfile build/emularium-session
```

(`DOCKER_BUILDKIT=0` without buildx; `emularium:1`, for the fixed boards,
as `Dockerfile` here says.) The node has no registry to pull from, so the
images go into the k3s image with nginx, which `guests/k3s/Dockerfile`
then builds as usual:

```sh
docker save nginx:1.30.6-alpine emularium:1 emularium-session:1 \
    -o guests/k3s/rootfs/var/lib/rancher/k3s/agent/images/workloads.tar
```

To update an image in a running node instead, put its `docker save` tar
into the guest's `/var/lib/rancher/k3s/agent/images/`: k3s imports what
appears there while it runs (Alpine's k3s has no `k3s ctr`).

## Running

With the k3s image built as above, e.g. into `build/k3s-emularium.img`:

```sh
target/release/hearth --kernel build/linux/vmlinux --disk build/k3s-emularium.img,overlay \
    --cpus 2 --memory 2G --tap tap0 --balloon
```

then, in the guest, `kubectl apply -f -` with `hub/hub.yaml` (or
`session/session.yaml` for one session on port 31000, or `emularium.yaml`
for the fixed boards), and `http://192.168.100.2:30999/` in a browser.

## Tests

`test/session-test.py URL EMULARIUM_DIR` tests one session from outside,
the way its page uses it: a file that isn't Intel HEX, or has a bad
checksum, is refused; other firmware, uploaded, runs instead of Tetris;
a schematic saved through the editor runs after Run (without the keypad's
data-available net, a key no longer starts the game); a board Emularium
can't load is reported stopped, with its error; templates and download
work. It reads the board's display and presses its keys over noVNC's
WebSocket (`test/wsvnc.py`).

`test/hub-test.py http://192.168.100.2:30999 EMULARIUM_DIR` tests the hub,
and runs `session-test.py` on a session it creates: sessions are ready in
about 3 s, a fifth is refused, a key pressed in one session changes only
that one, deleting anything but a session is refused, and ending them
leaves none. Both passed on a fresh boot of the k3s image on 2026-11-21.
Idle sessions were ended by the hub after `IDLE_MINUTES` (set to 1 for
the test) while a polled one stayed; that isn't in the scripts, as it
takes minutes. The page itself was checked by screenshots from headless
Firefox: the editor, the connected display and the output.

## Not done

- Accounts: a session belongs to whoever has its address.
- A PIC assembler: Alpine has no `gputils`, so firmware is assembled
  elsewhere and uploaded as HEX.
- A board's UART on TCP and VCD traces aren't reachable from the page;
  a UART without `tcp` prints to the output.
- Sessions are lost when their pod goes; Download is the way to keep one.
