#!/usr/bin/env python3
"""The Emularium hub: gives each user a session of their own. "New session"
creates a session pod (as ../session/session.yaml describes one) and a
NodePort Service for it through the Kubernetes API, and sends the browser
there. Sessions go when their user ends them, when no page has used them
for IDLE_MINUTES, or after MAX_HOURS; at most MAX_SESSIONS run at once.

  /                       the hub's page
  /api/sessions           GET: the sessions; POST ?template=T: a new one
  /api/sessions/NAME      DELETE: end it

The pod's ServiceAccount may create, list and delete pods and services in
its namespace (hub.yaml). There are no accounts: anyone who reaches the hub
can start and end sessions, so it belongs on a trusted network only.
"""

import calendar
import json
import os
import re
import secrets
import ssl
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

SA = "/var/run/secrets/kubernetes.io/serviceaccount"
IMAGE = os.environ.get("SESSION_IMAGE", "emularium-session:1")
MAX_SESSIONS = int(os.environ.get("MAX_SESSIONS", "4"))
IDLE_MINUTES = int(os.environ.get("IDLE_MINUTES", "30"))
MAX_HOURS = int(os.environ.get("MAX_HOURS", "8"))
TEMPLATES = ("tetris", "spiram")
LABEL = "emularium-hub/session"
SESSION_NAME = re.compile(r"emularium-[0-9a-f]{6}")
HERE = os.path.dirname(os.path.abspath(__file__))


class Kube:
    """Just enough of the Kubernetes API, with the pod's ServiceAccount."""

    def __init__(self):
        self.base = "https://{}:{}".format(os.environ["KUBERNETES_SERVICE_HOST"],
                                           os.environ["KUBERNETES_SERVICE_PORT"])
        self.ctx = ssl.create_default_context(cafile=f"{SA}/ca.crt")
        with open(f"{SA}/namespace") as f:
            self.ns = f.read().strip()

    def call(self, method, path, body=None, missing_ok=False):
        with open(f"{SA}/token") as f:          # rotated by the kubelet
            token = f.read().strip()
        req = urllib.request.Request(self.base + path, method=method,
                                     data=json.dumps(body).encode() if body is not None else None,
                                     headers={"Authorization": f"Bearer {token}",
                                              "Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(req, context=self.ctx, timeout=15) as r:
                return json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code == 404 and missing_ok:
                return None
            raise RuntimeError(f"{method} {path}: {e.code} {e.read().decode(errors='replace')[:300]}")

    def pods(self):
        return self.call("GET", f"/api/v1/namespaces/{self.ns}/pods?labelSelector={LABEL}")["items"]

    def services(self):
        return self.call("GET", f"/api/v1/namespaces/{self.ns}/services?labelSelector={LABEL}")["items"]


def session_pod(name, template):
    """The pod of ../session/session.yaml, named for this session."""
    def c(cname, command, cpu, mem, limit, work=True, cpu_limit=None):
        lim = {"memory": limit}
        if cpu_limit:
            lim["cpu"] = cpu_limit
        spec = {"name": cname, "image": IMAGE, "imagePullPolicy": "Never", "command": command,
                "resources": {"requests": {"cpu": cpu, "memory": mem}, "limits": lim}}
        if work:
            spec["volumeMounts"] = [{"name": "work", "mountPath": "/work"}]
        return spec
    front = c("front", ["python3", "/opt/session/front.py", "--port", "8080"], "20m", "32Mi", "128Mi")
    front["ports"] = [{"name": "http", "containerPort": 8080}]
    front["readinessProbe"] = {"httpGet": {"path": "/session/idle", "port": "http"}, "periodSeconds": 5}
    init = c("template", ["python3", "/opt/session/front.py", "--init", template], "10m", "16Mi", "64Mi")
    return {
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "labels": {"app": "emularium-session", LABEL: name}},
        "spec": {
            "initContainers": [init],
            "containers": [
                front,
                c("editor", ["python3", "/opt/emularium/tools/schematic.py", "/work/board.toml",
                             "--no-browser", "--port", "8001"], "10m", "32Mi", "128Mi"),
                c("emulator", ["emulator-loop"], "350m", "16Mi", "128Mi", cpu_limit="1"),
                c("websockify", ["websockify", "127.0.0.1:6080", "127.0.0.1:5900"], "10m", "16Mi", "64Mi",
                  work=False),
            ],
            "volumes": [{"name": "work", "emptyDir": {"sizeLimit": "16Mi"}}],
            "automountServiceAccountToken": False,
            # Nothing in a session saves anything on the way out.
            "terminationGracePeriodSeconds": 2,
        },
    }


def session_service(name):
    return {
        "apiVersion": "v1", "kind": "Service",
        "metadata": {"name": name, "labels": {LABEL: name}},
        "spec": {"type": "NodePort", "selector": {LABEL: name},
                 "ports": [{"port": 8080, "targetPort": "http"}]},
    }


def age_seconds(obj):
    """Kubernetes' timestamps are UTC, e.g. 2026-11-21T06:30:00Z."""
    created = calendar.timegm(time.strptime(obj["metadata"]["creationTimestamp"], "%Y-%m-%dT%H:%M:%SZ"))
    return time.time() - created


def idle_seconds(pod):
    """What the session's front says, or None while it can't be asked."""
    ip = pod.get("status", {}).get("podIP")
    if not ip:
        return None
    try:
        with urllib.request.urlopen(f"http://{ip}:8080/session/idle", timeout=3) as r:
            return json.loads(r.read())["idle"]
    except (OSError, ValueError, KeyError):
        return None


class Hub:
    def __init__(self):
        self.kube = Kube()
        self.lock = threading.Lock()

    def sessions(self):
        ports = {s["metadata"]["name"]: s["spec"]["ports"][0].get("nodePort")
                 for s in self.kube.services()}
        out = []
        for p in self.kube.pods():
            name = p["metadata"]["name"]
            statuses = p.get("status", {}).get("containerStatuses") or []
            ending = "deletionTimestamp" in p["metadata"]
            out.append({
                "name": name,
                "phase": "Ending" if ending else p.get("status", {}).get("phase", "Pending"),
                "ready": not ending and bool(statuses) and all(s.get("ready") for s in statuses),
                "nodePort": ports.get(name),
                "age": int(age_seconds(p)),
                "idle": idle_seconds(p),
            })
        return sorted(out, key=lambda s: s["age"])

    def create(self, template):
        if template not in TEMPLATES:
            raise ValueError(f"no template {template!r}")
        with self.lock:
            if len(self.kube.pods()) >= MAX_SESSIONS:
                raise ValueError(f"all {MAX_SESSIONS} sessions are in use; try again later")
            name = "emularium-" + secrets.token_hex(3)
            ns = self.kube.ns
            self.kube.call("POST", f"/api/v1/namespaces/{ns}/pods", session_pod(name, template))
            try:
                svc = self.kube.call("POST", f"/api/v1/namespaces/{ns}/services", session_service(name))
            except RuntimeError:
                self.delete(name)
                raise
        return name, svc["spec"]["ports"][0]["nodePort"]

    def delete(self, name):
        """Deletes a session's Service and pod, and nothing else: only names
        the hub makes, and only objects labelled as that session (its Role
        allows deleting any pod or Service in the namespace, the hub's own
        Service included)."""
        if not SESSION_NAME.fullmatch(name):
            raise ValueError("not a session")
        ns = self.kube.ns
        found = False
        for kind in ("services", "pods"):
            obj = self.kube.call("GET", f"/api/v1/namespaces/{ns}/{kind}/{name}", missing_ok=True)
            if obj is None:
                continue
            if obj["metadata"].get("labels", {}).get(LABEL) != name:
                raise ValueError("not a session")
            self.kube.call("DELETE", f"/api/v1/namespaces/{ns}/{kind}/{name}", missing_ok=True)
            found = True
        if not found:
            raise ValueError("no such session")

    def reap(self):
        """Ends sessions nobody uses, and ones past their time."""
        while True:
            time.sleep(60)
            try:
                for s in self.sessions():
                    idle = s["idle"]
                    if s["age"] > MAX_HOURS * 3600 or (idle is not None and idle > IDLE_MINUTES * 60):
                        print(f"ending {s['name']}: age {s['age']} s, idle {idle} s", flush=True)
                        self.delete(s["name"])
            except Exception as e:                  # keep reaping whatever went wrong
                print("reaping:", e, file=sys.stderr, flush=True)


def handler(hub):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            pass

        def reply(self, code, body, ctype="application/json"):
            if not isinstance(body, bytes):
                body = (body if isinstance(body, str) else json.dumps(body)).encode()
            self.send_response(code)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def host(self):
            """The node's address as the browser knows it: a session's
            NodePort is on the same address as the hub's."""
            return (self.headers.get("Host") or "localhost").rsplit(":", 1)[0]

        def do_GET(self):
            url = urlparse(self.path)
            if url.path == "/":
                with open(os.path.join(HERE, "hub.html"), "rb") as f:
                    return self.reply(200, f.read(), "text/html; charset=utf-8")
            if url.path == "/api/sessions":
                try:
                    sessions = hub.sessions()
                except RuntimeError as e:
                    return self.reply(502, {"error": str(e)})
                for s in sessions:
                    if s["nodePort"]:
                        s["url"] = f"http://{self.host()}:{s['nodePort']}/session/"
                return self.reply(200, {"sessions": sessions, "max": MAX_SESSIONS,
                                        "templates": TEMPLATES, "idleMinutes": IDLE_MINUTES})
            self.reply(404, {"error": "not found"})

        def do_POST(self):
            url = urlparse(self.path)
            self.rfile.read(int(self.headers.get("Content-Length") or 0))
            if url.path != "/api/sessions":
                return self.reply(404, {"error": "not found"})
            template = parse_qs(url.query).get("template", ["tetris"])[0]
            try:
                name, port = hub.create(template)
            except ValueError as e:
                return self.reply(409, {"error": str(e)})
            except RuntimeError as e:
                return self.reply(502, {"error": str(e)})
            self.reply(201, {"name": name, "url": f"http://{self.host()}:{port}/session/"})

        def do_DELETE(self):
            url = urlparse(self.path)
            prefix = "/api/sessions/"
            if not url.path.startswith(prefix):
                return self.reply(404, {"error": "not found"})
            try:
                hub.delete(url.path[len(prefix):])
            except ValueError as e:
                return self.reply(400, {"error": str(e)})
            except RuntimeError as e:
                return self.reply(502, {"error": str(e)})
            self.reply(200, {"ok": True})
    return Handler


def main():
    hub = Hub()
    threading.Thread(target=hub.reap, daemon=True).start()
    server = ThreadingHTTPServer(("0.0.0.0", int(os.environ.get("PORT", "8080"))), handler(hub))
    server.daemon_threads = True
    print(f"Emularium hub: at most {MAX_SESSIONS} sessions, idle after {IDLE_MINUTES} min", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
