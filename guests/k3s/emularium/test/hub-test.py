#!/usr/bin/env python3
"""Tests the Emularium hub from outside: sessions are created, ready, kept
apart and capped; the hub ends only its own sessions; and a session it
creates passes session-test.py.

Usage: hub-test.py http://HOST:30999 EMULARIUM_DIR
  Expects no sessions at the start, and MAX_SESSIONS at its default, 4;
  ends every session it made.
"""
import json, os, subprocess, sys, time, urllib.error, urllib.request

HUB = sys.argv[1].rstrip("/")
EMULARIUM = sys.argv[2]
HERE = os.path.dirname(os.path.abspath(__file__))
failed = []


def ok(cond, msg):
    print(("ok:   " if cond else "FAIL: ") + msg)
    if not cond:
        failed.append(msg)


def call(method, path):
    req = urllib.request.Request(HUB + path, method=method, data=b"" if method == "POST" else None)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


def sessions():
    return {s["name"]: s for s in call("GET", "/api/sessions")[1]["sessions"]}


def wait_ready(name, timeout=120):
    end = time.time() + timeout
    while time.time() < end:
        s = sessions().get(name)
        if s and s["ready"]:
            return s
        time.sleep(1)
    return None


def screen(url, key=None):
    host, port = url.split("//")[1].split("/")[0].split(":")
    out = subprocess.run([sys.executable, "-I", os.path.join(HERE, "wsvnc.py"), host, port] + ([key] if key else []),
                         capture_output=True, text=True).stdout
    return out.rsplit("=", 1)[-1].strip()


ok(call("GET", "/api/sessions")[0] == 200 and not sessions(), "the hub answers, with no sessions")

made = []
t0 = time.time()
code, a = call("POST", "/api/sessions?template=tetris")
ok(code == 201, f"a session is created ({a.get('name')})")
made.append(a["name"])
sa = wait_ready(a["name"])
ok(sa is not None, f"it is ready in {time.time() - t0:.1f} s, at {a['url']}")

print("--- session-test.py on it:")
r = subprocess.run([sys.executable, "-I", os.path.join(HERE, "session-test.py"), a["url"].rsplit("/session/", 1)[0], EMULARIUM])
ok(r.returncode == 0, "the session passes session-test.py")
print("---")

code, b = call("POST", "/api/sessions?template=tetris")
made.append(b["name"])
for t in ("spiram", "tetris"):
    made.append(call("POST", f"/api/sessions?template={t}")[1]["name"])
code, e = call("POST", "/api/sessions?template=tetris")
ok(code == 409, f"a fifth session is refused ({e.get('error')})")
ok(all(wait_ready(n) for n in made), "all four sessions are ready")

time.sleep(2)
before = (screen(a["url"]), screen(b["url"]))
screen(a["url"], "0x20")
time.sleep(2)
after = (screen(a["url"]), screen(b["url"]))
ok(before[1] == after[1] and before[0] != after[0],
   f"a key pressed in one session changes only that one (A {before[0]} -> {after[0]}, B {before[1]} -> {after[1]})")

for name in ("emularium-hub", "kubernetes", "emularium-abc", "emularium-000000"):
    code, j = call("DELETE", f"/api/sessions/{name}")
    ok(code == 400, f"DELETE {name} is refused ({j.get('error')})")
ok(call("GET", "/api/sessions")[0] == 200, "the hub, and its own Service, are still there after them")

for name in made:
    call("DELETE", f"/api/sessions/{name}")
end = time.time() + 60
while time.time() < end and sessions():
    time.sleep(1)
ok(not sessions(), "ending all sessions leaves none")
sys.exit(1 if failed else 0)
