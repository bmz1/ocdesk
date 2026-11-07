#!/usr/bin/env python3
"""Deterministic OpenAI-compatible streaming LLM for exercising ocdesk offline.

Dev tooling only; never shipped. Usage:
    python3 dev/mock_llm.py [port]          # default 4555
then run opencode with dev/mock-opencode.json (see README "Developing").

Behaviour is keyed on the last user message:
    contains "tool:bash"  -> calls the shell tool ("bash" on old servers), then summarizes
    contains "tool:edit"  -> calls the write tool on mock_note.txt, then summarizes
    contains "tool:task"  -> calls the subagent tool (spawns a "general" subagent), then summarizes
    contains "tool:sleep" -> runs `sleep 20` through the shell tool (a long-running command)
    contains "slow"       -> a reply streamed over ~10 s (for queue/steer testing)
    contains "long"       -> streams a long markdown answer with code blocks
    contains "rich"       -> long reasoning + every markdown construct (tables, quotes, lists, code)
    anything else         -> short reasoning + a short markdown answer
"""
import json
import os
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DELAY = float(os.environ.get("MOCK_DELAY", "0.03"))  # seconds per streamed word


def last_user_text(messages):
    for m in reversed(messages):
        if m.get("role") == "user":
            c = m.get("content")
            if isinstance(c, list):
                return " ".join(p.get("text", "") for p in c if isinstance(p, dict))
            return c or ""
    return ""


def words(s):
    out, cur = [], ""
    for ch in s:
        cur += ch
        if ch in " \n":
            out.append(cur)
            cur = ""
    if cur:
        out.append(cur)
    return out


LONG = "# A long answer\n\n" + "\n\n".join(
    f"## Section {i}\n\nSome *emphasis*, some `inline code`, and a [link](https://opencode.ai).\n\n"
    f"```rust\nfn section_{i}() -> u32 {{\n    let x = {i};\n    x * 2\n}}\n```\n\n- item one\n- item two"
    for i in range(1, 9)
)

RICH_REASONING = (
    "The user wants to see every **markdown** construct. I'll cover headings, "
    "emphasis, `inline code`, lists, a table, a quote and a few code blocks.\n\n"
    "- check the table renders\n- check copy buttons on code\n"
)

RICH = """## Rich markdown

Plain text with **bold**, *italic*, ~~strike~~, `inline code` and a [link to opencode](https://opencode.ai).

> A blockquote that spans
> two lines, with `code` inside.

1. First ordered item
2. Second, with a nested list:
   - nested bullet
   - another one
3. Third

- [x] done task
- [ ] open task

| Column | Type | Notes |
|---|---|---|
| `id` | string | primary key |
| `created` | u64 | ms since epoch |

```rust
fn main() {
    println!("hello from rust");
}
```

```python
def greet(name: str) -> str:
    return f"hello {name}"
```

```
plain fenced block without a language
```

---

That's all.
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("mock_llm: " + fmt % args + "\n")

    def do_GET(self):
        if self.path.endswith("/models"):
            self._json({"object": "list", "data": [{"id": "mock-1", "object": "model"}]})
        else:
            self._json({"ok": True})

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")
        msgs = req.get("messages", [])
        has_tool_result = bool(msgs) and msgs[-1].get("role") == "tool"
        user = last_user_text(msgs)
        sys_text = " ".join(
            m.get("content", "") if isinstance(m.get("content"), str) else ""
            for m in msgs if m.get("role") == "system"
        )
        is_title = "title" in sys_text.lower() and not req.get("tools")
        tools = {t.get("function", {}).get("name"): t.get("function", {}) for t in req.get("tools") or []}
        offered = set(tools)
        if offered:
            sys.stderr.write("mock_llm: tools offered: " + ", ".join(sorted(n for n in offered if n)) + "\n")

        def pick(*names):
            """First tool name the server offers (opencode 2.x renamed bash -> shell)."""
            return next((n for n in names if n in offered), names[0])

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

        def chunk(delta, finish=None):
            payload = {
                "id": "mock", "object": "chat.completion.chunk", "created": int(time.time()),
                "model": "mock-1",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }
            self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
            self.wfile.flush()

        def stream_text(text, key="content"):
            for w in words(text):
                chunk({key: w})
                time.sleep(DELAY)

        def tool_call(name, args):
            chunk({"tool_calls": [{"index": 0, "id": f"call_{int(time.time()*1000)}", "type": "function",
                                   "function": {"name": name, "arguments": ""}}]})
            s = json.dumps(args)
            for i in range(0, len(s), 12):
                chunk({"tool_calls": [{"index": 0, "function": {"arguments": s[i:i+12]}}]})
                time.sleep(DELAY)
            chunk({}, "tool_calls")

        try:
            chunk({"role": "assistant"})
            if is_title:
                # opencode 2.x asks: "...<message>\n{prompt}\n</message>\n\nReply with only the title."
                m = re.search(r"<message>\s*(.*?)\s*</message>", user, re.S)
                first = (m.group(1) if m else user).strip().splitlines()
                stream_text(" ".join((first[0] if first else "").split()[:6]) or "Mock session")
                chunk({}, "stop")
            elif has_tool_result:
                stream_text("Thinking about the tool output.", "reasoning_content")
                stream_text("The tool finished. Output was:\n\n```\n" + str(msgs[-1].get("content"))[:400] + "\n```\n")
                chunk({}, "stop")
            elif "tool:sleep" in user:
                stream_text("Starting a long command.\n")
                tool_call(pick("shell", "bash"), {"command": "sleep 20 && echo slept", "description": "Wait a while"})
            elif "tool:bash" in user:
                stream_text("I should run a command.", "reasoning_content")
                stream_text("Running a command.\n")
                tool_call(pick("shell", "bash"), {"command": "echo hello-from-bash && ls | head -5", "description": "Say hello and list files"})
            elif "tool:task" in user and "Subagent:" not in user:
                # Fill the server's own task-tool schema, whatever its field names.
                name = pick("subagent", "task")
                params = tools.get(name, {}).get("parameters", {})
                sys.stderr.write(f"mock_llm: {name} schema: " + json.dumps(params) + "\n")
                values = {"description": "Look around", "prompt": "Subagent: say hello",
                          "subagent_type": "general", "agent": "general", "subagent": "general"}
                props = params.get("properties", {})
                args = {k: values.get(k, "general") for k in (params.get("required") or props)}
                stream_text("Delegating to a subagent.\n")
                tool_call(name, args)
            elif "slow" in user:
                for w in words("A deliberately slow reply, one word at a time, so there is time to queue or steer. " * 3):
                    chunk({"content": w})
                    time.sleep(0.25)
                chunk({}, "stop")
            elif "tool:edit" in user:
                stream_text("Writing a file.\n")
                tool_call(pick("write", "edit"), {"path": "mock_note.txt", "content": "line one\nline two\n"})
            elif "rich" in user:
                stream_text(RICH_REASONING, "reasoning_content")
                stream_text(RICH)
                chunk({}, "stop")
            elif "long" in user:
                stream_text("Planning a long answer with many sections.", "reasoning_content")
                stream_text(LONG)
                chunk({}, "stop")
            else:
                stream_text("The user wants a short reply. I will be brief.", "reasoning_content")
                stream_text("Hello from the **mock** model. You said: " + user.strip()[:200])
                chunk({}, "stop")
            usage = {"id": "mock", "object": "chat.completion.chunk", "created": int(time.time()), "model": "mock-1",
                     "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}}
            self.wfile.write(f"data: {json.dumps(usage)}\n\ndata: [DONE]\n\n".encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 4555
    print(f"mock_llm listening on http://127.0.0.1:{port}/v1", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()

