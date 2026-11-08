# Workflow builder: plan

_Status: proposal, 2026-11-08. Nothing implemented yet._

A **Workflows** tab where you put agents together into a saved, reusable
pipeline and run it later against a folder. Like n8n, but local, and every
node is an opencode agent + model + prompt. Example:

```
            ┌──────────┐     ┌──────┐     ┌────────────┐
 task ────▶ │ architect│ ──▶ │ swe  │ ──▶ │ security   │ ─┐
            │ opus/high│     │sonnet│  ├▶ │ reviewer   │ ─┼─▶ synthesizer ──▶ result
            └──────────┘     └──────┘  └▶ │ qa         │ ─┘   (verdict: ship / fix)
                                          └────────────┘            │
                                   ▲                                │ fix (max 2×)
                                   └────────────────────────────────┘
```

ocdesk stays a client: **each node run is an ordinary opencode session**.
All tools, permissions, subagents and providers stay in opencode. The workflow
engine only creates sessions, sends prompts, waits for executions to finish
and passes text between them. That means everything we already have (tool
cards, approvals, Needs-you, Changes, Terminal) works for every node for free:
clicking a node opens its session in the normal conversation view.

## 1. What the server gives us (2.2.6, checked in the OpenAPI doc)

| Need | How | Notes |
|---|---|---|
| One run of one node | `POST /api/session {location}` | A top-level session per node run; not a `subagent` child (we want our own model/agent per node, and parallel nodes) |
| Agent per node | `POST /api/session/{id}/agent {agent}` before the first prompt | Same path the composer already uses for drafts |
| Model + variant per node | `POST /api/session/{id}/model {model: {providerID, id, variant}}` | Same |
| Node prompt | `POST /api/session/{id}/prompt {text, files, skills, agents}` | There is **no `system` field** on prompt; see §4 |
| Tag sessions as workflow-owned | `PATCH /api/session/{id} {metadata, title}` | `metadata: {ocdesk: {run, node, iteration}}`; lets the sidebar hide/group them and lets a restart find them |
| Node finished | SSE `session.execution.succeeded/failed/interrupted` | Already parsed into `Event::ExecutionEnded { outcome }` in `src/api/events.rs` |
| Node output | `GET /api/session/{id}/message` → last assistant text | `api::messages` already exists |
| Node blocked on a human | `permission.asked`, `form.*` for that session | Existing plumbing; run shows *waiting on you* |
| Cancel | `POST /api/session/{id}/interrupt` per running node | |
| Isolation for parallel writers | `create_worktree` / `move_session` | Already wrapped in `src/api/mod.rs` |
| Custom agents with their own system prompt | `.opencode/agent/*.md` or `agent.<id>` in `opencode.json`; `Agent.Info` has `system`, `model`, `permissions`, `steps` | Read via `GET /api/agent`; writing via `PATCH /api/experimental/config` is experimental |

No new server capability is required for v1.

## 2. Data model

One DAG model; the editors (§5) are just views over it.

```rust
// src/workflow/model.rs  (serde, versioned)
struct Workflow {
    version: u32,                 // schema version, migrate on load
    id: String,                   // slug, also the file name
    name: String,
    description: String,
    inputs: Vec<InputDef>,        // e.g. `task: text`, `files: paths`
    nodes: Vec<Node>,
    edges: Vec<Edge>,             // from → to (+ optional condition)
    defaults: NodeDefaults,       // model/agent/permission mode fallback
    limits: Limits,               // max_parallel, max_iterations, run timeout
}

struct Node {
    id: String,                   // "architect"; referenced by templates
    label: String,
    kind: NodeKind,
    pos: Option<(f32, f32)>,      // canvas only; ignored by the stage editor
}

enum NodeKind {
    Agent {
        agent: String,            // opencode agent id: build, plan, or custom
        model: Option<ModelRef>,  // None → workflow default → folder default
        role: String,             // per-node instructions (see §4)
        prompt: String,           // template, see §3
        permission: PermissionMode,     // e.g. reviewers read-only
        isolation: Isolation,     // SharedFolder | Worktree | ParentWorktree
        on_fail: FailPolicy,      // FailRun | Continue | Retry(n)
        output: OutputSpec,       // Text | Verdict { options: ["ship","fix"] }
    },
    Gate { message: String },     // human approval in the run view
    Output { template: String },  // final report
}

struct Edge { from: String, to: String, when: Option<Condition> }
enum Condition { Verdict(String), Always }
```

- **Fan-out** = one node with several outgoing edges. **Join** = a node with
  several incoming edges; it starts when all its non-skipped inputs are done.
  The synthesizer is just an `Agent` node with N inputs.
- **Loops** (synth → swe on `fix`) are the one non-DAG thing. Allowed only as
  a back edge with a `Verdict` condition, bounded by `limits.max_iterations`.
  Validation rejects any other cycle. Each iteration is a new session per node
  (`iteration` in metadata), so history stays readable.
- `Workflow` validation (pure, unit-tested): unique ids, edges reference real
  nodes, exactly one entry, no unbounded cycles, every template reference
  resolves to an upstream node, verdict conditions match the source's options.

**Where workflows live**

- Project: `<folder>/.ocdesk/workflows/<id>.json` — checked into git, shared
  with the team. Shown first.
- Personal: `<config dir>/ocdesk/workflows/<id>.json` — usable in any folder.
- Built-in templates (embedded): *Architect → SWE → Review/QA/Security →
  Synthesizer*, *Plan → Implement → Test*, *Parallel bug hunt*.

**Runs** are persisted to `<state dir>/ocdesk/runs/<run-id>.json`
(workflow snapshot + inputs + per-node `{status, session_ids, output_path,
started, ended, error}`), written after every state change, so a crash or
sidecar restart can resume (§6).

## 3. Passing work between nodes

Templates use `{{…}}` with a tiny, purpose-built renderer (no new crate):

| Placeholder | Value |
|---|---|
| `{{input.task}}` | Run input |
| `{{nodes.architect.output}}` | That node's final assistant text (latest iteration) |
| `{{inputs}}` | All incoming outputs, each under a `## <label>` heading (the default for joins) |
| `{{nodes.synth.verdict}}` | Parsed verdict |
| `{{iteration}}` | Loop count, for "address the feedback below" prompts |

**Large outputs.** Inlining a 20k-token architect spec into four prompts is
expensive and noisy. Each node output is also written to
`<folder>/.ocdesk/runs/<run>/<node>.md` (gitignored), and by default a
downstream node gets it as a **file attachment** (`files: [{uri}]`, which the
server already inlines and shows as a chip) plus a short pointer in the text.
Per-edge toggle: *attach* (default) / *inline* / *summary only*.

**Verdicts** (for conditions). The node's `output: Verdict` adds an instruction
to end with a fenced block:

````
```verdict
{"verdict": "fix", "reasons": ["SQL injection in handlers/user.rs"]}
```
````

Parsed from the last assistant message. Missing/invalid → one automatic
steer prompt ("Reply with the verdict block only"), then the node fails.

## 4. Per-node role / system prompt

opencode's prompt API has no system field. Options:

1. **Role prepended to the first user message** (v1). Works with any agent,
   no config writes. Downside: it's visible in the transcript (fine: it's
   honest, and the user can see what each node was told).
2. **Pick a custom agent** that already carries a system prompt
   (`.opencode/agent/security.md`). Supported in v1 for free, since node
   `agent` is any id from `GET /api/agent`, including `mode: subagent` ones.
3. **Generate agents** from the workflow (write `.opencode/agent/wf-<id>-<node>.md`
   with `system`, `model`, `permissions`), so the role is a real system prompt
   and permissions are enforced by opencode. v2, behind a toggle: it writes
   into the user's project and the agent catalog then needs a refresh
   (`agent.updated` already triggers one).

## 5. UI

A new top-level mode next to the conversation (not the right panel: a graph
needs the width). Sidebar gets a **Workflows** section under each folder;
`Mod+K` gets *Run workflow…* and *New workflow*.

**Editor, v1: stage editor.** Rows are stages run top to bottom; cards in a
row run in parallel; a stage's inputs are the previous stage's outputs. This
covers architect → swe → [security ∥ review ∥ qa] → synthesizer exactly, is
fast to build with existing gpui-component pieces (cards, selects, inputs),
and is keyboard-friendly. A node card: label, agent select, model+variant
select (the composer's pickers, reused), permission mode, isolation, and a
role/prompt editor with `{{` completion of node ids. A loop is a "on verdict
*fix* → back to stage *swe*" control on the synthesizer card.

**Editor, v2: canvas.** Free-form nodes and edges for DAGs the stage view
can't express (skip connections, uneven joins). GPUI has no node-graph
widget: absolutely positioned cards + bezier edges painted in a `canvas()`
element, drag to move/connect, pan/zoom. Same `Workflow` model; the stage
view becomes a layout of it (and refuses to open graphs it can't show).

**Run view.** The same graph, read-only, each node showing status
(queued / running / waiting on you / done / failed / skipped), model, elapsed
time, cost and tokens (from `Session.Info`). Click → opens that node's session
in the conversation view, with a breadcrumb back to the run. Run header:
Stop (interrupts every running node), Retry failed node, Resume, open the
final output. Approvals from any node go through the existing Needs-you list
and notifications; the run shows which node is blocked.

**Start dialog.** Pick folder, fill inputs (task text, `@` files reuse the
composer's mention picker), optional per-run model override, *Run*.
"Run later" = saved workflow + **Run again** from the run history (same
inputs, editable), plus *Queue* (runs start one after another per folder).
Scheduled runs (cron-like) only while ocdesk is open are a v3 nicety; anything
headless belongs in a CLI (see open questions).

## 6. Engine

```
src/workflow/
  model.rs      Workflow, validation, (de)serialization + schema migration
  template.rs   {{…}} renderer + reference checking
  verdict.rs    verdict block parsing
  store.rs      load/save workflows (project + personal), runs on disk
  engine.rs     Run state machine, scheduling
  executor.rs   trait NodeExecutor + the opencode implementation
src/ui/workflows/{list.rs, stage_editor.rs, run_view.rs, canvas.rs}
```

- **Executor seam.** `trait NodeExecutor { start(node, prompt, files) -> SessionId;
  wait(SessionId) -> Outcome; output(SessionId) -> String; cancel(..) }`.
  The real one wraps `api::Client`; tests use a fake. The scheduler is pure
  logic over node states and is unit-tested without a server.
- **Scheduling.** A node is ready when all incoming edges are satisfied
  (source done, condition true) or skipped. Ready nodes start up to
  `max_parallel` (default 4). Nodes whose conditions fail are marked skipped,
  and skips propagate.
- **Completion** comes from the existing event pump: the controller forwards
  `ExecutionEnded` for sessions owned by a run to the engine (a
  `session_id → (run, node)` map). The engine then reads messages (REST,
  after the projection catches up: same lag caveat as reconcile, so re-read
  once if the last block is still streaming) and checks `/api/session/active`
  before declaring done, in case queued inbox items start another turn.
- **Failure.** `on_fail` per node. `FailRun` interrupts siblings and stops.
  Interrupt by the user = run *cancelled*, not failed.
- **Restart/crash.** On start, unfinished runs from disk are listed as
  *interrupted* with Resume. Resume re-reads each running node's session:
  finished → collect output; still running → re-attach; gone → re-run node.
  Sidecar generations (already in the controller) fence stale callbacks.
- **Parallel writers.** Two `build` agents editing the same checkout will
  trample each other. Default isolation: write-capable nodes in the same stage
  get their own worktree (`create_worktree`), read-only nodes (reviewer,
  security, QA with permission mode *plan*/read-only) share the folder. Joining
  worktrees back is *not* automated in v1: the synthesizer gets each branch's
  diff, and the run's final step offers the existing Changes/review UI.
  Validation warns when two write nodes share a folder in one stage.
- **Budgets.** Per-run limits: max nodes run, max iterations, wall-clock
  timeout, optional cost ceiling (summed `Session.Info.cost`); hitting one
  stops the run cleanly with a reason.
- **Sidebar noise.** Workflow sessions carry `metadata.ocdesk.run` and are
  grouped under the run instead of flooding the folder's session list.

## 7. Phases

| Phase | Scope | Done when |
|---|---|---|
| **P0 engine** | model, validation, templates, verdicts, fake-executor tests, opencode executor, run persistence; a hidden `OCDESK_WORKFLOW=<file>` hook that runs a JSON workflow headlessly | the example pipeline runs end to end against `dev/mock_llm.py` (new keywords for verdicts), `cargo test` covers scheduling, joins, skips, loops, failure, resume |
| **P1 run UI** | run view (stage layout), node → session navigation, Needs-you integration, Stop/Retry/Resume, sidebar grouping, run history | the example runs from the UI with an approval on one node; screenshot under Xvfb |
| **P2 stage editor** | create/edit/save workflows (project + personal), built-in templates, start dialog with inputs and `@` files, per-node model/agent pickers | a new workflow can be built, saved to `.ocdesk/workflows/`, rerun from history |
| **P3 isolation + polish** | worktree per write node, cost/time budgets, attach/inline/summary edges, queueing | two parallel `build` nodes run without conflicts |
| **P4 canvas + agents** | free canvas editor, generated agents with real system prompts (§4 option 3), scheduled runs while the app is open | |

P0+P1 is the minimum useful slice and has no editor: start from the
built-in template and edit JSON. That flushes out the engine against a real
opencode before investing in canvas UI.

## 8. Risks

- **opencode semantics drift.** Relies on `execution.*` events, metadata on
  PATCH and agent/model switching; all pinned to 2.2.6 and covered by the
  protocol test harness, so a version bump re-probes them.
- **Cost.** Fan-out multiplies tokens. Show an estimate (nodes × model) in the
  start dialog and default `max_parallel` low.
- **Output quality between hops.** "Last assistant message" can be a
  one-liner ("Done!") after a long tool-heavy turn. Each Agent node's prompt
  gets a fixed footer asking it to end with a handoff summary; the run view
  shows exactly what was passed on, editable before Resume when a Gate is used.
- **GPUI canvas effort** is the biggest unknown, which is why it is last.

## 9. Open questions for the owner

1. Stage editor first (fast, covers the example) or go straight to a canvas?
2. Should workflow files be in the project (`.ocdesk/workflows/`, shared via
   git) by default, or personal by default?
3. Parallel write nodes: worktree per node (safe, needs merge) or
   "one writer per stage" as a validation rule (simpler)?
4. Is "run later" = rerun on demand + queue enough, or do you want scheduled /
   headless runs (that implies a `ocdesk run <workflow>` CLI without the GUI)?
5. OK to write generated agent files into `.opencode/agent/` (P4), or keep
   roles as prepended prompt text only?
