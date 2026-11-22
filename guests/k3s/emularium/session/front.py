#!/usr/bin/env python3
"""The front of an Emularium session pod: the only server on the pod's
address, as Emularium and its schematic editor listen on 127.0.0.1 only.

The pod's containers share /work: the editor edits /work/board.toml, the
emulator loop (emulator-loop) runs Emularium on it, and this server ties
them together for the browser:

  /session/            the page: the editor beside the board's display
  /session/status      JSON: the template, whether Emularium runs, its log
  /session/run         POST: restart Emularium on the saved board
  /session/firmware    POST: the body, an Intel HEX file, as the PIC's ROM
  /session/template    POST ?name=T: start over from boards/T
  /session/download    the board, its layout and firmware, as a zip
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
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

WORK = os.environ.get("WORK", "/work")
BOARDS = os.environ.get("BOARDS", "/opt/emularium/boards")
NOVNC = os.environ.get("NOVNC", "/usr/share/novnc")
HERE = os.path.dirname(os.path.abspath(__file__))
EDITOR = ("127.0.0.1", 8001)
WEBSOCKIFY = ("127.0.0.1", 6080)
LOG_TAIL = 200
MAX_UPLOAD = 1 << 20
last_use = time.monotonic()

# Boards a session can start from: name -> (board file, firmware in it).
TEMPLATES = {
    "tetris": ("tetris/tetris.toml", "tetris.hex"),
    "spiram": ("spiram/spiram.toml", "spiram_test.hex"),
}
ROM = re.compile(r'rom\s*=\s*"[^"]*"')


def path(name):
    return os.path.join(WORK, name)


def use_template(name):
    """Replaces the board, its layout and firmware with a template's, its
    PIC's ROM renamed to firmware.hex, the file uploads replace."""
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


def request_run():
    open(path(".restart"), "w").close()


def status():
    def read(name, default=""):
        try:
            with open(path(name), errors="replace") as f:
                return f.read()
        except OSError:
            return default
    log = read("emulator.log").splitlines()[-LOG_TAIL:]
    return {"template": read("template").strip(), "state": read("emulator.state").strip(),
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
        if url.path == "/session/templates":
            return self.reply(200, json.dumps(sorted(TEMPLATES)), "application/json")
        if url.path == "/session/download":
            buf = io.BytesIO()
            with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
                for name in ("board.toml", "board.layout.toml", "firmware.hex", "board.svg"):
                    if os.path.exists(path(name)):
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
                request_run()
                return self.reply(200, "ok")
            if url.path == "/session/firmware":
                data = self.body()
                err = valid_hex(data)
                if err:
                    return self.reply(400, f"Not an Intel HEX file: {err}.")
                with open(path("firmware.hex"), "wb") as f:
                    f.write(data)
                request_run()
                return self.reply(200, "ok")
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
