# opencode 2.x protocol notes

Observed against **opencode 2.2.6** (`@opencode/cli` on npm; GitHub tags
`v2.x` of `anomalyco/opencode`), November 2026. Every sample is a real frame
from a running server, trimmed only where marked. The server's own OpenAPI
document (`GET /openapi.json`, 170 operations, all under `/api/*`) is checked
in as [`openapi-opencode-2.2.6.json`](openapi-opencode-2.2.6.json); diff it
when bumping the pinned version. The captures used by the unit tests are in
[`tests/fixtures/`](../tests/fixtures).

ocdesk supports opencode 2.x only. 1.x (`opencode-ai` on npm, which is also
what `curl https://opencode.ai/install` installs) is rejected at startup.

## Install and process

- `npm i -g @opencode/cli` installs two names for the same binary: `opencode`
  and `opencode2`. ocdesk prefers `opencode2`, so a leftover 1.x `opencode`
  on `PATH` can't shadow it. `OCDESK_OPENCODE=/path/to/opencode` overrides.
- `opencode --version` prints `opencode v2.2.6`.
- `opencode serve --hostname 127.0.0.1 --port 0` binds a random free port and
  prints one line to stdout: `server listening on http://127.0.0.1:<port>`.
  (`serve` also has `--stdio` and `--service`; ocdesk uses neither.)
- `--print-logs` is accepted but prints nothing for `serve` at info level.
- Auth: with `OPENCODE_SERVER_PASSWORD` set, `/api/*` and `/openapi.json`
  need HTTP Basic auth, user `opencode`; otherwise `401`.
- Health: `GET /api/info` (authenticated) →
  `{"version":"2.2.6","pid":…,"urls":[…],"capabilities":{…}}`.
  `/global/health` and `/doc` are **not** API routes in 2.x: every unknown
  path serves the bundled web UI's `index.html` with `200`.
- Config: providers come from `$XDG_CONFIG_HOME/opencode/opencode.json` (and a
  project `opencode.json`). The dev mock uses `XDG_CONFIG_HOME=dev/xdg`.
- `GET /api/model/default?location[directory]=…` → the default model for a folder.

## Routes ocdesk uses

| Need | Route | Notes |
|---|---|---|
| Health/version | `GET /api/info` | |
| List sessions (all folders) | `GET /api/session?limit=&order=desc&cursor=` | `{data: [Session.Info], cursor: {next, previous}}`; also `directory`, `project`, `archived`, `parentID`, `search` filters |
| Create | `POST /api/session` `{location: {directory}}` | → `{data: Session.Info}`; pinned to that directory |
| Prompt | `POST /api/session/{id}/prompt` `{text}` | → `{data: Session.Inbox.User}`; also `files`, `agents`, `skills`, `delivery: steer\|queue`, `resume` |
| History | `GET /api/session/{id}/message?order=asc&limit=&cursor=` | projection, see *Messages* |
| Stop | `POST /api/session/{id}/interrupt` | → `{"interrupted": true}` |
| Busy sessions | `GET /api/session/active` | `{data: {"ses_…": {type: "running"}}}` |
| Rename | `PATCH /api/session/{id}` `{title}` | → 204, then `session.renamed`; also `archived`, `metadata`, `permissions` |
| Delete | `DELETE /api/session/{id}` | → 204, then `session.deleted` |
| Validate a folder | `GET /api/location?location[directory]=…` | `{directory, project: {id, directory, canonical}}` |
| Events (all folders) | `GET /api/event` | one SSE stream; `?types=` filter exists |
| Permissions | `GET /api/session/{id}/permission`, `POST …/permission/{id}/reply {decision}` | 204 |
| Models | `GET /api/model?location[directory]=…&types=chat` | `{data: [Model.Info]}`: `id`, `providerID`, `name`, `enabled`, `variants: [{id, settings}]`; includes the built-in `opencode` provider's free models |
| Default model | `GET /api/model/default?location[directory]=…` | `{data: Model.Info}` |
| Agents | `GET /api/agent?location[directory]=…` | `{data: [{id, name, mode: primary\|subagent\|all, hidden, model?}]}`; `build`/`plan` primary, `general`/`explore` subagents, `title`/`summary`/`compaction` hidden |
| Switch model / agent | `POST /api/session/{id}/model {model: {providerID, id, variant?}}`, `POST …/agent {agent}` | 204, then `session.model.selected` / `session.agent.selected`; `Session.Info` gains `model` (`variant: "default"` when none) and `agent` |
| File search | `GET /api/fs/find?location[directory]=…&query=&limit=` | `{data: [{path, type: file\|directory}]}`, paths relative, directories end in `/`, the root is `"/"` |
| Latest-turn file changes | `GET /api/session/{id}/diff?context=3` | `{data: [{file, patch, status, additions, deletions, more?}]}`; omitting `from` selects the latest user turn |

`Session.Info`:

```json
{"id":"ses_e4a6d0131ffeIP79svsw8WMMvA","projectID":"ed55…","cost":0,"tokens":{…},"outcome":"succeeded",
 "time":{"created":1794047672020,"updated":1794047674114,"busy":1794047672062,"idle":1794047673601},
 "title":"Renamed in 2.x","location":{"directory":"/tmp/claude-0/demo2/payments-service"}}
```

`title` is absent until the server generates one (shortly after the first
prompt) or someone renames the session.

## Event stream (`GET /api/event`)

SSE `data:` lines with one JSON object each, plus `: heartbeat` comment lines
about every 10 s. First frame: `{"type":"server.connected"}`.

Envelope:

```json
{"id":"evt_…","created":1794047672128,"type":"session.step.started",
 "location":{"directory":"/tmp/claude-0/demo2/payments-service"},
 "data":{ … },
 "durable":{"aggregateID":"ses_…","seq":5,"version":1}}
```

- `durable` is present on durable events only; `seq` is per session. Deltas
  are live-only.
- `location` says which folder the event belongs to (missing on some
  session-level events such as `session.execution.*`).
- Text/reasoning blocks are keyed by `ordinal` (per kind, per assistant
  message); tools by `id` (the call id). Every `*.ended` repeats the full
  text, so missed deltas self-heal at the end of each block.

### A turn with a tool call, then an interrupted turn (captured)

```
session.created              (seq 0)  data: {sessionID, slug, version, projectID, location}   ← no title, no times
session.inbox.enqueued       (seq 1)  data: {inboxID, item: {type: "user", payload: {text}, delivery}}
session.execution.started    (seq 2)  ← busy
session.instructions.updated
session.inbox.delivered      (seq 4)  data: {inboxID}
session.step.started         (seq 5)  data: {assistantMessageID, agent, model, snapshot}
session.reasoning.started    (seq 6)  data: {ordinal: 0}
session.reasoning.delta      (live)   data: {ordinal, delta}
session.usage.updated
session.renamed              (seq 8)  data: {title}                                          ← generated title
session.text.started         (seq 9)  data: {ordinal: 0}
session.text.delta           (live)
session.tool.input.started   (seq 10) data: {id, name}
session.reasoning.ended      (seq 11) data: {ordinal, text: <full>}
session.tool.input.ended     (seq 12) data: {id, text: <json>}
session.tool.called          (seq 13) data: {id, input: {...}, executed: false}
session.text.ended           (seq 14) data: {ordinal, text: <full>}
session.step.streamed        (seq 15)
shell.created / session.tool.progress / shell.exited            (the shell tool's process)
session.tool.success         (seq 16) data: {id, content: [{type: "text", text}], metadata: {exit, truncated}}
session.step.ended           (seq 17) data: {finish: "tool-calls", tokens, cost}
… second step answers with the tool output, finish: "stop" …
session.execution.succeeded  (seq 25) ← idle
…
session.step.failed                   data: {error: {type: "aborted", message: "Step interrupted"}}
session.execution.interrupted         data: {reason: "user"}                                 ← idle
```

Samples:

```json
{"id":"evt_1b593a8a7001zGTyuSQ6hEg6Jv","created":1794047715495,"type":"session.inbox.enqueued","location":{"directory":"/tmp/claude-0/demo2/payments-service"},"data":{"inboxID":"msg_1b593a8a600142fsZBeZgFbpqo","sessionID":"ses_e4a6c5780ffen246hqVhrqGI4E","item":{"type":"user","payload":{"text":"tool:bash please"},"delivery":"steer"}},"durable":{"aggregateID":"ses_e4a6c5780ffen246hqVhrqGI4E","seq":1,"version":1}}
```

```json
{"id":"evt_1b593a9c5001eRnHs6a2zCqhX6","created":1794047715781,"type":"session.text.delta","location":{"directory":"/tmp/claude-0/demo2/payments-service"},"data":{"sessionID":"ses_e4a6c5780ffen246hqVhrqGI4E","assistantMessageID":"msg_1b593a8c7001ZCrybzFU5eAiN8","ordinal":0,"delta":"Running a command.\n"}}
```

```json
{"id":"evt_1b593aabd001dgwOMvPKLfujQs","created":1794047716029,"type":"session.tool.called","location":{"directory":"/tmp/claude-0/demo2/payments-service"},"data":{"sessionID":"ses_e4a6c5780ffen246hqVhrqGI4E","assistantMessageID":"msg_1b593a8c7001ZCrybzFU5eAiN8","id":"call_1794047715780","input":{"command":"echo hello-from-bash && ls | head -5","description":"Say hello and list files"},"executed":false},"durable":{"aggregateID":"ses_e4a6c5780ffen246hqVhrqGI4E","seq":13,"version":1}}
```

```json
{"id":"evt_1b593b69b0015Bt9tELpdHhPGj","created":1794047719067,"type":"session.execution.interrupted","data":{"sessionID":"ses_e4a6c5780ffen246hqVhrqGI4E","reason":"user"},"durable":{"aggregateID":"ses_e4a6c5780ffen246hqVhrqGI4E","seq":36,"version":1}}
```

`session.execution.failed` carries `{error: {type, message}}`;
`session.execution.interrupted` reasons are `user`, `shutdown`, `superseded`,
`inactivity`. Other events in the spec that ocdesk doesn't use yet include
`permission.asked`/`replied` (M3), `form.*` (questions), `session.compaction.*`,
`session.retry.scheduled`, `session.archived`, `session.moved`,
`session.revert.*`, `shell.*`, `pty.*`, `vcs.branch.updated`.

### Messages (projection)

`GET /api/session/{id}/message?order=asc`:

```json
{"id":"msg_…","time":{"created":…},"text":"tool:bash please","type":"user"}
{"id":"msg_…","type":"assistant","time":{"created":…,"streamed":…,"completed":…},
 "agent":"build","model":{"id":"mock-1","providerID":"mock"},
 "content":[
   {"type":"reasoning","text":"I should run a command.","time":{…}},
   {"type":"text","text":"Running a command.\n"},
   {"type":"tool","id":"call_1794047672348","name":"shell","executed":false,
    "state":{"status":"completed","input":{"command":"…","description":"…"},
             "content":[{"type":"text","text":"hello-from-bash\nREADME.md\nmain.rs\n"}],
             "metadata":{"status":"completed","truncated":false,"exit":0}},
    "time":{"created":…,"ran":…,"completed":…}}],
 "finish":"tool-calls","cost":0,"tokens":{…}}
{"id":"msg_…","type":"assistant", … ,"finish":"error","error":{"type":"aborted","message":"Step interrupted"}}
{"id":"msg_…","time":{"created":…},"type":"idle","outcome":"interrupted"}
```

Text and reasoning content has **no ids**: ocdesk numbers them per kind, in
order, which matches the stream's `ordinal`. `idle` entries mark the end of a
turn (`outcome`: `succeeded`, `interrupted`, …). Other types (`synthetic`,
`system`, `shell`, `compaction`, `agent-switched`, `model-switched`) are skipped
for now. A unit test replays the captured live events through the store and
checks the result equals this projection.

## Surprises

- The tool is called **`shell`** in 2.x (1.x: `bash`). A model calling `bash`
  still works: the server runs it as `shell` and appends a note telling the
  model to call `shell`.
- `/global/health` and `/doc` don't exist; unknown paths return the web UI
  with `200`, so a health check must hit an `/api/*` route.
- The REST projection can briefly lag the stream right after a turn ends, so
  reconcile keeps local text that strictly extends the snapshot's.
- `session.created` has no title and no timestamps (use the envelope's
  `created`); the title arrives later via `session.renamed`.


## M3 client behavior

`permission.asked.data` is a `Permission.Request`: `{id, sessionID, action,
resources, save?, message?, metadata?, source?}`. `permission.replied.data`
uses `{sessionID, requestID, reply}`, not `id`. Replies send
`{decision: "once" | "always" | "reject"}` and accept the empty 204 response.
Pending requests belong to sessions independently of loaded message history.
After reconnect/selection they are restored from the session permission list;
a per-session revision prevents an in-flight REST snapshot from overwriting
newer asked/replied events. Outdated reads are re-fetched. Sidecar generations
and stream epochs fence off stale permission callbacks.

Tool progress carries metadata only; it is not output text. Terminal success
and failure metadata replaces progress metadata, matching the message REST
projection. Failed tools can have both `content` and `error`. The UI retains
both, together with exit codes, truncation markers, and optional edit diffs.

Session diffs come from the server's snapshot comparison, never from guessed
old/new tool arguments. `context=3` bounds unchanged context. `more: true`
means opencode truncated the patch; Copy cannot restore data the server omitted.
The UI also bounds its own preview, while Copy retains the returned patch.
`form.*` question events remain outside this milestone.

## M4 client behavior

- **Prompt attachments.** `files: [{uri: "file:///abs/path", name, mention:
  {start, end, text}}]` makes the server read and inline the file (the user
  message stores `data` base64 + `mime`). `agents: [{name, mention}]`
  delegates to a subagent. Mention offsets are **UTF-16 code units** (JS
  string indices); verified with a non-ASCII prefix on 2.2.6.
- **Model/agent choice.** The composer shows the selected session's
  `model`/`agent`, else the last pick ("draft", sticky across new sessions),
  else the folder default (`/api/model/default`, agent `build`). A new
  session gets the draft via the switch endpoints *before* its first prompt.
  Switching emits `model-switched` / `agent-switched` (and, for `plan`, a
  `synthetic` reminder) entries in the message projection; they're skipped
  like other non-chat types.
- **Catalog refresh.** Models/agents are re-read per folder after every
  (re)connect, after adding a folder, and (debounced 300 ms) on
  `model.updated` / `agent.updated` / `provider.updated`, which arrive in a
  burst at sidecar startup.

## M7: inbox, commands, background tasks, worktrees (probed on 2.2.6)

Captured with `dev/harness.py` + the mock model (`tool:task`, `tool:sleep`,
`slow` keywords) against a real `opencode serve`.

**Inbox: queue and steer.** Every prompt goes through a per-session inbox.
`POST …/prompt {text, delivery: "steer"|"queue"}` → `{data: {id, sessionID,
time, type: "user", payload: {text}, delivery}}` (the inbox item; its `id`
becomes the user message id). While a turn runs:

- `steer` items are delivered at the next step boundary of the running turn
  (they become a user message mid-turn; the model sees them on its next call);
- `queue` items wait until the turn ends, then start the next turn.

Order observed: `slow` turn → steer delivered → queued delivered.

| Need | Route | Notes |
|---|---|---|
| Pending items | `GET /api/session/{id}/inbox` | `{data: [item]}`, oldest first |
| Cancel | `DELETE /api/session/{id}/inbox/{itemID}` | 204, then `session.inbox.cancelled {sessionID, inboxID}` |
| Re-target | `PATCH …/inbox/{itemID} {delivery}` | 204, then `session.inbox.delivery.changed {sessionID, inboxID, delivery}` |

Events: `session.inbox.enqueued {sessionID, inboxID, item: {type, payload, delivery}}`,
`session.inbox.delivered {sessionID, inboxID}`. An idle session's prompt is
enqueued and delivered within ~20 ms. Item `type` is `user`, or `compaction`
for `POST …/compact`.

**Compaction.** `POST /api/session/{id}/compact {}` → an inbox item
`{type: "compaction", delivery: "steer"}`. The message projection gains
`{type: "compaction", status: "completed"|"failed", reason: "manual"|…,
error?, tokens, cost}`. (With the mock model it fails: "Compaction summary did
not match the required template", which is a fine error-path fixture.)

**Commands.** `GET /api/command?location[directory]=…` → `{data: [{name,
description}]}`; built-ins on 2.2.6: `init`, `review`, `goal`.
`POST /api/session/{id}/command {name, text, files?, agents?, skills?, delivery?}`
→ 204; the expanded template becomes the user message (e.g. `/review` →
"You are a code reviewer…"). Unknown name → 404 `CommandNotFoundError`.

**Skills.** `GET /api/skill?location[directory]=…` → `{data: [{id, name,
description, path, content}]}` (built-in `opencode` skill). Two ways to use
one: a `skills: [{id, mention?}]` prompt attachment, or
`POST /api/experimental/session/{id}/skill {id}` → 204 +
`session.skill.activated {sessionID, id, name, text}` and a `{type: "skill"}`
entry in the projection.

**MCP.** `GET /api/mcp?location[directory]=…` → `{data: [{name, title?,
integrationID?, status: {status: connected|pending|disabled|failed|needs_auth,
error?}}]}`. Config key is `mcp.servers.<name>` (2.x). Connect/disconnect:
`POST /api/experimental/mcp/{server}/connect|disconnect`. Event
`mcp.status.changed`.

**Subagents.** The tool is `subagent` (`{agent, description, prompt,
background?}`, `background: true` returns at once). The child is a normal
session with `parentID` (`GET /api/session?parentID=<id>`) and `agent`;
`session.created` carries `parentID`, `title` (= description) and `agent`.
The parent's tool block: `state.metadata.sessionID` = child id,
`state.metadata.status`, `state.input = {agent, description, prompt}`; result
text is wrapped in `<subagent sessionID=… state=completed>…</subagent>`. The
child's own events stream on `/api/event` like any session's.

**Shells and jobs.** A long `shell` tool call is moved to the background by
the server (tool result: "Command moved to the background (shell ID: sh_…)").
`GET /api/shell?location[directory]=…` → `{data: [Shell.Info]}`: `{id, status:
running|exited|timeout|killed, command, cwd, shell, file, pid, exit?, signal?,
metadata: {sessionID}, time: {started, completed?}}`.
`GET /api/shell/{id}/output?cursor=&limit=` → `{data: {output, cursor, size,
truncated}}`. `DELETE /api/shell/{id}` kills it. Events: `shell.created {info}`,
`shell.exited {id, status, exit?}`, `shell.deleted {id}`.
`GET /api/experimental/job` → `{data: [Job.Shell | Job.Subagent | Job.Image]}`
with `kind`, `status: running|completed|error|cancelled`, `sessionID`, and
`shellID`/`command` or `childSessionID`/`agent`/`description`.

**VCS and worktrees.** `GET /api/vcs?location[directory]=…` → `{data:
{provider: "git", branch: {current, default}}}` (`branch: {}` until resolved;
event `vcs.branch.updated {branch}` with the event's `location`).
`GET /api/vcs/branch` → `{data: ["main", …]}`. Worktrees are per project
(`projectID` from `GET /api/location` → `project.id`):

| Need | Route | Notes |
|---|---|---|
| List | `GET /api/worktree?projectID=` | `[{directory, strategy?}]`; includes the main checkout (no `strategy`) |
| Create | `POST /api/worktree {projectID, name?, branch?, from?, directory?}` | `{directory}`, default under `$XDG_DATA_HOME/opencode/worktree/<hash>/<name>` |
| Remove | `DELETE /api/worktree {projectID, directory, force}` | 400 with `forceRequired` when dirty |
| Move a session | `POST /api/session/{id}/move {directory}` | 204; the session's `location.directory` changes |

Events: `worktree.updated {projectID}`, `worktree.resolved`. A worktree
location reports its own `project.id` with `canonical` = the main checkout.
