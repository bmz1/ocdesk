# opencode server protocol notes

Observed against **opencode 1.19.5** (`opencode serve`), November 2026. Every
sample below is a real frame captured from `GET /event`, trimmed only where
marked. The full OpenAPI document served at `GET /doc` is checked in as
[`openapi-opencode-1.19.5.json`](openapi-opencode-1.19.5.json); diff it when
bumping the pinned version.

Samples were captured with the free `opencode` provider (until it hit
`free_tier_limit`, see *Retry* below) and the deterministic mock provider in
`dev/mock_llm.py` (see README, *Developing*).

## Process and auth

- `opencode serve --hostname 127.0.0.1 --port 0` prints exactly one line to
  stdout: `opencode server listening on http://127.0.0.1:<port>`.
- **Surprise:** `--port 0` is *not* "pick a random port". It binds **4096**
  when free and only falls back to a random port when 4096 is taken. ocdesk
  therefore parses the `listening on` line instead of assuming a port.
- With `OPENCODE_SERVER_PASSWORD` set, every route (including `/doc` and
  `/global/health`) requires HTTP Basic auth, username `opencode`. Without it the
  server prints `Warning: OPENCODE_SERVER_PASSWORD is not set; server is
  unsecured.` to stderr. Wrong or missing credentials → `401`.
- `--print-logs` sends the server's log to stderr; ocdesk keeps the last 200
  lines for crash reports.
- Health: `GET /global/health` → `200`.
- `OPENCODE_CONFIG_CONTENT` (inline JSON config) is honoured, which is how the
  mock provider is injected without touching the user's config.

## Endpoints used so far

| Purpose | Method + path | Notes |
|---|---|---|
| Health | `GET /global/health` | used by sidecar startup |
| Spec | `GET /doc` | OpenAPI 3.1, 162 operations (v1 + `/api/*` v2) |
| Create session | `POST /session` | body `{"title"?: string, ...}` → `Session` |
| List sessions | `GET /session` | |
| Prompt (async) | `POST /session/{id}/prompt_async` | → `204`, work is observed on SSE |
| Prompt (sync) | `POST /session/{id}/message` | blocks until the turn ends, returns `{info, parts}` |
| Messages | `GET /session/{id}/message` | `?limit=&before=` paging; reconcile source of truth |
| Abort | `POST /session/{id}/abort` | → `true` |
| Pending permissions | `GET /permission` | array of `PermissionRequest` |
| Reply permission | `POST /permission/{requestID}/reply` | body `{"reply": "once"\|"always"\|"reject", "message"?}` → `true` |
| Events | `GET /event` | SSE, one JSON object per `data:` line |
| Providers | `GET /config/providers` | `{providers: [...], default: {providerID: modelID}}` |

`POST /session/{id}/permissions/{permissionID}` (`permission.respond`, which
`opencode-codes` wraps) is marked **deprecated** in the spec; we will use
`POST /permission/{requestID}/reply` (`permission.reply`) in M3.

Prompt body shape (minimal, what ocdesk sends today):

```json
{"parts":[{"type":"text","text":"Say hello in one short sentence."}]}
```

Optional `model: {providerID, modelID}`, `agent`, `variant`, `noReply`, `system`,
`tools`. **Surprise:** `prompt_async` with a nonexistent model still returns
`204`; the failure only shows up on the stream as `session.error` (twice, see
below).

## Event stream (`GET /event`)

Every frame: `{"id": "evt_…", "type": "<dotted.name>", "properties": {...}}`.
The first frame after connecting is `server.connected`; `server.heartbeat`
arrives periodically (roughly every 10 s with an idle server). On startup the
server also emits a burst of ~45 `plugin.added` frames plus `catalog.updated`,
`reference.updated`, `integration.updated`; ocdesk ignores those.

### Order of a simple turn

```
message.updated (user)            → message.part.updated (text, the prompt)
session.status busy               → session.updated (agent/model filled in)
message.updated (assistant, no time.completed)
message.part.updated step-start
message.part.updated reasoning "" → message.part.delta field=text …   (reasoning tokens)
message.part.updated reasoning (final text, time.end)
message.part.updated text ""      → message.part.delta field=text …   (answer tokens)
message.part.updated text (final text, time.end)
message.part.updated step-finish (tokens, cost, reason)
message.updated (assistant, time.completed set, finish)
session.status idle               → session.idle
session.diff
```

Key insight for streaming: a part is created with `message.part.updated`
(empty `text`), then grown with `message.part.delta` frames that carry only
`{messageID, partID, field, delta}`. A final `message.part.updated` carries the
complete text, so a client that missed deltas self-heals at the end of each
part.

### session.created / session.updated

```json
{"id":"evt_1b5355633001NZeYFSqdwBDaC9","type":"session.updated","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","info":{"id":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","slug":"glowing-squid","projectID":"global","directory":"/tmp/claude-0/oc","path":"tmp/claude-0/oc","title":"m0 probe","agent":"build","model":{"id":"big-pickle","providerID":"opencode","variant":"default"},"version":"1.19.5","cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1794041533958,"updated":1794041534001}}}}
```

Note: a session's `model` is `{id, providerID, variant}`, but a message's is
`{providerID, modelID}`.

### message.updated (user, then assistant)

```json
{"id":"evt_1b5355639001DIhrHjJVs71DLP","type":"message.updated","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","info":{"id":"msg_1b53556310013zxEoGukN2axA0","role":"user","sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","time":{"created":1794041534001},"agent":"build","model":{"providerID":"opencode","modelID":"big-pickle"}}}}
```

```json
{"id":"evt_1b53621fa001oUfU5W08XWLZ0k","type":"message.updated","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","info":{"id":"msg_1b535564c001iqI4FPltLSXHvk","parentID":"msg_1b53556310013zxEoGukN2axA0","role":"assistant","mode":"build","agent":"build","path":{"cwd":"/tmp/claude-0/oc","root":"/"},"cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"modelID":"big-pickle","providerID":"opencode","time":{"created":1794041534028,"completed":1794041586170},"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM"}}}
```

The assistant message is emitted several times; `time.completed` appears on the
last one. Message ids sort chronologically (`msg_` + time-based prefix).

### message.part.updated (text / reasoning / step parts)

```json
{"id":"evt_1b53ba0220010X015NlN9GiOZ3","type":"message.part.updated","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","part":{"id":"prt_1b53ba022001kQ0BJuDMNhybRl","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","type":"reasoning","text":"","time":{"start":1794041946146}},"time":1794041946146}}
```

```json
{"id":"evt_1b53baff5001ryPAlMMr8YGz5Z","type":"message.part.updated","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","part":{"id":"prt_1b53baff5001ksrrSYpH3D4yiy","reason":"tool-calls","snapshot":"444a8fa98e219b9ee8585973bba9425676aba452","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","type":"step-finish","tokens":{"total":30,"input":10,"output":20,"reasoning":0,"cache":{"write":0,"read":0}},"cost":0},"time":1794041950197}}
```

### message.part.delta

```json
{"id":"evt_1b53ba0240013rDsTj0YiYTRsr","type":"message.part.delta","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T","partID":"prt_1b53ba022001kQ0BJuDMNhybRl","field":"text","delta":"I "}}
```

Only `field: "text"` has been observed (for both `text` and `reasoning` parts).

### Tool parts: pending → running → completed

The same part id is re-sent with a growing `state`:

```json
{"id":"evt_1b53ba113001D3PRbmVWz5AhGq","type":"message.part.updated","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","part":{"id":"prt_1b53ba112001BmPIl2ptuvjQID","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","type":"tool","tool":"bash","callID":"call_1794041946383","state":{"status":"pending","input":{},"raw":""}},"time":1794041946387}}
```

```json
{"id":"evt_1b53ba1ec001PADa39uJT9ugdW","type":"message.part.updated","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","part":{"type":"tool","tool":"bash","callID":"call_1794041946383","state":{"status":"running","input":{"command":"echo hello-from-bash && ls | head -5","description":"Say hello and list files"},"time":{"start":1794041946604}},"id":"prt_1b53ba112001BmPIl2ptuvjQID","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T"},"time":1794041946604}}
```

```json
{"id":"evt_1b53bafe7001E5iRxsYAMfnZtm","type":"message.part.updated","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","part":{"type":"tool","tool":"bash","callID":"call_1794041946383","state":{"status":"completed","input":{"command":"echo hello-from-bash && ls | head -5","description":"Say hello and list files"},"output":"hello-from-bash\nREADME.md\n","metadata":{"output":"hello-from-bash\nREADME.md\n","exit":0,"truncated":false},"title":"echo hello-from-bash && ls | head -5","time":{"start":1794041946604,"end":1794041950183}},"id":"prt_1b53ba112001BmPIl2ptuvjQID","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","messageID":"msg_1b53b9dcc0011EufSrm3wpe73T"},"time":1794041950183}}
```

The tool stays `running` while it waits for a permission reply. A
tool-calling step ends with `step-finish` `reason: "tool-calls"`, and the agent
then starts a **new assistant message** for the follow-up step.

Write tool, completed (the file itself is announced separately by
`file.edited`):

```json
{"id":"evt_1b5492f20001Rf55vEuOX64Dyo","type":"message.part.updated","properties":{"sessionID":"ses_e4ab6dd64ffeIB5jYt08pI5Opa","part":{"type":"tool","tool":"write","callID":"call_1794042831887","state":{"status":"completed","input":{"filePath":"mock_note.txt","content":"line one\nline two\n"},"output":"Wrote file successfully.","metadata":{"diagnostics":{},"filepath":"/tmp/claude-0/oc/proj/mock_note.txt","exists":false,"truncated":false},"title":"mock_note.txt","time":{"start":1794042832047,"end":1794042834720}},"id":"prt_1b5492411001wYIpZFKSuKEL60","sessionID":"ses_e4ab6dd64ffeIB5jYt08pI5Opa","messageID":"msg_1b5492333001PoYOLkMbUAT6xH"}}}
```

```json
{"id":"evt_1b5492f1c001bKd58srlFeLg3x","type":"file.edited","properties":{"file":"/tmp/claude-0/oc/proj/mock_note.txt"}}
```

### Permissions

Only asked when the config says `"ask"` (the default `build` agent allows bash
and edits without asking). `dev/mock-opencode.json` sets
`"permission": {"bash": "ask", "edit": "ask"}`.

```json
{"id":"evt_1b53ba229002ZnvWKaMNll0mDY","type":"permission.asked","properties":{"id":"per_1b53ba229001ncHFJdxYRaTUPn","sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","permission":"bash","patterns":["echo hello-from-bash","ls","head -5"],"metadata":{"command":"echo hello-from-bash && ls | head -5"},"always":["echo *","ls *","head *"],"tool":{"messageID":"msg_1b53b9dcc0011EufSrm3wpe73T","callID":"call_1794041946383"}}}
```

Edit permissions carry the full unified diff in `metadata.diff`, which is
useful for M3:

```json
{"id":"evt_1b54924b7002vN4KykcmXr32e2","type":"permission.asked","properties":{"id":"per_1b54924b7001e0wwi0xbBnH5Md","sessionID":"ses_e4ab6dd64ffeIB5jYt08pI5Opa","permission":"edit","patterns":["mock_note.txt"],"metadata":{"filepath":"/tmp/claude-0/oc/proj/mock_note.txt","diff":"Index: /tmp/claude-0/oc/proj/mock_note.txt\n===================================================================\n--- /tmp/claude-0/oc/proj/mock_note.txt\n+++ /tmp/claude-0/oc/proj/mock_note.txt\n@@ -0,0 +1,2 @@\n+line one\n+line two\n"},"always":["*"],"tool":{"messageID":"msg_1b5492333001PoYOLkMbUAT6xH","callID":"call_1794042831887"}}}
```

`tool.callID` links the request to the tool part. After
`POST /permission/{id}/reply {"reply":"once"}`:

```json
{"id":"evt_1b53bafd7001rwlQPW5yzBuJtD","type":"permission.replied","properties":{"sessionID":"ses_e4ac46421ffeEx5YIT69lZ4Qf1","requestID":"per_1b53ba229001ncHFJdxYRaTUPn","reply":"once"}}
```

There is also a `permission.v2.asked` / `/api/session/{id}/permission/…/reply`
pair; 1.19.5 did not emit it on the v1 `/event` stream.

### session.status / session.idle

```json
{"id":"evt_1b53556470012PejTumBAxT7v7","type":"session.status","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","status":{"type":"busy"}}}
{"id":"evt_1b53621ff0018p4sv2LjEdUqdp","type":"session.status","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","status":{"type":"idle"}}}
{"id":"evt_1b5362200001GTZjRKr7vocrjd","type":"session.idle","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM"}}
```

`busy` is sent several times per turn (once per step). `GET /session/status`
returns only non-idle sessions (`{}` when everything is idle).

### Retry (provider throttling)

```json
{"id":"evt_1b53559f9001y6bfQV693kR0Gp","type":"session.status","properties":{"sessionID":"ses_e4acaa9f9ffe1QOLtGLxcwv0CM","status":{"type":"retry","attempt":1,"message":"Free usage exceeded, subscribe to Go","action":{"reason":"free_tier_limit","provider":"opencode","title":"Free limit reached","message":"Subscribe to OpenCode Go for reliable access to the best open-source models for $10/month.","label":"subscribe","link":"https://opencode.ai/go"},"next":1794098198969}}}
```

`next` is an epoch-ms timestamp for the next attempt (here ~16 h away), so the
session sits in `retry` and the UI must offer Abort. `action.link` is worth
rendering as a button.

### Errors

Abort mid-stream produces a `session.error` and stamps the assistant message:

```json
{"id":"evt_1b549450000119rnHGCOmtg7yi","type":"session.error","properties":{"sessionID":"ses_e4ab6c12bffeRbJ4gn4DY2qVk7","error":{"name":"MessageAbortedError","data":{"message":"Aborted"}}}}
```

```json
{"id":"evt_1b549452e001uJLKwfMnzzp3Lr","type":"message.updated","properties":{"sessionID":"ses_e4ab6c12bffeRbJ4gn4DY2qVk7","info":{"id":"msg_1b5493f11001UImW8Ru2BdCa5x","role":"assistant","…":"…","time":{"created":1794042838801,"completed":1794042840366},"error":{"name":"MessageAbortedError","data":{"message":"Aborted"}}}}}
```

A bad model produces **two** `session.error` frames for one prompt: a short one,
then one whose `data.message` contains a full stack trace (trimmed here):

```json
{"id":"evt_1b549457d001z9BnQMkR2R2pt7","type":"session.error","properties":{"sessionID":"ses_e4ab6babfffeZa4S7QOIl5zIq1","error":{"name":"UnknownError","data":{"message":"Model not found: mock/does-not-exist."}}}}
{"id":"evt_1b549457f001ll8NmaUrJVXEQV","type":"session.error","properties":{"sessionID":"ses_e4ab6babfffeZa4S7QOIl5zIq1","error":{"name":"UnknownError","data":{"message":"ProviderModelNotFoundError: Model not found: mock/does-not-exist.\n    at <anonymous> (/$bunfs/root/chunk-vhbb8cv6.js:2:64699)\n    …"}}}}
```

The UI should show the first line of `data.message` and de-duplicate by session.

**Ordering on abort** (seen from ocdesk's Abort button): `session.error`
(`MessageAbortedError`) and `session.idle` arrive on the stream *before* the
`POST /session/{id}/abort` response (`true`) returns to the caller. Don't
wait for the abort response to clear the busy state.

### Other frames seen, ignored for now

`server.connected`, `server.heartbeat`, `plugin.added`, `catalog.updated`,
`reference.updated`, `integration.updated`, `session.diff` (`{"diff": []}`; may
feed the M3 changed-files summary).

## opencode-codes (1.19.5) notes

- Types are generated from the spec; `Event` is a tagged enum with ~110
  variants. ocdesk normalizes the ~15 it needs into `api::events::Event`.
- `EventStream` reconnects with exponential backoff and yields
  `StreamEvent::Connected` on every (re)open. That's the hook for M1 reconcile.
- `OpencodeClient::event_stream` applies no request timeout (correct for SSE);
  normal requests use the builder's timeout.
- `ManagedServer` was not used: it inherits stderr, hides the pid, and does not
  report crashes. See `src/sidecar.rs`.
