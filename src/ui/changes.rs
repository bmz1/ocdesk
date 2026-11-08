//! Read-only working-tree diff (HEAD to the working copy) from opencode.
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::Store;

/// What the review panel adds to each file: collapse state, the comment
/// hook and elements under commented lines.
pub type ToggleFile = std::rc::Rc<dyn Fn(String, &mut Window, &mut App)>;

pub struct FileHooks<'a> {
    pub collapsed: &'a std::collections::HashSet<String>,
    pub toggle: ToggleFile,
    pub on_comment: &'a dyn Fn(&str) -> OnLineComment,
    pub below: &'a dyn Fn(&str) -> Vec<(u32, AnyElement)>,
}

pub fn render_changes(
    store: &Store,
    controller: Entity<Controller>,
    hooks: &FileHooks,
    cx: &App,
) -> AnyElement {
    let Some(sid) = store.selected.clone() else {
        return div().into_any_element();
    };
    let theme = cx.theme();
    let dir = store.selected_diff_dir();
    let loading = dir.as_ref().is_some_and(|d| store.diff_loading.contains(d));
    let refresh_controller = controller.clone();
    let refresh_sid = sid;
    let mut col = v_flex().w_full().gap_3().child(
        h_flex()
            .gap_2()
            .justify_between()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Uncommitted changes"),
            )
            .child(
                Button::new("refresh-diff")
                    .label(if loading { "Loading…" } else { "Refresh" })
                    .ghost()
                    .small()
                    .on_click(move |_, _, cx| {
                        refresh_controller
                            .update(cx, |c, cx| c.refresh_diff(refresh_sid.clone(), cx))
                    }),
            ),
    );
    let sid = store.selected.as_ref().unwrap();
    let Some(diff_dir) = dir else {
        return col.into_any_element();
    };
    if let Some(error) = store.diff_errors.get(&diff_dir) {
        col = col.child(div().text_color(theme.danger).child(error.clone()));
    }
    // Same shape as `Controller::viewed_key`, built once instead of per file.
    let viewed_prefix = format!("{}\nWorking tree:\n", diff_dir.display());
    if let Some(files) = store.diffs.get(&diff_dir) {
        if files.is_empty() && !loading {
            col = col.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("No uncommitted changes."),
            );
        }
        let (adds, dels) = files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        if !files.is_empty() {
            col = col.child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} file{}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ))
                    .child(div().text_color(theme.success).child(format!("+{adds}")))
                    .child(div().text_color(theme.danger).child(format!("−{dels}"))),
            );
        }
        for (i, file) in files.iter().enumerate() {
            let key = format!("{viewed_prefix}{}", file.file);
            let viewed = store.preferences.viewed.get(&key) == Some(&file.fingerprint);
            let collapsed = hooks.collapsed.contains(&file.file) || viewed;
            let mark_c = controller.clone();
            let mark_sid = sid.clone();
            let mark_file = file.file.clone();
            let fingerprint = file.fingerprint;
            let open_c = controller.clone();
            let open_file = file.file.clone();
            let toggle = hooks.toggle.clone();
            let toggle_file = file.file.clone();
            let (dir, name) = match file.file.rsplit_once('/') {
                Some((d, n)) => (format!("{d}/"), n.to_string()),
                None => (String::new(), file.file.clone()),
            };
            col = col.child(
                v_flex()
                    .w_full()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("file-head-{i}")))
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.secondary.opacity(0.5)))
                            .on_click(move |_, window, cx| toggle(toggle_file.clone(), window, cx))
                            .child(
                                gpui_kit::component::Icon::new(if collapsed {
                                    gpui_kit::component::IconName::ChevronRight
                                } else {
                                    gpui_kit::component::IconName::ChevronDown
                                })
                                .xsmall(),
                            )
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .child(
                                        div()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(dir),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(name),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(file.status.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.success)
                                    .child(format!("+{}", file.additions)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.danger)
                                    .child(format!("−{}", file.deletions)),
                            )
                            .child(
                                Button::new(SharedString::from(format!("viewed-{i}")))
                                    .label(if viewed { "✓ Viewed" } else { "Viewed" })
                                    .ghost()
                                    .xsmall()
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        mark_c.update(cx, |c, cx| {
                                            c.mark_viewed(
                                                mark_sid.clone(),
                                                mark_file.clone(),
                                                fingerprint,
                                                cx,
                                            )
                                        })
                                    }),
                            )
                            .child({
                                let path = file.file.clone();
                                Button::new(SharedString::from(format!("open-tab-{i}")))
                                    .icon(gpui_kit::component::IconName::File)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Open in a tab")
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        window.dispatch_action(
                                            Box::new(super::keys::OpenFile {
                                                path: path.clone(),
                                                line: None,
                                            }),
                                            cx,
                                        );
                                    })
                            })
                            .child(
                                Button::new(SharedString::from(format!("open-file-{i}")))
                                    .icon(gpui_kit::component::IconName::ExternalLink)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Open in external editor")
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        open_c
                                            .update(cx, |c, cx| c.open_file(open_file.clone(), cx))
                                    }),
                            ),
                    )
                    .when(!collapsed && !file.patch.is_empty(), |el| {
                        el.child(div().px_2().pb_2().child(patch_view_with(
                            &format!("diff-{sid}-{i}"),
                            &file.patch,
                            PatchOpts {
                                max_h: None,
                                on_comment: Some((hooks.on_comment)(&file.file)),
                                below_line: (hooks.below)(&file.file),
                                show_copy: false,
                                language: Some(file.file.clone()),
                            },
                            cx,
                        )))
                    })
                    .when(file.more && !collapsed, |el| {
                        el.child(
                            div()
                                .px_2()
                                .pb_2()
                                .text_xs()
                                .text_color(theme.warning)
                                .child("Patch truncated by opencode."),
                        )
                    })
                    .when(file.patch.is_empty() && !collapsed, |el| {
                        el.child(
                            div()
                                .px_2()
                                .pb_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("No textual patch available."),
                        )
                    }),
            );
        }
    }
    col.into_any_element()
}

/// Called with (new-side line number, line text) when a line's comment
/// button is pressed.
pub type OnLineComment = std::rc::Rc<dyn Fn(u32, String, &mut Window, &mut App)>;

/// How a patch renders beyond the defaults.
#[derive(Default)]
pub struct PatchOpts {
    /// Scroll the patch past this height; `None` shows it whole.
    pub max_h: Option<f32>,
    pub on_comment: Option<OnLineComment>,
    /// An element (a comment editor or saved comments) shown under a
    /// new-side line.
    pub below_line: Vec<(u32, AnyElement)>,
    pub show_copy: bool,
    /// The file's path (or a language name), for syntax colours.
    pub language: Option<String>,
}

/// One displayable line of a unified diff.
#[derive(Clone, Debug, PartialEq)]
pub enum PatchLine {
    /// `@@ … @@ context`: shown as a quiet separator.
    Hunk(String),
    Context {
        old: u32,
        new: u32,
        text: String,
    },
    Added {
        new: u32,
        text: String,
    },
    Removed {
        old: u32,
        text: String,
    },
}

/// Parse a unified diff into display lines, dropping file headers
/// (`diff --git`, `index`, `Index:`, `===`, `---`/`+++`, modes, renames).
pub fn parse_patch(patch: &str) -> Vec<PatchLine> {
    let mut out = Vec::new();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // @@ -a,b +c,d @@ context
            let mut parts = rest.split_whitespace();
            let o = parts.next().unwrap_or("-0");
            let n = parts.next().unwrap_or("+0");
            let start = |s: &str| {
                s.get(1..)
                    .unwrap_or("")
                    .split(',')
                    .next()
                    .and_then(|x| x.parse::<u32>().ok())
                    .unwrap_or(0)
            };
            old = start(o);
            new = start(n);
            in_hunk = true;
            let context = rest.split_once("@@").map_or("", |x| x.1).trim().to_string();
            out.push(PatchLine::Hunk(context));
            continue;
        }
        let header = line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("Index:")
            || line.starts_with("===")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename ");
        if line.starts_with('\\') {
            // "\ No newline at end of file"
            continue;
        }
        if header && (!in_hunk || !(line.starts_with("--- ") || line.starts_with("+++ "))) {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(t) = line.strip_prefix('+') {
            out.push(PatchLine::Added {
                new,
                text: t.to_string(),
            });
            new += 1;
        } else if let Some(t) = line.strip_prefix('-') {
            out.push(PatchLine::Removed {
                old,
                text: t.to_string(),
            });
            old += 1;
        } else {
            let t = line.strip_prefix(' ').unwrap_or(line);
            out.push(PatchLine::Context {
                old,
                new,
                text: t.to_string(),
            });
            old += 1;
            new += 1;
        }
    }
    out
}

/// Syntax colours for a patch's lines. Each side (old: context and
/// removed lines; new: context and added) is highlighted as one text so
/// strings and comments spanning lines colour correctly.
struct SideHighlights {
    old: super::syntax::Spans,
    new: super::syntax::Spans,
    /// Per parsed line: which side and its byte range there.
    lines: Vec<Option<(bool, std::ops::Range<usize>)>>,
}

impl SideHighlights {
    fn line(&self, i: usize, len: usize) -> Vec<(std::ops::Range<usize>, &'static str)> {
        let Some(Some((new, r))) = self.lines.get(i) else {
            return Vec::new();
        };
        let spans = if *new { &self.new } else { &self.old };
        super::syntax::slice(spans, r.start..r.start + len.min(r.len()))
    }
}

type Prepared = std::rc::Rc<(Vec<PatchLine>, Option<SideHighlights>)>;

thread_local! {
    /// Parsed and highlighted patches by (patch, language): rendering runs
    /// every frame, parsing and highlighting only when the patch changes.
    static PREPARED: std::cell::RefCell<super::lru::Lru<Prepared>> =
        std::cell::RefCell::new(super::lru::Lru::new(128));
}

fn prepare(patch: &str, language: Option<&str>) -> Prepared {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (patch, language).hash(&mut h);
    let key = h.finish();
    if let Some(hit) = PREPARED.with(|c| c.borrow_mut().get(key)) {
        return hit;
    }
    let parsed = parse_patch(patch);
    let colours = side_highlights(&parsed, language);
    let prepared: Prepared = std::rc::Rc::new((parsed, colours));
    PREPARED.with(|c| c.borrow_mut().insert(key, prepared.clone()));
    prepared
}

fn side_highlights(parsed: &[PatchLine], language: Option<&str>) -> Option<SideHighlights> {
    let lang = language.filter(|l| super::syntax::supported(l))?;
    let (mut old, mut new) = (String::new(), String::new());
    let mut lines = Vec::with_capacity(parsed.len());
    let push = |side: &mut String, text: &str| {
        let start = side.len();
        side.push_str(text);
        side.push('\n');
        start..start + text.len()
    };
    for l in parsed {
        lines.push(match l {
            PatchLine::Hunk(_) => None,
            PatchLine::Context { text, .. } => {
                push(&mut old, text);
                Some((true, push(&mut new, text)))
            }
            PatchLine::Added { text, .. } => Some((true, push(&mut new, text))),
            PatchLine::Removed { text, .. } => Some((false, push(&mut old, text))),
        });
    }
    Some(SideHighlights {
        old: super::syntax::highlight(lang, &old),
        new: super::syntax::highlight(lang, &new),
        lines,
    })
}

pub fn patch_view(id: &str, patch: &str, language: Option<&str>, cx: &App) -> AnyElement {
    patch_view_with(
        id,
        patch,
        PatchOpts {
            max_h: Some(400.),
            show_copy: true,
            language: language.map(str::to_string),
            ..Default::default()
        },
        cx,
    )
}

/// A unified diff with old/new line numbers and tinted rows.
pub fn patch_view_with(id: &str, patch: &str, opts: PatchOpts, cx: &App) -> AnyElement {
    const MAX_LINES: usize = 800;
    let theme = cx.theme();
    let copy = patch.to_string();
    let prepared = prepare(patch, opts.language.as_deref());
    let (parsed, colours) = (&prepared.0, &prepared.1);
    let total = parsed.len();
    let mut below = opts.below_line;
    let gutter = |n: Option<u32>| {
        div()
            .w(px(34.))
            .flex_none()
            .pr_1()
            .text_right()
            .text_color(theme.muted_foreground.opacity(0.7))
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    let mut lines = v_flex()
        .w_full()
        .font_family(theme.mono_font_family.clone())
        .text_xs();
    for (i, line) in parsed.iter().take(MAX_LINES).cloned().enumerate() {
        let (old, new, sign, text, bg, fg) = match line {
            PatchLine::Hunk(ctx) => {
                lines = lines.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_0p5()
                        .bg(theme.secondary.opacity(0.6))
                        .text_color(theme.muted_foreground)
                        .child(if ctx.is_empty() {
                            "⋯".to_string()
                        } else {
                            format!("⋯ {ctx}")
                        }),
                );
                continue;
            }
            PatchLine::Context { old, new, text } => {
                (Some(old), Some(new), " ", text, None, theme.foreground)
            }
            PatchLine::Added { new, text } => (
                None,
                Some(new),
                "+",
                text,
                Some(theme.success.opacity(0.14)),
                theme.foreground,
            ),
            PatchLine::Removed { old, text } => (
                Some(old),
                None,
                "-",
                text,
                Some(theme.danger.opacity(0.14)),
                theme.foreground,
            ),
        };
        // A line can itself be enormous (minified files).
        let preview: String = text.chars().take(2000).collect();
        let group = SharedString::from(format!("{id}-l{i}"));
        let comment = opts.on_comment.clone().zip(new).map(|(f, n)| {
            let line_text = preview.clone();
            div()
                .id(SharedString::from(format!("{id}-c{i}")))
                .absolute()
                .left(px(2.))
                .top_0()
                .px_1()
                .rounded_sm()
                .bg(theme.primary)
                .text_color(theme.primary_foreground)
                .cursor_pointer()
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .child("+")
                .on_click(move |_, window, cx| f(n, line_text.clone(), window, cx))
        });
        lines = lines.child(
            h_flex()
                .group(group.clone())
                .relative()
                .w_full()
                .when_some(bg, |el, bg| el.bg(bg))
                .child(gutter(old))
                .child(gutter(new))
                .child(
                    div()
                        .w(px(14.))
                        .flex_none()
                        .text_color(match sign {
                            "+" => theme.success,
                            "-" => theme.danger,
                            _ => theme.muted_foreground,
                        })
                        .child(sign),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(fg)
                        .whitespace_nowrap()
                        .child(match &colours {
                            Some(c) => {
                                let spans = c.line(i, preview.len());
                                super::syntax::styled(preview, &spans, cx).into_any_element()
                            }
                            None => preview.into_any_element(),
                        }),
                )
                .children(comment),
        );
        if let Some(n) = new {
            while let Some(pos) = below.iter().position(|(l, _)| *l == n) {
                let (_, el) = below.remove(pos);
                lines = lines.child(div().w_full().pl(px(82.)).py_1().child(el));
            }
        }
    }
    let body = div()
        .id(SharedString::from(id.to_string()))
        .w_full()
        .overflow_x_scroll()
        .child(lines);
    v_flex()
        .w_full()
        .gap_1()
        .when(opts.show_copy, |el| {
            el.child(
                h_flex().justify_end().child(
                    Button::new(SharedString::from(format!("copy-{id}")))
                        .label("Copy patch")
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                ),
            )
        })
        .child(match opts.max_h {
            Some(h) => div()
                .id(SharedString::from(format!("{id}-scroll")))
                .max_h(px(h))
                .overflow_y_scroll()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(body)
                .into_any_element(),
            None => div()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(body)
                .into_any_element(),
        })
        .when(total > MAX_LINES, |el| {
            el.child(
                div()
                    .text_color(theme.muted_foreground)
                    .text_xs()
                    .child(format!(
                        "Showing {MAX_LINES} of {total} lines. Copy includes the full patch."
                    )),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod patch_tests {
    use super::{PatchLine, parse_patch};

    #[::core::prelude::v1::test]
    fn strips_headers_and_numbers_lines() {
        let p = "Index: a.rs\n===================================================================\ndiff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -3,2 +3,3 @@ fn main()\n keep\n-old\n+new\n+more\n";
        assert_eq!(
            parse_patch(p),
            vec![
                PatchLine::Hunk("fn main()".into()),
                PatchLine::Context {
                    old: 3,
                    new: 3,
                    text: "keep".into()
                },
                PatchLine::Removed {
                    old: 4,
                    text: "old".into()
                },
                PatchLine::Added {
                    new: 4,
                    text: "new".into()
                },
                PatchLine::Added {
                    new: 5,
                    text: "more".into()
                },
            ]
        );
    }
}
