#!/usr/bin/env python3
"""The front of an Emularium session pod: the only server on the pod's
address, as Emularium and its schematic editor listen on 127.0.0.1 only.

The pod's containers share /work: the editor edits /work/board.toml, the
emulator loop (emulator-loop) runs Emularium on it, and this server ties
them together for the browser:

  /session/            the page: the editor beside the board's display
  /session/status      JSON: the template, whether Emularium runs, its log,
                       and the debugger's status while it runs
  /session/parts       JSON: the saved board's parts, and the file each
                       takes (an MCU's firmware, a memory's image)
  /session/run         POST: restart Emularium on the saved board;
                       ?paused=1 keeps it stopped at the start
  /session/stop        POST: end Emularium until the next Run
  /session/debug       POST: the body, a command line for Emularium's
                       debugger; its JSON answer
  /session/firmware    ?part=P: GET the file part P takes; POST the body
                       as that file (Intel HEX for an MCU), and restart
  /session/template    POST ?name=T: start over from boards/T
  /session/download    the board, its layout and the files its parts take,
                       as a zip
  /session/idle        seconds since the last request, for the hub; it
                       doesn't count as one itself
  /novnc/...           noVNC, the VNC client in the page
  /websockify          WebSocket to websockify, which speaks VNC to Emularium
  anything else        proxied to the editor (it uses /, /api/save, ...)

Usage: front.py [--port N]       serve
       front.py --init TEMPLATE  fill an empty /work from a template board
"""

import argparse
import http.client
import io
import json
import os
import re
import shutil
import socket
import sys
import threading
import time
import tomllib
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

WORK = os.environ.get("WORK", "/work")
BOARDS = os.environ.get("BOARDS", "/opt/emularium/boards")
NOVNC = os.environ.get("NOVNC", "/usr/share/novnc")
HERE = os.path.dirname(os.path.abspath(__file__))
EDITOR = ("127.0.0.1", 8001)
WEBSOCKIFY = ("127.0.0.1", 6080)
DEBUGGER = ("127.0.0.1", 3333)
LOG_TAIL = 200
MAX_UPLOAD = 1 << 20
last_use = time.monotonic()

# Boards a session can start from: name -> (board file, firmware in it).
TEMPLATES = {
    "tetris": ("tetris/tetris.toml", "tetris.hex"),
    "spiram": ("spiram/spiram.toml", "spiram_test.hex"),
}
ROM = re.compile(r'rom\s*=\s*"[^"]*"')

# Models whose parts take a file, the parameter naming it, and what it is;
# and the models the debugger knows as MCUs.
FILE_PARAMS = {"PIC16F876": ("rom", "firmware (Intel HEX)"),
               "25LC1024": ("image", "image (raw bytes)")}
MCUS = {"PIC16F876"}
# Files of the session itself, which a part's file mustn't be.
OWN = {"board.toml", "board.layout.toml", "board.svg", "template", "breakpoints",
       "emulator.log", "emulator.state"}


def path(name):
    return os.path.join(WORK, name)


def use_template(name):
    """Replaces the board, its layout and firmware with a template's, its
    PIC's ROM renamed to firmware.hex, and drops the breakpoints."""
    board, rom = TEMPLATES[name]
    src = os.path.join(BOARDS, board)
    with open(src) as f:
        text = f.read()
    if not ROM.search(text):
        raise ValueError(f"{board} has no rom = ...")
    with open(path("board.toml"), "w") as f:
        f.write(ROM.sub('rom = "firmware.hex"', text, count=1))
    layout = os.path.splitext(src)[0] + ".layout.toml"
    if os.path.exists(layout):
        shutil.copy(layout, path("board.layout.toml"))
    elif os.path.exists(path("board.layout.toml")):
        os.remove(path("board.layout.toml"))
    shutil.copy(os.path.join(os.path.dirname(src), rom), path("firmware.hex"))
    with open(path("template"), "w") as f:
        f.write(name)
    if os.path.exists(path("breakpoints")):
        os.remove(path("breakpoints"))


def valid_hex(data):
    """Intel HEX: lines of ':' and hex digits, each with a correct checksum,
    ending with an end-of-file record."""
    try:
        lines = data.decode("ascii").split()
    except UnicodeDecodeError:
        return "not text"
    if not lines:
        return "empty"
    for n, line in enumerate(lines, 1):
        if not re.fullmatch(r":([0-9A-Fa-f]{2})+", line):
            return f"line {n} isn't an Intel HEX record"
        b = bytes.fromhex(line[1:])
        if len(b) < 5 or len(b) != b[0] + 5 or sum(b) & 0xff:
            return f"line {n}: wrong length or checksum"
    if lines[-1].upper() != ":00000001FF":
        return "no end-of-file record"
    return None


def request_run(paused=False):
    prune_breakpoints()
    with open(path(".restart"), "w") as f:
        f.write("paused" if paused else "")


def parts():
    """The saved board's parts: name -> {model, param, what, file}, the
    last three for parts that take a file (file None when it names none,
    or one that isn't a plain file of the session)."""
    try:
        with open(path("board.toml"), "rb") as f:
            board = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError):
        return {}
    out = {}
    for name, p in (board.get("parts") or {}).items():
        if not isinstance(p, dict):
            continue
        model = str(p.get("model", ""))
        part = {"model": model, "mcu": model in MCUS}
        if model in FILE_PARAMS:
            param, what = FILE_PARAMS[model]
            f = p.get(param)
            ok = isinstance(f, str) and re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", f) and f not in OWN
            part.update(param=param, what=what, file=f if ok else None)
        out[name] = part
    return out


def read_breakpoints():
    try:
        with open(path("breakpoints")) as f:
            return [tuple(l.split()) for l in f if len(l.split()) == 2]
    except OSError:
        return []


def write_breakpoints(bps):
    with open(path("breakpoints.tmp"), "w") as f:
        f.writelines(f"{d} {a}\n" for d, a in bps)
    os.replace(path("breakpoints.tmp"), path("breakpoints"))


def prune_breakpoints():
    """Drops the breakpoints of MCUs the saved board no longer has, which
    Emularium would refuse to start with."""
    mcus = {n for n, p in parts().items() if p["mcu"]}
    bps = read_breakpoints()
    if any(d not in mcus for d, _ in bps):
        write_breakpoints([(d, a) for d, a in bps if d in mcus])


def debug(line, timeout=5):
    """Sends a command to Emularium's debugger; its answer, parsed, or
    None when Emularium isn't running. The breakpoints it reports are kept
    for the next run."""
    try:
        with socket.create_connection(DEBUGGER, timeout=timeout) as s:
            s.sendall(line.encode() + b"\n")
            buf = b""
            while not buf.endswith(b"\n"):
                chunk = s.recv(65536)
                if not chunk:
                    return None
                buf += chunk
    except OSError:
        return None
    try:
        answer = json.loads(buf)
    except ValueError:
        return None
    if "devices" in answer:
        bps = [(d["name"], hex(a)) for d in answer["devices"] for a in d["breakpoints"]]
        if bps != read_breakpoints():
            write_breakpoints(bps)
    return answer


def status():
    def read(name, default=""):
        try:
            with open(path(name), errors="replace") as f:
                return f.read()
        except OSError:
            return default
    log = read("emulator.log").splitlines()[-LOG_TAIL:]
    state = read("emulator.state").strip()
    return {"template": read("template").strip(), "state": state,
            "debug": debug("status", 1) if state.startswith("running") else None,
            "log": "\n".join(log)}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def reply(self, code, body, ctype="text/plain; charset=utf-8", extra=()):
        if isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        for k, v in extra:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n > MAX_UPLOAD:
            raise ValueError("too large")
        return self.rfile.read(n)

    def parse_request(self):
        global last_use
        ok = super().parse_request()
        if ok and urlparse(self.path).path != "/session/idle":
            last_use = time.monotonic()
        return ok

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/session/idle":
            return self.reply(200, json.dumps({"idle": int(time.monotonic() - last_use)}), "application/json")
        if url.path in ("/session", "/session/"):
            with open(os.path.join(HERE, "page.html"), "rb") as f:
                return self.reply(200, f.read(), "text/html; charset=utf-8")
        if url.path == "/session/status":
            return self.reply(200, json.dumps(status()), "application/json")
        if url.path == "/session/parts":
            return self.reply(200, json.dumps(parts()), "application/json")
        if url.path == "/session/firmware":
            try:
                _, p = self.part_file(url)
                with open(path(p["file"]), "rb") as f:
                    return self.reply(200, f.read(), "application/octet-stream")
            except (OSError, ValueError) as e:
                return self.reply(404, str(e))
        if url.path == "/session/templates":
            return self.reply(200, json.dumps(sorted(TEMPLATES)), "application/json")
        if url.path == "/session/download":
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
                files = ["board.toml", "board.layout.toml", "board.svg"]
                files += sorted({p["file"] for p in parts().values() if p.get("file")})
                for name in files:
                    if os.path.isfile(path(name)):
                        z.write(path(name), "board/" + name)
            return self.reply(200, buf.getvalue(), "application/zip",
                              [("Content-Disposition", 'attachment; filename="board.zip"')])
        if url.path.startswith("/novnc/"):
            return self.static(url.path[len("/novnc/"):])
        if url.path == "/websockify":
            return self.websocket()
        self.proxy()

    def do_POST(self):
        url = urlparse(self.path)
        try:
            if url.path == "/session/run":
                self.body()
                request_run(parse_qs(url.query).get("paused") == ["1"])
                return self.reply(200, "ok")
            if url.path == "/session/stop":
                self.body()
                open(path(".stop"), "w").close()
                return self.reply(200, "ok")
            if url.path == "/session/debug":
                line = self.body().decode("utf-8", "replace").strip()
                if not line or "\n" in line:
                    return self.reply(400, "One command line, please.")
                answer = debug(line)
                if answer is None:
                    return self.reply(409, "Emularium isn't running: press Run.")
                return self.reply(200, json.dumps(answer), "application/json")
            if url.path == "/session/firmware":
                name, p = self.part_file(url)
                data = self.body()
                if p["mcu"]:
                    err = valid_hex(data)
                    if err:
                        return self.reply(400, f"Not an Intel HEX file: {err}.")
                with open(path(p["file"] + ".tmp"), "wb") as f:
                    f.write(data)
                os.replace(path(p["file"] + ".tmp"), path(p["file"]))
                request_run()
                return self.reply(200, f"{p['file']} ({name}'s {p['param']})")
            if url.path == "/session/template":
                self.body()
                name = parse_qs(url.query).get("name", [""])[0]
                if name not in TEMPLATES:
                    return self.reply(400, f"No template {name!r}.")
                use_template(name)
                request_run()
                return self.reply(200, "ok")
        except (OSError, ValueError) as e:
            return self.reply(400, str(e))
        self.proxy()

    def part_file(self, url):
        """The part ?part= names, and its entry in parts(), if it takes a
        file; else a ValueError saying what's missing."""
        name = parse_qs(url.query).get("part", [""])[0]
        p = parts().get(name)
        if p is None:
            raise ValueError(f"The saved board has no part {name!r}: Save it in the editor first.")
        if "param" not in p:
            raise ValueError(f"{name} ({p['model']}) takes no file.")
        if not p["file"]:
            raise ValueError(f"{name} names no {p['param']} file: double-click it in the editor, "
                             f"give it one (e.g. {name}.hex), and Save.")
        return name, p

    def static(self, rel):
        full = os.path.realpath(os.path.join(NOVNC, rel))
        if not full.startswith(os.path.realpath(NOVNC) + os.sep) or not os.path.isfile(full):
            return self.reply(404, "not found")
        types = {".html": "text/html", ".js": "text/javascript", ".css": "text/css",
                 ".svg": "image/svg+xml", ".png": "image/png", ".json": "application/json",
                 ".ico": "image/x-icon", ".mjs": "text/javascript"}
        with open(full, "rb") as f:
            self.reply(200, f.read(), types.get(os.path.splitext(full)[1], "application/octet-stream"))

    def proxy(self):
        """Passes the request to the editor and its answer back."""
        data = self.body() if self.command == "POST" else None
        conn = http.client.HTTPConnection(*EDITOR, timeout=60)
        try:
            headers = {k: v for k, v in self.headers.items()
                       if k.lower() in ("content-type", "accept")}
            conn.request(self.command, self.path, data, headers)
            r = conn.getresponse()
            body = r.read()
        except OSError as e:
            return self.reply(502, f"The schematic editor isn't answering: {e}")
        finally:
            conn.close()
        self.reply(r.status, body, r.getheader("Content-Type", "application/octet-stream"))

    def websocket(self):
        """Hands the connection to websockify: the request as it came, then
        bytes both ways until either side closes."""
        try:
            up = socket.create_connection(WEBSOCKIFY, timeout=10)
        except OSError as e:
            return self.reply(502, f"websockify isn't answering: {e}")
        up.settimeout(None)
        head = f"{self.command} {self.path} {self.request_version}\r\n"
        head += "".join(f"{k}: {v}\r\n" for k, v in self.headers.items()) + "\r\n"
        up.sendall(head.encode())

        def down():
            try:
                while chunk := up.recv(65536):
                    self.connection.sendall(chunk)
            except OSError:
                pass
            finally:
                try:
                    self.connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        t = threading.Thread(target=down, daemon=True)
        t.start()
        try:
            while chunk := self.rfile.read1(65536):
                up.sendall(chunk)
        except OSError:
            pass
        finally:
            try:
                up.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            t.join(timeout=5)
            up.close()
        self.close_connection = True


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=8080)
    ap.add_argument("--init", metavar="TEMPLATE")
    a = ap.parse_args()
    if a.init:
        if not os.path.exists(path("board.toml")):
            use_template(a.init)
        return
    server = ThreadingHTTPServer(("0.0.0.0", a.port), Handler)
    server.daemon_threads = True
    print(f"Emularium session on port {a.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    sys.exit(main())
