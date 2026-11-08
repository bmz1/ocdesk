#!/usr/bin/env python3
"""M3 contract smoke test against real opencode 2.2.6 + the deterministic model.

Usage: python3 dev/m3-protocol-test.py [/path/to/opencode2]
Uses a temporary repository and config/data directories; no provider keys needed.
"""
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
import urllib.error
import urllib.parse
import urllib.request
import base64

ROOT = Path(__file__).resolve().parents[1]


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("OCDESK_OPENCODE", "opencode2")
    with tempfile.TemporaryDirectory(prefix="ocdesk-m3-") as tmp:
        work = Path(tmp)
        project = work / "project"
        project.mkdir()
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        (project / "README.md").write_text("M3 test\n")
        subprocess.run(["git", "-C", str(project), "add", "."], check=True)
        subprocess.run(["git", "-C", str(project), "-c", "user.name=QA", "-c", "user.email=qa@example.com", "commit", "-qm", "init"], check=True)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            mock_port = sock.getsockname()[1]
        config = work / "config" / "opencode"
        config.mkdir(parents=True)
        settings = json.loads((ROOT / "dev/xdg/opencode/opencode.json").read_text())
        settings["provider"]["mock"]["options"]["baseURL"] = f"http://127.0.0.1:{mock_port}/v1"
        settings["permission"] = {"shell": "ask", "edit": "ask"}
        (config / "opencode.json").write_text(json.dumps(settings))
        env = dict(os.environ, XDG_CONFIG_HOME=str(work / "config"), XDG_DATA_HOME=str(work / "data"), OPENCODE_SERVER_PASSWORD="m3-test-only")
        mock_log = (work / "mock.log").open("w")
        mock = subprocess.Popen([sys.executable, str(ROOT / "dev/mock_llm.py"), str(mock_port)], stdout=mock_log, stderr=mock_log)
        server = subprocess.Popen([binary, "serve", "--hostname", "127.0.0.1", "--port", "0"], env=env, cwd=project, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            lines = queue.Queue()
            def read_stdout():
                for line in server.stdout:
                    lines.put(line)
            threading.Thread(target=read_stdout, daemon=True).start()
            deadline = time.monotonic() + 40
            base = None
            while time.monotonic() < deadline:
                line = lines.get(timeout=40)
                match = re.search(r"server listening on (http://127\.0\.0\.1:\d+)", line)
                if match:
                    base = match.group(1)
                    break
            assert base, "opencode did not start"
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            auth = "Basic " + base64.b64encode(b"opencode:m3-test-only").decode()
            def request(path, body=None, method=None):
                raw = None if body is None else json.dumps(body).encode()
                req = urllib.request.Request(base + path, data=raw, method=method, headers={"Authorization": auth, "Content-Type": "application/json"})
                with opener.open(req, timeout=40) as response:
                    data = response.read()
                    return json.loads(data) if data else None
            assert request("/api/info")["version"] == "2.2.6"
            events = queue.Queue()
            def read_events():
                req = urllib.request.Request(base + "/api/event", headers={"Authorization": auth})
                with opener.open(req, timeout=90) as response:
                    for line in response:
                        if line.startswith(b"data: "):
                            events.put(json.loads(line[6:]))
            threading.Thread(target=read_events, daemon=True).start()
            def wait_event(kind, sid=None):
                until = time.monotonic() + 40
                while time.monotonic() < until:
                    event = events.get(timeout=40)
                    if kind == "permission.asked" and event["type"] in ("session.execution.failed", "session.execution.succeeded") and event.get("data", {}).get("sessionID") == sid:
                        raise AssertionError(f"Turn ended without requesting permission: {event}")
                    if kind == "terminal" and event["type"] in ("session.execution.succeeded", "session.execution.failed", "session.execution.interrupted") and event.get("data", {}).get("sessionID") == sid:
                        return event["data"]
                    if event["type"] == kind and (sid is None or event.get("data", {}).get("sessionID") == sid):
                        return event["data"]
                raise AssertionError(f"No {kind} for {sid}")
            wait_event("server.connected")
            for decision, prompt in [("once", "tool:bash please"), ("reject", "tool:bash please"), ("always", "tool:edit please")]:
                sid = request("/api/session", {"location": {"directory": str(project)}})["data"]["id"]
                request(f"/api/session/{sid}/prompt", {"text": prompt})
                asked = wait_event("permission.asked", sid)
                pending = request(f"/api/session/{sid}/permission")["data"]
                assert any(p["id"] == asked["id"] for p in pending), "pending request must be recoverable without its SSE event"
                # ocdesk recovers every session's requests with one read per folder.
                folder = request("/api/permission/request?" + urllib.parse.urlencode({"location[directory]": str(project)}))["data"]
                assert any(p["id"] == asked["id"] and p["sessionID"] == sid for p in folder), folder
                assert request(f"/api/session/{sid}/permission/{asked['id']}/reply", {"decision": decision}) is None
                replied = wait_event("permission.replied", sid)
                assert replied["requestID"] == asked["id"] and replied["reply"] == decision
                wait_event("terminal", sid)
                assert request(f"/api/session/{sid}/permission")["data"] == []
                history = request(f"/api/session/{sid}/message?order=asc")["data"]
                tools = [b for m in history if m["type"] == "assistant" for b in m.get("content", []) if b["type"] == "tool"]
                assert tools, "tool must appear in REST history"
                status = tools[0]["state"]["status"]
                assert status == ("error" if decision == "reject" else "completed"), tools
                if decision == "always":
                    diffs = request(f"/api/session/{sid}/diff?context=3")["data"]
                    assert any(f["file"].endswith("mock_note.txt") and f["additions"] == 2 and "+line one" in f["patch"] for f in diffs), diffs
                print(f"PASS {decision}: permission event, pending recovery, 204 reply, tool {status}" + (", file diff" if decision == "always" else ""), flush=True)
            # Question forms are also recovered with one read per folder.
            sid = request("/api/session", {"location": {"directory": str(project)}})["data"]["id"]
            created = request(f"/api/session/{sid}/form", {"title": "Proceed?", "fields": [{"key": "ok", "type": "boolean"}]})
            form_id = (created.get("data") or created)["id"]
            folder = request("/api/form?" + urllib.parse.urlencode({"location[directory]": str(project)}))["data"]
            assert any(f["id"] == form_id and f["sessionID"] == sid for f in folder), folder
            print("PASS folder forms: pending form listed per folder", flush=True)
            print("M3 protocol smoke test passed", flush=True)
        finally:
            for child in (server, mock):
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            mock_log.close()


if __name__ == "__main__":
    main()
