//! Syntax highlighting outside markdown: diffs, file reads, written files.
//!
//! Uses the same syntect syntax set as the markdown code fences
//! (gpui-component's `syntect` feature, two-face's extended set) and maps
//! TextMate scopes onto the theme's highlight names (`keyword`, `string`, …),
//! so colours follow the app theme and match the fences. The scope mapping
//! follows gpui-component's `highlighter/syntect_backend.rs` (Apache-2.0).

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::LazyLock;

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::HighlightStyleResolver as _;
use gpui_kit::*;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Highlight longer text only up to here; the rest stays plain.
const MAX_BYTES: usize = 256 * 1024;
const CACHE_ENTRIES: usize = 256;

/// Ordered, non-overlapping highlight names over a text.
pub type Spans = Rc<Vec<(Range<usize>, &'static str)>>;

thread_local! {
    static CACHE: RefCell<HashMap<u64, Spans>> = RefCell::new(HashMap::new());
}

/// The syntax for a path (`src/a.rs`), extension (`rs`) or language name
/// (`rust`); `None` for plain text or unknown.
fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let s = &*SYNTAXES;
    let file = lang.rsplit('/').next().unwrap_or(lang);
    let ext = file.rsplit_once('.').map(|(_, e)| e);
    [Some(lang), ext, Some(file)]
        .into_iter()
        .flatten()
        .filter(|t| !t.is_empty())
        .find_map(|t| {
            s.find_syntax_by_token(t)
                .or_else(|| s.find_syntax_by_extension(t))
        })
        .filter(|syn| syn.name != "Plain Text")
}

/// Whether `lang` (path, extension or name) is a language we can colour.
pub fn supported(lang: &str) -> bool {
    find_syntax(lang).is_some()
}

/// Highlight names for `code` in `lang`; empty when unknown. Cached.
pub fn highlight(lang: &str, code: &str) -> Spans {
    let Some(syntax) = find_syntax(lang) else {
        return Rc::default();
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    syntax.name.hash(&mut h);
    code.hash(&mut h);
    let key = h.finish();
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let end = if code.len() > MAX_BYTES {
        let mut e = MAX_BYTES;
        while !code.is_char_boundary(e) {
            e -= 1;
        }
        e
    } else {
        code.len()
    };
    let spans: Spans = Rc::new(parse(syntax, &code[..end]));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_ENTRIES {
            c.clear();
        }
        c.insert(key, spans.clone());
    });
    spans
}

fn parse(syntax: &SyntaxReference, text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut names = HashMap::<Scope, Option<&'static str>>::new();
    let mut out: Vec<(Range<usize>, &'static str)> = Vec::new();
    let mut offset = 0;
    let mut push = |range: Range<usize>,
                    scopes: &ScopeStack,
                    out: &mut Vec<(Range<usize>, &'static str)>| {
        if range.is_empty() {
            return;
        }
        // The innermost scope that names something wins.
        let name = scopes
            .scopes
            .iter()
            .rev()
            .find_map(|s| *names.entry(*s).or_insert_with(|| semantic_name(*s)));
        if let Some(name) = name {
            match out.last_mut() {
                Some((last, n)) if last.end == range.start && *n == name => last.end = range.end,
                _ => out.push((range, name)),
            }
        }
    };
    for line in LinesWithEndings::from(text) {
        if let Ok(ops) = parser.parse_line(line, &SYNTAXES) {
            let mut cursor = 0;
            for (index, op) in ops {
                push(offset + cursor..offset + index, &scopes, &mut out);
                let _ = scopes.apply(&op);
                cursor = index;
            }
            push(offset + cursor..offset + line.len(), &scopes, &mut out);
        }
        offset += line.len();
    }
    out
}

/// The theme's highlight name for a TextMate scope, `None` to defer to an
/// enclosing scope.
fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let is = |p: &str| {
        scope == p
            || scope
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('.'))
    };
    Some(
        if is("punctuation.definition.string")
            || is("punctuation.definition.comment")
            || is("invalid")
            || is("meta")
            || is("source")
            || is("text")
        {
            return None;
        } else if is("comment.block.documentation") || is("comment.line.documentation") {
            "comment.doc"
        } else if is("comment") {
            "comment"
        } else if is("constant.character.escape") {
            "string.escape"
        } else if is("string.regexp") {
            "string.regex"
        } else if is("string") {
            "string"
        } else if is("constant.numeric") {
            "number"
        } else if is("constant.language.boolean") {
            "boolean"
        } else if is("constant") || is("support.constant") || is("entity.name.constant") {
            "constant"
        } else if is("keyword.operator") {
            "operator"
        } else if is("keyword.control.import") {
            "preproc"
        } else if is("keyword") || is("storage.modifier") {
            "keyword"
        } else if is("storage.type") {
            let decl = [
                "class",
                "enum",
                "function",
                "impl",
                "interface",
                "module",
                "namespace",
                "struct",
                "trait",
                "type",
                "let",
                "var",
                "const",
            ];
            if decl.iter().any(|w| scope.contains(w)) {
                "keyword"
            } else {
                "type"
            }
        } else if is("entity.name.function") || is("support.function") || is("variable.function") {
            "function"
        } else if is("entity.name.type")
            || is("entity.name.class")
            || is("entity.name.struct")
            || is("entity.name.enum")
            || is("entity.name.trait")
            || is("entity.name.interface")
            || is("entity.other.inherited-class")
            || is("support.type")
            || is("support.class")
        {
            "type"
        } else if is("entity.name.tag") {
            "tag"
        } else if is("entity.other.attribute-name") {
            "attribute"
        } else if is("entity.name.section") || is("markup.heading") {
            "title"
        } else if is("variable.language") {
            "variable.special"
        } else if is("variable.other.member")
            || is("variable.other.property")
            || is("support.variable.property")
            || is("meta.mapping.key")
        {
            "property"
        } else if is("variable") {
            "variable"
        } else if is("markup.bold") {
            "emphasis.strong"
        } else if is("markup.italic") {
            "emphasis"
        } else if is("punctuation.section") || is("punctuation.definition.block") {
            "punctuation.bracket"
        } else if is("punctuation.separator") || is("punctuation.terminator") {
            "punctuation.delimiter"
        } else if is("punctuation") {
            "punctuation"
        } else {
            return None;
        },
    )
}

/// `spans` restricted to `range` and shifted to start at 0.
pub fn slice(
    spans: &[(Range<usize>, &'static str)],
    range: Range<usize>,
) -> Vec<(Range<usize>, &'static str)> {
    let start = spans.partition_point(|(r, _)| r.end <= range.start);
    spans[start..]
        .iter()
        .take_while(|(r, _)| r.start < range.end)
        .map(|(r, n)| {
            (
                r.start.max(range.start) - range.start..r.end.min(range.end) - range.start,
                *n,
            )
        })
        .filter(|(r, _)| !r.is_empty())
        .collect()
}

/// `text` coloured by `spans` (names resolved through the active theme).
pub fn styled(
    text: impl Into<SharedString>,
    spans: &[(Range<usize>, &'static str)],
    cx: &App,
) -> StyledText {
    let text: SharedString = text.into();
    let theme = &cx.theme().highlight_theme;
    let highlights: Vec<(Range<usize>, HighlightStyle)> = spans
        .iter()
        .filter(|(r, _)| {
            r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
        })
        .filter_map(|(r, n)| theme.style(n).map(|s| (r.clone(), s)))
        .collect();
    StyledText::new(text).with_highlights(highlights)
}

/// Like [`highlight`], for tool output whose lines may carry a line-number
/// gutter (`00012| code`, `  12\tcode`) or `<file>` wrapper lines: the
/// gutters are left plain and the code after them is highlighted as one
/// text. Spans index into `text` itself.
pub fn highlight_gutter(lang: &str, text: &str) -> Vec<(Range<usize>, &'static str)> {
    if !supported(lang) {
        return Vec::new();
    }
    // (offset of the code in `text`, offset in `code`, length)
    let mut map: Vec<(usize, usize, usize)> = Vec::new();
    let mut code = String::with_capacity(text.len());
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let skip = gutter_len(body);
        let wrapper = body.trim() == "<file>" || body.trim() == "</file>";
        if !wrapper {
            map.push((at + skip, code.len(), body.len() - skip));
            code.push_str(&body[skip..]);
            code.push('\n');
        }
        at += line.len();
    }
    let spans = highlight(lang, &code);
    let mut out = Vec::new();
    for (text_at, code_at, len) in map {
        for (r, n) in slice(&spans, code_at..code_at + len) {
            out.push((r.start + text_at..r.end + text_at, n));
        }
    }
    out
}

/// Bytes of a leading `  12| ` / `00012\t` style line-number gutter.
fn gutter_len(line: &str) -> usize {
    let digits_start = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[digits_start..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return 0;
    }
    let after = &rest[digits..];
    match after.chars().next() {
        Some('|') | Some('\t') | Some(':') => {
            let mut n = digits_start + digits + 1;
            if line[n..].starts_with(' ') {
                n += 1;
            }
            n
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn finds_languages_by_path_extension_and_name() {
        assert!(supported("src/main.rs"));
        assert!(supported("ts"));
        assert!(supported("python"));
        assert!(!supported("notes.unknownext"));
    }

    #[::core::prelude::v1::test]
    fn highlights_keywords_and_strings() {
        let src = "fn main() { let s = \"hi\"; }\n";
        let spans = highlight("rs", src);
        let name_at = |needle: &str| {
            let at = src.find(needle).unwrap();
            spans.iter().find(|(r, _)| r.contains(&at)).map(|(_, n)| *n)
        };
        assert_eq!(name_at("fn"), Some("keyword"));
        assert_eq!(name_at("\"hi\""), Some("string"));
    }

    #[::core::prelude::v1::test]
    fn gutters_stay_plain_and_code_is_coloured() {
        let text = "<file>\n00001| fn a() {}\n00002| \"x\"\n</file>\n";
        let spans = highlight_gutter("rs", text);
        let at = |needle: &str| text.find(needle).unwrap();
        let name = |i: usize| spans.iter().find(|(r, _)| r.contains(&i)).map(|(_, n)| *n);
        assert_eq!(name(at("fn")), Some("keyword"));
        assert_eq!(name(at("\"x\"")), Some("string"));
        assert_eq!(name(at("00001")), None);
        assert_eq!(gutter_len("  12\tlet"), 5);
        assert_eq!(gutter_len("let x = 1"), 0);
    }

    #[::core::prelude::v1::test]
    fn slice_clips_and_shifts() {
        let spans = vec![(0..4, "keyword"), (5..10, "string")];
        assert_eq!(
            slice(&spans, 3..7),
            vec![(0..1, "keyword"), (2..4, "string")]
        );
    }
}
