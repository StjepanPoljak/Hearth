"""VNC over WebSocket, as noVNC does it: wsvnc.py HOST PORT [KEYSYM]."""
import base64, os, socket, struct, sys, time
host, port = sys.argv[1], int(sys.argv[2])
s = socket.create_connection((host, port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((f"GET /websockify HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n").encode())
head = b""
while b"\r\n\r\n" not in head: head += s.recv(1)
assert head.startswith(b"HTTP/1.1 101"), head
buf = b""
def frame_in():
    global buf
    def need(n):
        global buf
        while len(buf) < n:
            c = s.recv(65536)
            if not c: raise EOFError
            buf += c
    need(2); n = buf[1] & 0x7f; off = 2
    if n == 126: need(4); n = struct.unpack(">H", buf[2:4])[0]; off = 4
    elif n == 127: need(10); n = struct.unpack(">Q", buf[2:10])[0]; off = 10
    need(off + n); data = buf[off:off+n]; buf = buf[off+n:]; return data
rfb = b""
def recv(n):
    global rfb
    while len(rfb) < n: rfb += frame_in()
    d, rfb = rfb[:n], rfb[n:]; return d
def send(data):
    m = os.urandom(4); hdr = bytes([0x82])
    hdr += bytes([0x80 | len(data)]) if len(data) < 126 else bytes([0x80 | 126]) + struct.pack(">H", len(data))
    s.sendall(hdr + m + bytes(b ^ m[i % 4] for i, b in enumerate(data)))
ver = recv(12); send(ver)
types = list(recv(recv(1)[0])); send(bytes([1])); assert struct.unpack(">I", recv(4))[0] == 0
send(bytes([1])); w, h = struct.unpack(">HH", recv(4)); recv(16); name = recv(struct.unpack(">I", recv(4))[0]).decode()
if len(sys.argv) > 3:
    for down in (1, 0): send(struct.pack(">BBHI", 4, down, 0, int(sys.argv[3], 0))); time.sleep(0.3)
    time.sleep(2)
send(struct.pack(">BBHHHH", 3, 0, 0, 0, w, h))
recv(4); x, y, rw, rh, enc = struct.unpack(">HHHHi", recv(12)); px = recv(rw * rh * 4)
lit = sum(1 for i in range(0, len(px), 4) if px[i:i+3] != px[0:3])
print(f"websocket ok, {ver.decode().strip()}, {w}x{h} {name!r}, pixels-not-background={lit}")
