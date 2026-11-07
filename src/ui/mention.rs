//! `@` mentions and the `/` menu in the composer: finding the query under
//! the caret and turning the composer's tokens into prompt attachments. Pure
//! functions, so they're testable without a window.

use std::ops::Range;
use std::path::Path;

use crate::api::model::{Mention, PromptInput};

/// Token ids carry what they reference: `file:<relative path>` or
/// `agent:<agent id>`.
pub const FILE_PREFIX: &str = "file:";
pub const AGENT_PREFIX: &str = "agent:";
pub const SKILL_PREFIX: &str = "skill:";

/// The `/query` being typed: only as the first word of the prompt, with the
/// caret still inside it.
pub fn slash_query(text: &str, caret: usize) -> Option<&str> {
    let before = text.get(..caret)?;
    let query = before.strip_prefix('/')?;
    (!query.chars().any(char::is_whitespace)).then_some(query)
}

/// `/name rest…` → (`name`, `rest`), for running a server command.
pub fn parse_command(text: &str) -> Option<(&str, &str)> {
    let body = text.trim_start().strip_prefix('/')?;
    let (name, rest) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    (!name.is_empty()).then(|| (name, rest.trim()))
}

/// The `@query` the caret is in, as (byte offset of `@`, query). A mention
/// starts at the beginning of the text or after whitespace, runs up to the
/// caret without whitespace, and can't sit inside an existing token.
pub fn active_query<'a>(
    text: &'a str,
    caret: usize,
    tokens: &[Range<usize>],
) -> Option<(usize, &'a str)> {
    let before = text.get(..caret)?;
    let at = before.rfind('@')?;
    let query = &before[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    if before[..at]
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace())
    {
        return None;
    }
    if tokens.iter().any(|t| t.start < caret && at < t.end) {
        return None;
    }
    Some((at, query))
}

/// Build the prompt from the composer's text and its token spans.
/// File tokens resolve against `folder`; unknown token ids stay plain text.
pub fn build_prompt<'a>(
    text: &str,
    tokens: impl IntoIterator<Item = (Range<usize>, &'a str)>,
    folder: &Path,
) -> PromptInput {
    let mut input = PromptInput {
        text: text.to_string(),
        ..Default::default()
    };
    for (range, id) in tokens {
        let Some(mention) = Mention::from_byte_range(text, range) else {
            continue;
        };
        if let Some(rel) = id.strip_prefix(FILE_PREFIX) {
            input.files.push((folder.join(rel), mention));
        } else if let Some(agent) = id.strip_prefix(AGENT_PREFIX) {
            input.agents.push((agent.to_string(), mention));
        } else if let Some(skill) = id.strip_prefix(SKILL_PREFIX) {
            input.skills.push((skill.to_string(), mention));
        }
    }
    input
}

/// Case-insensitive subsequence match, so `mdrs` finds `src/model.rs` the
/// way the server's file search does.
pub fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut chars = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|q| chars.any(|c| c == q))
}

/// Up/Down recall of earlier prompts, shell-style. `cursor == None` means
/// the user is editing their own draft.
#[derive(Default)]
pub struct History {
    cursor: Option<usize>,
    draft: String,
}

impl History {
    /// The entry Up shows, or `None` when there's nothing older.
    pub fn older(&mut self, entries: &[String], current: &str) -> Option<String> {
        let next = match self.cursor {
            None => {
                self.draft = current.to_string();
                entries.len().checked_sub(1)?
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.cursor = Some(next);
        entries.get(next).cloned()
    }

    /// The entry Down shows; past the newest it restores the draft.
    pub fn newer(&mut self, entries: &[String]) -> Option<String> {
        let i = self.cursor?;
        if i + 1 < entries.len() {
            self.cursor = Some(i + 1);
            entries.get(i + 1).cloned()
        } else {
            self.cursor = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    pub fn browsing(&self) -> bool {
        self.cursor.is_some()
    }

    /// The user edited the recalled text or sent it: stop browsing.
    pub fn reset(&mut self) {
        self.cursor = None;
        self.draft.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_only_as_first_word() {
        assert_eq!(slash_query("/", 1), Some(""));
        assert_eq!(slash_query("/com", 4), Some("com"));
        assert_eq!(slash_query("/compact now", 12), None, "past the first word");
        assert_eq!(slash_query("say /x", 6), None);
        assert_eq!(slash_query(" /x", 3), None);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("/review"), Some(("review", "")));
        assert_eq!(
            parse_command("/review  branch main "),
            Some(("review", "branch main"))
        );
        assert_eq!(parse_command("/"), None);
        assert_eq!(parse_command("no slash"), None);
    }

    #[test]
    fn skill_tokens_become_skill_attachments() {
        let text = "/greeter hello";
        let input = build_prompt(text, [(0..8, "skill:greeter")], Path::new("/repo"));
        assert_eq!(input.skills.len(), 1);
        assert_eq!(input.skills[0].0, "greeter");
        assert_eq!(input.skills[0].1.end, 8);
    }

    #[test]
    fn finds_query_at_caret() {
        assert_eq!(active_query("@", 1, &[]), Some((0, "")));
        assert_eq!(active_query("look @src/ma", 12, &[]), Some((5, "src/ma")));
        // Caret in the middle of the word: query is what's before it.
        assert_eq!(active_query("a @readme x", 6, &[]), Some((2, "rea")));
        // Not after whitespace (an email), whitespace in the query, no @.
        assert_eq!(active_query("me@host", 7, &[]), None);
        assert_eq!(active_query("@a b", 4, &[]), None);
        assert_eq!(active_query("plain", 5, &[]), None);
        // Inside or right after an existing token.
        let token = [Range { start: 0, end: 10 }];
        assert_eq!(active_query("@README.md", 10, &token), None);
        // Multibyte text before the mention.
        assert_eq!(active_query("é @x", 5, &[]), Some((3, "x")));
    }

    #[test]
    fn prompt_from_tokens_uses_utf16_offsets() {
        let text = "é see @a.rs and @general";
        let file = text.find("@a.rs").unwrap();
        let agent = text.find("@general").unwrap();
        let input = build_prompt(
            text,
            [
                (file..file + 5, "file:src/a.rs"),
                (agent..agent + 8, "agent:general"),
                (0..2, "unknown:x"),
            ],
            Path::new("/repo"),
        );
        assert_eq!(input.text, text);
        assert_eq!(input.files.len(), 1);
        let (path, m) = &input.files[0];
        assert_eq!(path, Path::new("/repo/src/a.rs"));
        // "é" is 2 bytes but 1 UTF-16 unit.
        assert_eq!((m.start, m.end, m.text.as_str()), (6, 11, "@a.rs"));
        assert_eq!(input.agents[0].0, "general");
        let v = input.to_json();
        assert_eq!(v["files"][0]["uri"], "file:///repo/src/a.rs");
        assert_eq!(v["files"][0]["name"], "a.rs");
        assert_eq!(v["agents"][0]["mention"]["text"], "@general");
    }

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_match("General", "gen"));
        assert!(fuzzy_match("src/model.rs", "mdrs"));
        assert!(fuzzy_match("x", ""));
        assert!(!fuzzy_match("explore", "gen"));
    }

    #[test]
    fn history_walks_and_restores_draft() {
        let entries = vec!["one".to_string(), "two".to_string()];
        let mut h = History::default();
        assert_eq!(h.newer(&entries), None);
        assert_eq!(h.older(&entries, "draft").as_deref(), Some("two"));
        assert_eq!(h.older(&entries, "two").as_deref(), Some("one"));
        assert_eq!(h.older(&entries, "one"), None);
        assert_eq!(h.newer(&entries).as_deref(), Some("two"));
        assert_eq!(h.newer(&entries).as_deref(), Some("draft"));
        assert!(!h.browsing());
        assert_eq!(History::default().older(&[], ""), None);
    }
}
