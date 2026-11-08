# Workflow builder: plan

_Status: proposal, 2026-11-08. Nothing implemented._

## Idea

A workflow is a **saved list of steps**, like a skill that runs agents one
after another. Each step gets the previous step's output. Branching out is
done the opencode way: a step's agent hands work to **subagents**, opencode
runs them in parallel, and their results come back to that agent, which
synthesizes them. Under the hood every agent call is
**opencode run headless**; the tab is just a visual way to build, run and
watch the list.

```
[ architect ] → [ swe ] → [ synthesizer ]
                               ├─ security ┐
                               ├─ review   ├─ subagents, in parallel,
                               └─ qa       ┘  results back to synthesizer
```

No graph engine, no conditions, no loops, no worktrees in v1.

## Agents and prompts come from opencode

ocdesk does not define its own agents or system prompts. A step names an
**opencode agent**, and that agent's file carries its role prompt, model,
tools and permissions:

- project: `<folder>/.opencode/agent/<name>.md`
- global: `~/.config/opencode/agent/<name>.md`

```markdown
---
description: Reviews changes for security issues
mode: subagent
model: anthropic/claude-opus-5-5
---
You are a security reviewer. Look for injection, auth and secrets issues...
```

So `architect.md`, `swe.md`, `security.md`, `review.md`, `qa.md` and
`synthesizer.md` are ordinary opencode agents: they also work in the normal
chat, and the workflow is only the order they run in.

Reusable task prompts come from opencode's **commands** folder
(`.opencode/command/<name>.md`, templates with `$ARGUMENTS`). A step can use
one instead of free text, e.g. `/review`.

ocdesk reads both lists from the running server (`GET /api/agent`, which
includes each agent's `system` and `model`, and `GET /api/command`), so the
pickers always match what opencode actually loaded, built-ins included.

_To check on 2.2.6 before building: the exact folder names (`agent` vs
`agents`, `command` vs `commands`) and frontmatter keys._

## The workflow file

One file per workflow in `<folder>/.ocdesk/workflows/<name>.json`, committed
next to the agents it uses:

```json
{
  "name": "feature",
  "steps": [
    {"agent": "architect"},
    {"agent": "swe"},
    {"agent": "synthesizer", "subagents": ["security", "review", "qa"]}
  ]
}
```

- `steps` run in order, one after another.
- Per step, everything is optional except `agent`:
  - `subagents`: agents this step fans out to (they must be `mode: subagent`
    or `all` in their agent file);
  - `prompt`: extra text for this step only (`{{task}}` = what you typed at
    run start);
  - `command`: use a command template as the prompt;
  - `model`: override the agent file's model for this workflow.
- Nothing else. Role, model and permissions live in the agent file.

## How a run works

For each step, in order:

1. Build the message: the task (or the step's prompt/command), plus the
   previous step's output.
2. If the step has `subagents`, attach them to the prompt (opencode's
   `agents` attachment, the same thing an `@security` mention does) and add
   one line: "Delegate to each of these subagents in parallel, then combine
   their results." The step's agent calls opencode's `subagent` tool for each;
   opencode runs them as child sessions and returns their results to it.
3. Run the step headless in the workflow's folder and wait for it to finish.
   Its final reply is the step's output.

ocdesk does no parallel scheduling of its own: parallelism, the child
sessions and handing results back are all opencode's subagent mechanism.
The catch is that the agent decides how to call them; the agent file
(e.g. `synthesizer.md`) should say "always use your subagents", and the run
view shows which ones actually ran, so a skipped subagent is visible.

If a step fails, the run stops and shows the error. Stop interrupts the
running step, which also stops its subagents.

**Headless call.** `opencode run` with the agent, optional model or command,
and the message. Check its flags on the pinned 2.2.6 first. If it doesn't
take those, call the `opencode serve` process ocdesk already runs instead:
one new session per agent, set agent and model, prompt it (or run the
command), read the last reply. To the user it's identical, and the sidecar
also gives each agent a session to open in the chat view, with its approval
prompts.

**Outputs** are saved in `.ocdesk/runs/<run>/<step>-<agent>.md`; subagent
results are part of the step's session (child sessions with `parentID`).

## The tab

- **Workflows tab:** lists the folder's workflows; *New*, *Edit*, *Run*.
- **Editor:** the steps as a row of cards with arrows between them. A card
  is an agent picker (from opencode's list, showing its description and
  model), a subagent multi-picker (only `subagent`/`all` agents), an optional
  command picker, an optional model override and an optional extra prompt.
  Subagents show as small chips branching off the card. Buttons: add step,
  delete, move left/right. Save writes the JSON.
- **Agents:** *Open agent file* on a card opens its `.md` in the editor;
  *New agent* writes a starter `.opencode/agent/<name>.md`. opencode reloads
  agents on its own (`agent.updated`, which ocdesk already listens for).
- **Run:** type the task, press Run. The same row of cards, each showing
  waiting / running / done / failed, and its subagent chips light up as
  their child sessions start and finish (`session.created` with `parentID`,
  already tracked for the Tasks panel). Click a card or chip to open that
  session. Run
  history lists past runs with *Run again*.

## Phases

1. **Runner:** load a workflow file, run it headless with opencode agents and
   commands, write outputs. Tested with `dev/mock_llm.py` and sample agent
   files in a test folder.
2. **Tab:** list, run and run status view.
3. **Editor:** build and save workflows; open/new agent files.

## Later, only if needed

- Loops ("synthesizer says fix → back to SWE, max 2 times").
- Worktrees when two subagents in one step both edit files.
- A free canvas instead of the row of cards.
