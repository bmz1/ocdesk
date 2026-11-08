# Workflow builder: plan

_Status: proposal, 2026-11-08. Nothing implemented._

## Idea

A workflow is a **saved list of steps**, like a skill that runs agents one
after another. Each step is an agent + model + prompt. A step can hold
several agents that run in parallel. Each step gets the previous step's
output. Under the hood every agent call is **opencode run headless**; the
tab is just a visual way to build, run and watch the list.

```
[ architect ] → [ swe ] → [ security | review | qa ] → [ synthesizer ]
```

No graph engine, no conditions, no loops, no worktrees in v1.

## The file

One file per workflow in `<folder>/.ocdesk/workflows/<name>.json`, so it can
be committed and shared like a skill:

```json
{
  "name": "feature",
  "steps": [
    [{"label": "Architect", "agent": "plan",  "model": "anthropic/claude-opus-5-5",   "prompt": "Design: {{task}}"}],
    [{"label": "SWE",       "agent": "build", "model": "anthropic/claude-sonnet-5-5", "prompt": "Implement this design."}],
    [{"label": "Security",  "agent": "plan",  "prompt": "Security review of the changes."},
     {"label": "Review",    "agent": "plan",  "prompt": "Code review of the changes."},
     {"label": "QA",        "agent": "build", "prompt": "Write and run tests for the changes."}],
    [{"label": "Synthesizer", "agent": "plan", "prompt": "Merge the findings into one report."}]
  ]
}
```

- `steps` is a list of lists: the outer list runs in order, and the agents in
  an inner list run at the same time.
- `{{task}}` is what you type when you start a run. Everything else is plain
  prompt text.
- `model` is optional and falls back to the folder default.

## How a run works

For each step, in order:

1. Build each agent's prompt: its own prompt, plus the previous step's
   outputs appended under `## <label>` headings.
2. Run every agent in the step headless with its agent and model, all at once.
3. Wait for all of them, keep each one's final text, and move to the next step.

If an agent fails, the run stops and shows the error. Stop kills whatever is
running.

**Headless call.** Use the `opencode run` CLI with agent, model and prompt
arguments, in the workflow's folder. Before building, check its flags on the
pinned 2.2.6. If it doesn't take agent/model/prompt the way we need, call the
`opencode serve` sidecar ocdesk already runs instead: one new session per
agent, set its agent and model, prompt it, read the last reply. To the user
it's identical. The sidecar also gives us the session to open in the normal
conversation view, and approval prompts.

**Outputs** are saved next to the workflow in
`.ocdesk/runs/<run>/<step>-<label>.md`, so a run can be read later.

## The tab

- **Workflows tab:** lists the folder's workflows; *New*, *Edit*, *Run*.
- **Editor:** the steps as a row of cards with arrows between them, parallel
  agents stacked inside one step. A card has a label, an agent picker, a model
  picker (reuse the composer's) and a prompt box. Buttons: add step, add
  parallel agent, delete, move left/right. Save writes the JSON file.
- **Run:** type the task, press Run. The same row of cards, each showing
  waiting / running / done / failed. Click a card to read its output.
  Run history lists past runs with *Run again*.

## Phases

1. **Runner:** load a workflow file and run it headless, write outputs.
   Tested with `dev/mock_llm.py`.
2. **Tab:** list, run and run status view.
3. **Editor:** build and save workflows in the tab.

## Later, only if needed

- Loops ("synthesizer says fix → back to SWE, max 2 times").
- Worktrees when two agents in one step both edit files.
- A free canvas instead of the row of cards.
