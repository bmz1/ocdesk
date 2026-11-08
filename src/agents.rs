//! opencode agent files: `<root>/{agent,agents}/**/*.md`, a YAML
//! frontmatter (`description`, `mode`, `model`, …) and a body that is the
//! agent's system prompt. opencode 2.2.6 reads them from the folder's
//! `.opencode`, `.claude` and `.agents` directories and from the global
//! `$XDG_CONFIG_HOME/opencode` (default `~/.config/opencode`, on every
//! platform), `~/.claude` and `~/.agents`; it watches those directories
//! and reloads its agents (`agent.updated`) when a file changes.
//!
//! ocdesk edits only the simple scalar keys it shows. Every other
//! frontmatter entry (permissions, request, comments, …) is kept verbatim,
//! so saving never drops settings it doesn't understand.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

/// Where an agent file lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    /// Under the folder: shared with the project through git.
    Project,
    /// Under the user's home/config: available in every folder.
    Global,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Project => "Project",
            Scope::Global => "Global",
        }
    }
}

/// Top-level frontmatter entry: a `key:` line plus its indented
/// continuation lines, or a comment/blank line (`key: None`).
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    key: Option<String>,
    lines: Vec<String>,
}

/// A parsed agent file that renders back byte-for-byte when unchanged.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDoc {
    entries: Vec<Entry>,
    pub body: String,
}

impl AgentDoc {
    pub fn parse(content: &str) -> Self {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let Some((front, body)) = split_frontmatter(content) else {
            return Self {
                entries: Vec::new(),
                body: content.to_string(),
            };
        };
        let mut entries: Vec<Entry> = Vec::new();
        for line in front.lines() {
            let continuation = line.starts_with([' ', '\t']) || line.trim_start().starts_with('-');
            match top_key(line) {
                Some(key) if !continuation => entries.push(Entry {
                    key: Some(key),
                    lines: vec![line.to_string()],
                }),
                _ => match entries.last_mut() {
                    Some(e) if e.key.is_some() && (continuation || line.trim().is_empty()) => {
                        e.lines.push(line.to_string())
                    }
                    _ => entries.push(Entry {
                        key: None,
                        lines: vec![line.to_string()],
                    }),
                },
            }
        }
        // Blank lines that trail a keyed entry belong between entries.
        let mut fixed = Vec::with_capacity(entries.len());
        for mut e in entries {
            let mut tail = Vec::new();
            while e.key.is_some()
                && e.lines.len() > 1
                && e.lines.last().is_some_and(|l| l.trim().is_empty())
            {
                tail.push(e.lines.pop().unwrap_or_default());
            }
            fixed.push(e);
            fixed.extend(tail.into_iter().rev().map(|l| Entry {
                key: None,
                lines: vec![l],
            }));
        }
        Self {
            entries: fixed,
            body: body.to_string(),
        }
    }

    pub fn render(&self) -> String {
        if self.entries.is_empty() {
            return self.body.clone();
        }
        let mut out = String::from("---\n");
        for e in &self.entries {
            for l in &e.lines {
                out.push_str(l);
                out.push('\n');
            }
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }

    /// A scalar value (plain, quoted or block); `None` when absent or empty.
    pub fn get(&self, key: &str) -> Option<String> {
        let e = self
            .entries
            .iter()
            .find(|e| e.key.as_deref() == Some(key))?;
        let first = e.lines.first()?;
        let raw = first.split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
        let value = if raw.starts_with('|') || raw.starts_with('>') {
            let folded = raw.starts_with('>');
            let rest: Vec<&str> = e.lines[1..].iter().map(|l| l.trim()).collect();
            rest.join(if folded { " " } else { "\n" })
                .trim()
                .to_string()
        } else if raw.starts_with('"') {
            serde_json::from_str::<String>(raw)
                .unwrap_or_else(|_| raw.trim_matches('"').to_string())
        } else if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
            inner.replace("''", "'")
        } else {
            // A plain scalar ends at ` #` (a comment).
            raw.split(" #").next().unwrap_or(raw).trim().to_string()
        };
        (!value.is_empty()).then_some(value)
    }

    /// Set a scalar (`None` or empty removes the key). Kept in place when it
    /// exists; appended otherwise.
    pub fn set(&mut self, key: &str, value: Option<&str>) {
        let pos = self
            .entries
            .iter()
            .position(|e| e.key.as_deref() == Some(key));
        let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            if let Some(i) = pos {
                self.entries.remove(i);
            }
            return;
        };
        let entry = Entry {
            key: Some(key.to_string()),
            lines: vec![format!("{key}: {}", yaml_scalar(value))],
        };
        match pos {
            Some(i) => self.entries[i] = entry,
            None => self.entries.push(entry),
        }
    }
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    if let Some(body) = rest
        .strip_prefix("---\n")
        .or_else(|| rest.strip_prefix("---\r\n"))
    {
        return Some(("", body));
    }
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let front = rest[..at].trim_end_matches(['\n', '\r']);
            return Some((front, &rest[at + line.len()..]));
        }
        at += line.len();
    }
    None
}

fn top_key(line: &str) -> Option<String> {
    let (key, _) = line.split_once(':')?;
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    ok.then(|| key.to_string())
}

/// A YAML scalar for `value`: plain when that is unambiguous, otherwise a
/// double-quoted string (JSON escaping is valid YAML).
fn yaml_scalar(value: &str) -> String {
    let plain = value
        .chars()
        .all(|c| c.is_alphanumeric() || " _-./@,()".contains(c))
        && !value.starts_with([' ', '-', '@'])
        && !value.ends_with(' ')
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "null" | "on" | "off" | "~"
        )
        && value.parse::<f64>().is_err();
    if plain {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_default()
    }
}

/// One agent file on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentFile {
    /// The agent id: the path under `agent/` without `.md`.
    pub name: String,
    pub path: PathBuf,
    pub scope: Scope,
    /// The config directory it was found under (`.opencode`, `~/.claude`, …).
    pub root: PathBuf,
    pub doc: AgentDoc,
}

impl AgentFile {
    pub fn description(&self) -> Option<String> {
        self.doc.get("description")
    }
    /// `primary`, `subagent` or `all`; opencode treats a missing mode as `all`.
    pub fn mode(&self) -> Option<String> {
        self.doc.get("mode")
    }
    /// `provider/model`, optionally `#variant`.
    pub fn model(&self) -> Option<String> {
        self.doc.get("model")
    }
    pub fn prompt(&self) -> &str {
        self.doc.body.trim()
    }
}

/// What the editor changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentEdit {
    pub description: String,
    pub mode: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
}

impl AgentEdit {
    pub fn of(file: &AgentFile) -> Self {
        Self {
            description: file.description().unwrap_or_default(),
            mode: file.mode(),
            model: file.model(),
            prompt: file.prompt().to_string(),
        }
    }

    pub fn apply(&self, doc: &mut AgentDoc) {
        doc.set("description", Some(&self.description));
        doc.set("mode", self.mode.as_deref());
        // A legacy separate `variant` key would be joined onto the new model.
        if self.model.is_some() || doc.get("model").is_some() {
            doc.set("variant", None);
        }
        doc.set("model", self.model.as_deref());
        let prompt = self.prompt.trim();
        doc.body = if prompt.is_empty() {
            String::new()
        } else {
            format!("{prompt}\n")
        };
    }
}

/// `provider/model#variant` ↔ the API's model reference.
pub fn model_string(r: &crate::api::model::ModelRef) -> String {
    match &r.variant {
        Some(v) => format!("{}/{}#{v}", r.provider_id, r.id),
        None => format!("{}/{}", r.provider_id, r.id),
    }
}

pub fn parse_model(s: &str) -> Option<crate::api::model::ModelRef> {
    let (model, variant) = match s.split_once('#') {
        Some((m, v)) => (m, Some(v.to_string()).filter(|v| !v.is_empty())),
        None => (s, None),
    };
    let (provider, id) = model.split_once('/')?;
    (!provider.is_empty() && !id.is_empty()).then(|| crate::api::model::ModelRef {
        provider_id: provider.to_string(),
        id: id.to_string(),
        variant,
    })
}

/// opencode's global config directory.
pub fn global_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|c| c.join("opencode"))
}

/// Config roots searched, in opencode's precedence order (later wins).
fn roots(folder: &Path) -> Vec<(PathBuf, Scope)> {
    let mut out = Vec::new();
    if let Some(g) = global_config_dir() {
        out.push((g, Scope::Global));
    }
    if let Some(home) = dirs::home_dir() {
        out.push((home.join(".claude"), Scope::Global));
        out.push((home.join(".agents"), Scope::Global));
    }
    for d in [".claude", ".agents", ".opencode"] {
        out.push((folder.join(d), Scope::Project));
    }
    out
}

/// Every agent file visible from `folder`, project files first, then by name.
pub fn discover(folder: &Path) -> Vec<AgentFile> {
    discover_in(&roots(folder))
}

fn discover_in(roots: &[(PathBuf, Scope)]) -> Vec<AgentFile> {
    let mut out = Vec::new();
    for (root, scope) in roots {
        for sub in ["agent", "agents"] {
            let dir = root.join(sub);
            let mut files = Vec::new();
            collect_md(&dir, &mut files, 0);
            for path in files {
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(name) = path
                    .strip_prefix(&dir)
                    .ok()
                    .and_then(|p| p.to_str())
                    .map(|p| p.replace('\\', "/").trim_end_matches(".md").to_string())
                else {
                    continue;
                };
                out.push(AgentFile {
                    name,
                    path,
                    scope: *scope,
                    root: root.clone(),
                    doc: AgentDoc::parse(&content),
                });
            }
        }
    }
    out.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.name.cmp(&b.name)));
    out
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_md(&p, out, depth + 1);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// Where a new agent of `scope` goes: `<root>/agent/<name>.md`, or
/// `agents/` when only that directory exists.
pub fn new_agent_path(folder: &Path, scope: Scope, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let root = match scope {
        Scope::Project => folder.join(".opencode"),
        Scope::Global => global_config_dir().context("no home directory")?,
    };
    let sub = if !root.join("agent").is_dir() && root.join("agents").is_dir() {
        "agents"
    } else {
        "agent"
    };
    Ok(root.join(sub).join(format!("{name}.md")))
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Name the agent");
    }
    if name.starts_with(['.', '-'])
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Use letters, digits, - and _ only");
    }
    Ok(())
}

/// Write a new agent file; fails if one already exists there.
pub fn create(path: &Path, edit: &AgentEdit) -> Result<()> {
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let mut doc = AgentDoc::parse("---\n---\n");
    edit.apply(&mut doc);
    write(path, &doc)
}

pub fn write(path: &Path, doc: &AgentDoc) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // Write next to the file, then rename: opencode's watcher never reads a
    // half-written agent.
    let tmp = path.with_extension("md.ocdesk-tmp");
    std::fs::write(&tmp, doc.render()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))
}

/// Starter agents. `lead` runs the others: plan → implement → three
/// parallel reviews → one report.
pub fn pipeline_template() -> Vec<(&'static str, AgentEdit)> {
    let sub = |description: &str, prompt: &str| AgentEdit {
        description: description.into(),
        mode: Some("subagent".into()),
        model: None,
        prompt: prompt.into(),
    };
    vec![
        (
            "lead",
            AgentEdit {
                description:
                    "Runs architect, swe, then security/review/qa in parallel, and reports".into(),
                mode: Some("primary".into()),
                model: None,
                prompt: LEAD_PROMPT.into(),
            },
        ),
        (
            "architect",
            sub(
                "Designs the change before any code is written",
                "You are a software architect. Read the relevant code and produce a concise design for the requested change: the files to touch, the approach, interfaces and data shapes, edge cases, and how to test it. Do not edit files. End with a numbered implementation plan.",
            ),
        ),
        (
            "swe",
            sub(
                "Implements a design and makes the checks pass",
                "You are a senior software engineer. Implement the plan you are given, following the conventions of the codebase. Keep the change focused. Run the project's build, linters and tests and fix what fails. End with a summary of what changed and anything left undone.",
            ),
        ),
        (
            "security",
            sub(
                "Reviews the current changes for security issues",
                "You are a security reviewer. Review the uncommitted changes (git diff) for injection, authn/authz mistakes, secrets, unsafe deserialization, path traversal and unsafe defaults. Do not edit files. Report each finding with file:line, severity and a fix; say so plainly if there are none.",
            ),
        ),
        (
            "review",
            sub(
                "Reviews the current changes for correctness and design",
                "You are a code reviewer. Review the uncommitted changes (git diff) for bugs, missed edge cases, unclear naming and needless complexity. Do not edit files. Report each finding with file:line and a concrete suggestion, most important first.",
            ),
        ),
        (
            "qa",
            sub(
                "Tests the current changes",
                "You are a QA engineer. Work out how the change can break, add or extend tests for it, and run the test suite. Only edit test files. Report what you tested, what failed and how to reproduce it.",
            ),
        ),
    ]
}

const LEAD_PROMPT: &str = "You lead a small team of subagents. For every task:

1. Delegate the design to the `architect` subagent and wait for its plan.
2. Delegate the implementation to the `swe` subagent, passing it the full plan.
3. Start the `security`, `review` and `qa` subagents at the same time (in parallel) on the resulting changes.
4. If any of them reports a real problem, send the findings back to `swe` once, then re-run only the reviewers that found problems.
5. Finish with one report: what was built, the review/QA verdict, and any open issues.

Always use your subagents for these steps instead of doing the work yourself.";

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---
description: \"Reviews: security\"
mode: subagent
# keep me
permissions:
  - action: edit
    resource: \"*\"
    effect: deny

model: anthropic/claude-x
---
You are a reviewer.
";

    #[test]
    fn unchanged_file_round_trips() {
        let doc = AgentDoc::parse(SAMPLE);
        assert_eq!(doc.render(), SAMPLE);
        assert_eq!(doc.get("description").as_deref(), Some("Reviews: security"));
        assert_eq!(doc.get("mode").as_deref(), Some("subagent"));
        assert_eq!(doc.get("model").as_deref(), Some("anthropic/claude-x"));
        assert_eq!(doc.body, "You are a reviewer.\n");
    }

    #[test]
    fn editing_keeps_unknown_entries() {
        let mut doc = AgentDoc::parse(SAMPLE);
        AgentEdit {
            description: "New".into(),
            mode: None,
            model: Some("openai/gpt#high".into()),
            prompt: "Be brief.".into(),
        }
        .apply(&mut doc);
        let out = doc.render();
        assert!(out.contains("description: New\n"), "{out}");
        assert!(!out.contains("mode:"), "{out}");
        assert!(out.contains("model: \"openai/gpt#high\"\n"), "{out}");
        assert!(
            out.contains("# keep me\npermissions:\n  - action: edit\n"),
            "{out}"
        );
        assert!(out.ends_with("---\nBe brief.\n"), "{out}");
        let again = AgentDoc::parse(&out);
        assert_eq!(again.get("model").as_deref(), Some("openai/gpt#high"));
        assert_eq!(again.get("description").as_deref(), Some("New"));
    }

    #[test]
    fn scalars_in_all_forms() {
        let doc = AgentDoc::parse(
            "---\na: 'it''s'\nb: |\n  one\n  two\nc: plain # note\nd: >\n  x\n  y\n---\n",
        );
        assert_eq!(doc.get("a").as_deref(), Some("it's"));
        assert_eq!(doc.get("b").as_deref(), Some("one\ntwo"));
        assert_eq!(doc.get("c").as_deref(), Some("plain"));
        assert_eq!(doc.get("d").as_deref(), Some("x y"));
        assert_eq!(doc.get("missing"), None);
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let doc = AgentDoc::parse("Just a prompt\n");
        assert_eq!(doc.body, "Just a prompt\n");
        let mut doc = doc;
        doc.set("mode", Some("primary"));
        assert_eq!(doc.render(), "---\nmode: primary\n---\nJust a prompt\n");
    }

    #[test]
    fn scalar_quoting() {
        assert_eq!(yaml_scalar("Reviews code"), "Reviews code");
        assert_eq!(yaml_scalar("a: b"), "\"a: b\"");
        assert_eq!(yaml_scalar("true"), "\"true\"");
        assert_eq!(yaml_scalar("12"), "\"12\"");
        assert_eq!(yaml_scalar("say \"hi\""), "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn model_strings() {
        let r = parse_model("anthropic/claude-x#high").unwrap();
        assert_eq!(
            (r.provider_id.as_str(), r.id.as_str()),
            ("anthropic", "claude-x")
        );
        assert_eq!(r.variant.as_deref(), Some("high"));
        assert_eq!(model_string(&r), "anthropic/claude-x#high");
        let r = parse_model("openrouter/meta/llama").unwrap();
        assert_eq!(r.id, "meta/llama");
        assert!(parse_model("nope").is_none());
    }

    #[test]
    fn names() {
        assert!(validate_name("security-review_2").is_ok());
        for bad in ["", ".x", "-x", "a b", "a/b", "é"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn discover_create_and_write() {
        let tmp = std::env::temp_dir().join(format!("ocdesk-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let project = tmp.join("proj/.opencode");
        let global = tmp.join("global");
        std::fs::create_dir_all(project.join("agents/team")).unwrap();
        std::fs::write(
            project.join("agents/team/qa.md"),
            "---\nmode: subagent\n---\nTest it.\n",
        )
        .unwrap();
        std::fs::write(project.join("agents/notes.txt"), "ignored").unwrap();
        let path = global.join("agent/lead.md");
        create(&path, &pipeline_template()[0].1).unwrap();
        assert!(
            create(&path, &AgentEdit::default()).is_err(),
            "never overwrites"
        );

        let found = discover_in(&[
            (global.clone(), Scope::Global),
            (project.clone(), Scope::Project),
        ]);
        let names: Vec<_> = found.iter().map(|f| (f.name.as_str(), f.scope)).collect();
        assert_eq!(
            names,
            [("team/qa", Scope::Project), ("lead", Scope::Global)]
        );
        assert_eq!(found[0].mode().as_deref(), Some("subagent"));
        assert_eq!(found[1].mode().as_deref(), Some("primary"));
        assert!(found[1].prompt().contains("architect"));

        let mut qa = found[0].clone();
        let mut edit = AgentEdit::of(&qa);
        edit.prompt = "Test harder.".into();
        edit.apply(&mut qa.doc);
        write(&qa.path, &qa.doc).unwrap();
        let back = std::fs::read_to_string(&qa.path).unwrap();
        assert_eq!(back, "---\nmode: subagent\n---\nTest harder.\n");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
