"""Tests an Emularium session the way its page uses it, from outside:
upload to a part, Run and Stop, the debugger (pause, step, breakpoints
kept across runs, starting paused), the schematic editor, templates,
download, and the board's display over noVNC's WebSocket (wsvnc.py),
checking each by what the board does. A key press and its result are read
back from the display.

Usage: session-test.py http://HOST:PORT EMULARIUM_DIR
  EMULARIUM_DIR, an Emularium checkout, for the Tetris and SPI RAM
  firmware. The session starts and ends on the Tetris template.
"""
import io, json, os, re, subprocess, sys, time, urllib.request, zipfile
B = sys.argv[1].rstrip("/"); HOST, PORT = B.split("//")[1].split(":")
EMULARIUM = sys.argv[2]
WSVNC = os.path.join(os.path.dirname(os.path.abspath(__file__)), "wsvnc.py")
failed = []
def req(path, data=None):
    r = urllib.request.Request(B + path, data=data, method="POST" if data is not None else "GET")
    try:
        with urllib.request.urlopen(r, timeout=30) as f: return f.status, f.read()
    except urllib.error.HTTPError as e: return e.code, e.read()
def grab(key=None):
    out = subprocess.run([sys.executable, "-I", WSVNC, HOST, PORT] + ([key] if key else []),
                         capture_output=True, text=True).stdout
    m = re.search(r"pixels-not-background=(\d+)", out); return int(m.group(1)) if m else out.strip() or "no display"
def screen(key=None):
    """The screen once it stops changing: the PIC takes a while to draw."""
    s = grab(key)
    for _ in range(20):
        time.sleep(0.5)
        t = grab()
        if t == s: return s
        s = t
    return s
def run_id():
    st = json.loads(req("/session/status")[1])["state"].split()
    return st[1] if len(st) > 1 else None
last = [None]
def wait_running():
    """Until Emularium starts a run after the last one seen, then its display."""
    for _ in range(60):
        st = json.loads(req("/session/status")[1])["state"].split()
        if st[0] == "running" and st[1] != last[0]:
            last[0] = st[1]; time.sleep(1.5); return
        time.sleep(0.25)
    raise SystemExit("FAIL: no new run")
last[0] = run_id()
def ok(c, m):
    print(("ok:   " if c else "FAIL: ") + m)
    if not c: failed.append(m)

tetris = open(os.path.join(EMULARIUM, "boards/tetris/tetris.hex"), "rb").read()
spiram = open(os.path.join(EMULARIUM, "boards/spiram/spiram_test.hex"), "rb").read()

req("/session/run", b""); wait_running(); s_ = screen()
ok(s_ == 17152, f"the Tetris template shows its title screen ({s_})")
parts = json.loads(req("/session/parts")[1])
ok(parts.get("U1", {}).get("file") == "firmware.hex" and parts["U1"]["mcu"] and "param" not in parts.get("J1", {"param": 1}),
   f"the parts: U1, an MCU, takes firmware.hex; J1 takes nothing ({parts})")
c, b = req("/session/firmware?part=U1"); ok(c == 200 and b == tetris, "U1's firmware reads back")
F = "/session/firmware?part=U1"
c, b = req("/session/firmware?part=J1", tetris); ok(c == 400 and b"takes no file" in b, f"firmware for the LCD is refused ({b.decode().strip()})")
c, b = req("/session/firmware?part=U9", tetris); ok(c == 400 and b"no part" in b, f"firmware for a part the board hasn't is refused ({b.decode().strip()})")
c, b = req("/session/firmware", tetris); ok(c == 400, f"firmware without a part is refused ({b.decode().strip()})")
c, b = req(F, b"hello\n"); ok(c == 400 and b"Intel HEX" in b, f"a file that isn't HEX is refused ({b.decode().strip()})")
c, b = req(F, tetris[:-3] + b"00\n"); ok(c == 400, f"a HEX with a bad checksum is refused ({b.decode().strip()})")
c, b = req(F, spiram); wait_running(); s = screen()
ok(c == 200 and s != 17152, f"other firmware, uploaded to U1, runs on the board instead (lit pixels {s}; {b.decode()})")
c, _ = req(F, tetris); wait_running(); s_ = screen(); ok(c == 200 and s_ == 17152, f"uploading Tetris again brings its title back ({s_})")

# Stop, and Run again.
c, _ = req("/session/stop", b"")
for _ in range(40):
    st = json.loads(req("/session/status")[1])
    if st["state"].startswith("stopped"): break
    time.sleep(0.25)
ok(st["state"].endswith(" stop") and st["debug"] is None, f"Stop ends the board ({st['state']})")
c, b = req("/session/debug", b"status"); ok(c == 409, f"the debugger answers only while the board runs ({b.decode()})")
req("/session/run", b""); wait_running(); ok(screen() == 17152, "Run starts it again")

# The debugger.
def dbg(line):
    c, b = req("/session/debug", line.encode())
    return json.loads(b) if c == 200 else {"error": b.decode()}
d = dbg("pause")
ok(d.get("state") == "stopped" and d.get("reason") == "pause", f"pause stops the board ({d})")
time.sleep(0.5); d2 = dbg("status")
ok(d2["time_ns"] == d["time_ns"], f"paused, the board's time stands still ({d['time_ns']} ns, {d2['time_ns']} ns)")
pc, t = d["devices"][0]["pc"], d["time_ns"]
d = dbg("step"); ok(d.get("reason") == "step" and 0 < d["time_ns"] - t <= 2000,
                    f"step runs one instruction: PC {pc:#x} -> {d['devices'][0]['pc']:#x}, {d['time_ns'] - t} ns")
d = dbg("step U1 50"); here = d["devices"][0]["pc"]
d = dbg(f"break U1 {here:#x}"); ok(d.get("devices", [{}])[0].get("breakpoints") == [here], f"break U1 {here:#x}")
d = dbg("continue")
for _ in range(20):
    d = dbg("status")
    if d.get("state") == "stopped": break
    time.sleep(0.2)
ok(d.get("reason") == "breakpoint" and d["devices"][0]["pc"] == here, f"continue runs on to the breakpoint ({d.get('reason')}, PC {d['devices'][0]['pc']:#x})")
d = dbg("bogus"); ok("error" in d, f"a bad command is an error ({d})")
req("/session/run", b""); wait_running()
for _ in range(20):
    d = dbg("status")
    if d.get("state") == "stopped": break
    time.sleep(0.2)
ok(d.get("reason") == "breakpoint" and d["devices"][0]["breakpoints"] == [here], f"the breakpoint is kept for the next run, which stops at it ({d.get('reason')})")
d = dbg("delete"); d = dbg("continue"); s = screen("0x20")
ok(d.get("state") == "running" and s != 17152, f"without it, the board goes on, and a key starts a game ({s})")
req("/session/run?paused=1", b""); wait_running(); d = dbg("status")
ok(d.get("reason") == "start" and d["devices"][0]["pc"] == 0 and d["time_ns"] == 0, f"Run paused stops at the first instruction ({d.get('reason')})")
d = dbg("c"); s = [grab() for _ in range(20) if time.sleep(0.5) is None]
ok(d.get("state") == "running" and 17152 in s, "and continue starts it: the title screen comes up")

# The editor: save the board without the keypad's data-available net, as
# deleting that connection in the editor would.
page = req("/")[1].decode()
board = json.loads(re.search(r"window\.BOARD = (.*?);\n", page, re.S).group(1))
nets = [{"name": n["name"], "pins": n["pins"]} for n in board["nets"] if n["name"] != "KEY_DA"]
c, b = req("/api/save", json.dumps({"nets": nets}).encode()); ok(c == 200, f"the editor saves the board ({b.decode()})")
toml = zipfile.ZipFile(io.BytesIO(req("/session/download")[1])).read("board/board.toml").decode()
ok("KEY_DA" not in toml, "board.toml no longer has the KEY_DA net")
req("/session/run", b""); wait_running(); s = screen("0x20")
ok(s == 17152, f"after Run, the board runs the edited schematic: space no longer starts a game (lit pixels {s})")

c, _ = req("/session/template?name=spiram", b""); wait_running(); st = json.loads(req("/session/status")[1])
ok(c == 200 and st["template"] == "spiram", "switching to the SPI RAM template")
print("      its output:", " | ".join(st["log"].splitlines()[-3:]))
c, b = req("/session/download"); z = zipfile.ZipFile(io.BytesIO(b))
ok(c == 200 and {"board/board.toml", "board/firmware.hex"} <= set(z.namelist()), f"download: {z.namelist()}")
c, _ = req("/session/template?name=tetris", b""); wait_running(); s_ = screen(); ok(screen() == 17152, f"back to the Tetris template, title screen again ({s_})")

# A board Emularium can't load, as the editor would save it after a part's
# model is changed to one Emularium doesn't have.
page = req("/")[1].decode()
board = json.loads(re.search(r"window\.BOARD = (.*?);\n", page, re.S).group(1))
print("      parts as the editor has them:", {r: p["model"] for r, p in board["parts"].items()})
parts = [{"ref": r, "was": r, "model": ("NOPE123" if r == "J2" else p["model"]), "toml": p["toml"]} for r, p in board["parts"].items()]
nets = [{"name": n["name"], "pins": n["pins"]} for n in board["nets"]]
c, b = req("/api/save", json.dumps({"parts": parts, "nets": nets}).encode())
ok(c == 200, f"the editor saves a part with an unknown model ({c} {b.decode()[:100]})")
req("/session/run", b"")
for _ in range(40):
    st = json.loads(req("/session/status")[1])
    if st["state"].startswith("stopped"): break
    time.sleep(0.25)
ok(st["state"].startswith("stopped"), f"Emularium refuses it, and the page sees it stopped ({st['state']})")
print("      its error:", st["log"].splitlines()[-2:])
c, _ = req("/session/template?name=tetris", b""); wait_running(); s_ = screen(); ok(s_ == 17152, f"starting over from Tetris works after a broken board ({s_})")

sys.exit(1 if failed else 0)
