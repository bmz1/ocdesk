"""Shared bootstrap for protocol tests: a temporary git repo, isolated opencode
config/data, the deterministic mock model, a real `opencode serve`, an
authenticated JSON client and an SSE event queue.

    from harness import Harness
    with Harness(binary) as h:
        sid = h.request("/api/session", {"location": {"directory": str(h.project)}})["data"]["id"]
        h.request(f"/api/session/{sid}/prompt", {"text": "hi"})
        h.wait_event("terminal", sid)
"""
import base64
import json
import os
from pathlib import Path
import queue
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TERMINAL = ("session.execution.succeeded", "session.execution.failed", "session.execution.interrupted")


class Harness:
    def __init__(self, binary=None, permission=None, password="harness-only", files=None, config=None):
        """`files`: {relative path: text} written into the project before the
        first commit. `config`: keys merged into opencode.json."""
        self.binary = binary or os.environ.get("OCDESK_OPENCODE", "opencode2")
        self.permission = permission
        self.files = files or {}
        self.config = config or {}
        self.password = password
        self.events = queue.Queue()
        self.seen = []  # every event, for assertions after the fact

    def __enter__(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="ocdesk-harness-")
        work = Path(self._tmp.name)
        self.work = work
        self.project = work / "project"
        self.project.mkdir()
        git = ["git", "-C", str(self.project)]
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.project)], check=True)
        (self.project / "README.md").write_text("harness\n")
        for rel, text in self.files.items():
            path = self.project / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(git + ["add", "."], check=True)
        subprocess.run(git + ["-c", "user.name=QA", "-c", "user.email=qa@example.com", "commit", "-qm", "init"], check=True)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            mock_port = sock.getsockname()[1]
        config = work / "config" / "opencode"
        config.mkdir(parents=True)
        settings = json.loads((ROOT / "dev/xdg/opencode/opencode.json").read_text())
        settings["provider"]["mock"]["options"]["baseURL"] = f"http://127.0.0.1:{mock_port}/v1"
        settings.update(self.config)
        if self.permission is not None:
            settings["permission"] = self.permission
        (config / "opencode.json").write_text(json.dumps(settings))
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"), XDG_DATA_HOME=str(work / "data"),
                   OPENCODE_SERVER_PASSWORD=self.password)
        self.mock_log = (work / "mock.log").open("w")
        self.mock = subprocess.Popen([sys.executable, str(ROOT / "dev/mock_llm.py"), str(mock_port)],
                                     stdout=self.mock_log, stderr=self.mock_log)
        self.server = subprocess.Popen([self.binary, "serve", "--hostname", "127.0.0.1", "--port", "0"], env=env,
                                       cwd=self.project, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        lines = queue.Queue()
        threading.Thread(target=lambda: [lines.put(l) for l in self.server.stdout], daemon=True).start()
        deadline = time.monotonic() + 40
        self.base = None
        while time.monotonic() < deadline and not self.base:
            m = re.search(r"server listening on (http://127\.0\.0\.1:\d+)", lines.get(timeout=40))
            if m:
                self.base = m.group(1)
        assert self.base, "opencode did not start"
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.auth = "Basic " + base64.b64encode(f"opencode:{self.password}".encode()).decode()
        threading.Thread(target=self._read_events, daemon=True).start()
        self.wait_event("server.connected")
        return self

    def __exit__(self, *exc):
        for child in (self.server, self.mock):
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
        self.mock_log.close()
        self._tmp.cleanup()

    def request(self, path, body=None, method=None):
        raw = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=raw, method=method,
                                     headers={"Authorization": self.auth, "Content-Type": "application/json"})
        with self.opener.open(req, timeout=40) as response:
            data = response.read()
            return json.loads(data) if data else None

    def _read_events(self):
        req = urllib.request.Request(self.base + "/api/event", headers={"Authorization": self.auth})
        with self.opener.open(req, timeout=600) as response:
            for line in response:
                if line.startswith(b"data: "):
                    ev = json.loads(line[6:])
                    self.seen.append(ev)
                    self.events.put(ev)

    def wait_event(self, kind, sid=None, timeout=40):
        """Next event of `kind` (or "terminal" for any turn end) for `sid`."""
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            try:
                event = self.events.get(timeout=max(0.1, until - time.monotonic()))
            except queue.Empty:
                break
            data = event.get("data", {})
            if sid is not None and data.get("sessionID") != sid:
                continue
            if kind == "terminal" and event["type"] in TERMINAL:
                return event
            if event["type"] == kind:
                return event
        raise AssertionError(f"no {kind} for {sid} within {timeout}s")

    def new_session(self, directory=None):
        body = {"location": {"directory": str(directory or self.project)}}
        return self.request("/api/session", body)["data"]["id"]
