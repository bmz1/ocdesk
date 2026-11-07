//! The Terminal tab of the right panel: the selected session's opencode
//! PTY (see `api::pty`), emulated with `alacritty_terminal` and drawn as
//! styled monospace rows.
//!
//! One terminal per session, created on first open and kept by opencode, so
//! it survives switching sessions and restarts of ocdesk (the snapshot
//! restores the screen). Keys go to the shell while the terminal is
//! focused; app shortcuts with Cmd/Ctrl+Shift still work.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions as _, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as TColor, NamedColor, Processor, Rgb};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use crate::api::pty::{PtyMsg, frame};
use crate::controller::Controller;
use crate::store::{Store, StoreEvent};

const FONT_SIZE: f32 = 12.5;
const LINE_HEIGHT: f32 = 1.3;

/// Collects what the emulator wants written back to the PTY (answers to
/// cursor-position and similar queries).
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<String>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        if let TermEvent::PtyWrite(s) = event
            && let Ok(mut q) = self.0.lock()
        {
            q.push(s);
        }
    }
}

struct Session {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    input: Option<UnboundedSender<Vec<u8>>>,
    status: Status,
    _io: Option<Task<()>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Connecting,
    Live,
    Exited(Option<i64>),
    Failed(String),
}

pub struct TerminalView {
    store: Entity<Store>,
    controller: Entity<Controller>,
    focus: FocusHandle,
    /// Per session id.
    sessions: HashMap<String, Session>,
    cols: u16,
    rows: u16,
    cell: Option<Size<Pixels>>,
    _subs: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(
        store: Entity<Store>,
        controller: Entity<Controller>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subs = vec![cx.subscribe(&store, |_, _, ev: &StoreEvent, cx| {
            if matches!(ev, StoreEvent::SelectionChanged) {
                cx.notify();
            }
        })];
        Self {
            store,
            controller,
            focus: cx.focus_handle(),
            sessions: HashMap::new(),
            cols: 80,
            rows: 24,
            cell: None,
            _subs: subs,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn selected(&self, cx: &App) -> Option<String> {
        self.store.read(cx).selected.clone()
    }

    /// Open (or reattach) the selected session's terminal.
    fn ensure(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.selected(cx) else { return };
        if self.sessions.contains_key(&sid) {
            return;
        }
        let listener = Listener::default();
        let size = TermSize::new(self.cols as usize, self.rows as usize);
        let config = Config {
            scrolling_history: 5_000,
            ..Config::default()
        };
        self.sessions.insert(
            sid.clone(),
            Session {
                term: Term::new(config, &size, listener.clone()),
                parser: Processor::new(),
                listener,
                input: None,
                status: Status::Connecting,
                _io: None,
            },
        );
        let Some((api, rt)) = self.controller.read(cx).api_and_runtime() else {
            self.set_status(&sid, Status::Failed("opencode is not running".into()), cx);
            return;
        };
        let (cols, rows) = (self.cols, self.rows);
        let attachment = format!("ocdesk-{:x}", rand::random::<u64>());
        let session = sid.clone();
        let opened = rt.spawn(async move {
            let existing = api.session_terminals(&session).await?;
            let info = match existing.into_iter().find(|p| p.exit_code.is_none()) {
                Some(p) => p,
                None => api.create_terminal(&session, cols, rows).await?,
            };
            let (info, checkpoint) = api.pty_snapshot(&info.id).await?;
            let (tx, rx) = api.connect_pty(&info.id, info.tail, &attachment).await?;
            anyhow::Ok((info, checkpoint, tx, rx))
        });
        let key = sid.clone();
        let task = cx.spawn(async move |this, cx| {
            let opened = match opened.await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!(e)),
            };
            let (info, checkpoint, tx, mut rx) = match opened {
                Ok(x) => x,
                Err(e) => {
                    let _ = this.update(cx, |this, cx| {
                        this.set_status(&sid, Status::Failed(format!("{e:#}")), cx)
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                let (cols, rows) = (this.cols, this.rows);
                if let Some(s) = this.sessions.get_mut(&sid) {
                    s.feed(&checkpoint);
                    // Claim the viewport size; the server resizes the PTY.
                    let _ = tx.unbounded_send(frame(cols, rows, None));
                    s.input = Some(tx);
                    s.status = match info.exit_code {
                        Some(code) => Status::Exited(Some(code)),
                        None => Status::Live,
                    };
                }
                cx.notify();
            });
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    if let Some(s) = this.sessions.get_mut(&sid) {
                        s.handle(msg);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        });
        if let Some(s) = self.sessions.get_mut(&key) {
            s._io = Some(task);
        }
    }

    fn set_status(&mut self, sid: &str, status: Status, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.get_mut(sid) {
            s.status = status;
        }
        cx.notify();
    }

    /// End the selected session's shell and start a fresh one.
    fn restart_shell(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.selected(cx) else { return };
        let Some((api, rt)) = self.controller.read(cx).api_and_runtime() else {
            return;
        };
        self.sessions.remove(&sid);
        let removing = rt.spawn(async move {
            for p in api.session_terminals(&sid).await.unwrap_or_default() {
                let _ = api.remove_pty(&p.id).await;
            }
        });
        cx.spawn(async move |this, cx| {
            let _ = removing.await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
        cx.notify();
    }

    /// Drop the selected session's terminal state and connect again.
    fn reconnect(&mut self, cx: &mut Context<Self>) {
        if let Some(sid) = self.selected(cx) {
            self.sessions.remove(&sid);
            self.ensure(cx);
        }
    }

    fn send(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(sid) = self.selected(cx) else { return };
        let (cols, rows) = (self.cols, self.rows);
        if let Some(s) = self.sessions.get_mut(&sid)
            && let Some(tx) = &s.input
        {
            let _ = tx.unbounded_send(frame(cols, rows, Some(bytes)));
            s.term.scroll_display(Scroll::Bottom);
        }
    }

    /// Fit the grid to the panel; tell the server when it changes.
    fn fit(&mut self, bounds: Bounds<Pixels>, cell: Size<Pixels>, cx: &mut Context<Self>) {
        self.cell = Some(cell);
        let cols = ((bounds.size.width / cell.width).floor() as u16).clamp(20, 500);
        let rows = ((bounds.size.height / cell.height).floor() as u16).clamp(5, 300);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        for s in self.sessions.values_mut() {
            s.term.resize(TermSize::new(cols as usize, rows as usize));
            if let Some(tx) = &s.input {
                let _ = tx.unbounded_send(frame(cols, rows, None));
            }
        }
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let app_cursor = self
            .selected(cx)
            .and_then(|sid| self.sessions.get(&sid))
            .is_some_and(|s| s.term.mode().contains(TermMode::APP_CURSOR));
        if let Some(bytes) = key_bytes(&ev.keystroke, app_cursor) {
            self.send(&bytes, cx);
            cx.stop_propagation();
        }
    }

    fn scroll(&mut self, lines: i32, cx: &mut Context<Self>) {
        if let Some(sid) = self.selected(cx)
            && let Some(s) = self.sessions.get_mut(&sid)
        {
            s.term.scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }
}

impl Session {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        let replies: Vec<String> = self
            .listener
            .0
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default();
        if let Some(tx) = &self.input {
            let (cols, rows) = (self.term.columns() as u16, self.term.screen_lines() as u16);
            for r in replies {
                let _ = tx.unbounded_send(frame(cols, rows, Some(r.as_bytes())));
            }
        }
    }

    fn handle(&mut self, msg: PtyMsg) {
        match msg {
            PtyMsg::Output(b) => self.feed(&b),
            PtyMsg::Control(v) => match v.get("type").and_then(|t| t.as_str()) {
                Some("exited") => {
                    self.status = Status::Exited(v.get("exitCode").and_then(|c| c.as_i64()))
                }
                Some("resized") => {
                    // The server's canonical size and a checkpoint of the
                    // screen at that size: reset and replay it.
                    let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80) as usize;
                    let rows = v.get("rows").and_then(|c| c.as_u64()).unwrap_or(24) as usize;
                    self.term.resize(TermSize::new(cols, rows));
                    if let Some(cp) = v.get("checkpoint").and_then(|c| c.as_str()) {
                        let bytes = crate::api::pty::decode_checkpoint(cp);
                        if !bytes.is_empty() {
                            self.feed(b"\x1bc");
                            self.feed(&bytes);
                        }
                    }
                }
                _ => {}
            },
            PtyMsg::Closed(reason) => {
                if !matches!(self.status, Status::Exited(_)) {
                    self.status = Status::Failed(
                        reason
                            .filter(|r| !r.is_empty())
                            .unwrap_or_else(|| "Disconnected".into()),
                    );
                }
                self.input = None;
            }
        }
    }
}

/// Bytes a keystroke sends to the shell, `None` for keys the app keeps
/// (Cmd shortcuts, Cmd/Ctrl+Shift app shortcuts).
fn key_bytes(k: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
    let m = &k.modifiers;
    if m.platform || (m.control && m.shift) {
        return None;
    }
    let csi = |c: char| {
        if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let special: Option<Vec<u8>> = match k.key.as_str() {
        "enter" => Some(b"\r".to_vec()),
        "backspace" => Some(if m.alt {
            b"\x1b\x7f".to_vec()
        } else {
            b"\x7f".to_vec()
        }),
        "tab" if m.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "escape" => Some(b"\x1b".to_vec()),
        "up" => Some(csi('A')),
        "down" => Some(csi('B')),
        "right" => Some(csi('C')),
        "left" => Some(csi('D')),
        "home" => Some(csi('H')),
        "end" => Some(csi('F')),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "space" if m.control => Some(vec![0]),
        _ => None,
    };
    if special.is_some() {
        return special;
    }
    if m.control {
        let c = k.key.chars().next()?;
        if k.key.chars().count() == 1 && c.is_ascii_alphabetic() {
            return Some(vec![(c.to_ascii_lowercase() as u8) & 0x1f]);
        }
        return match c {
            '[' => Some(vec![0x1b]),
            '\\' => Some(vec![0x1c]),
            ']' => Some(vec![0x1d]),
            _ => None,
        };
    }
    let text = k.key_char.clone().or_else(|| {
        (k.key.chars().count() == 1).then(|| {
            if m.shift {
                k.key.to_uppercase()
            } else {
                k.key.clone()
            }
        })
    })?;
    let mut bytes = Vec::new();
    if m.alt {
        bytes.push(0x1b);
    }
    bytes.extend_from_slice(text.as_bytes());
    Some(bytes)
}

/// xterm's 16 colours, then the 6×6×6 cube and the grey ramp.
fn indexed(i: u8, dark: bool) -> Hsla {
    const BASE: [u32; 16] = [
        0x1e1e1e, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd0d0d0, 0x5c6370,
        0xff7a85, 0xb5e890, 0xffd68a, 0x82c4ff, 0xde9bf0, 0x7fd4dd, 0xffffff,
    ];
    let rgb_u32 = match i {
        0..=15 => {
            let c = BASE[i as usize];
            // Light themes: keep black dark and white readable.
            if !dark && i == 7 {
                0x404040
            } else if !dark && i == 15 {
                0x202020
            } else {
                c
            }
        }
        16..=231 => {
            let n = i - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x as u32 * 40 };
            (v(n / 36) << 16) | (v((n / 6) % 6) << 8) | v(n % 6)
        }
        _ => {
            let g = 8 + (i as u32 - 232) * 10;
            (g << 16) | (g << 8) | g
        }
    };
    rgb(rgb_u32).into()
}

fn color(c: TColor, fg: Hsla, bg: Hsla, dark: bool) -> Hsla {
    match c {
        TColor::Spec(Rgb { r, g, b }) => {
            rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).into()
        }
        TColor::Indexed(i) => indexed(i, dark),
        TColor::Named(n) => match n {
            NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => fg,
            NamedColor::DimForeground => fg.opacity(0.7),
            NamedColor::Background => bg,
            n if (n as usize) < 16 => indexed(n as u8, dark),
            n => {
                // Dim variants map onto the base colours, dimmed.
                let base = (n as usize).saturating_sub(NamedColor::DimBlack as usize);
                indexed(base.min(7) as u8, dark).opacity(0.75)
            }
        },
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(sid) = self.selected(cx) else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Start or open a session to use its terminal.")
                .into_any_element();
        };
        self.ensure(cx);
        let font = Font {
            family: theme.mono_font_family.clone(),
            ..Font::default()
        };
        let font_size = px(FONT_SIZE);
        let cell = {
            let id = window.text_system().resolve_font(&font);
            let w = window
                .text_system()
                .advance(id, font_size, 'm')
                .map(|s| s.width)
                .unwrap_or(px(7.5));
            size(w, px(FONT_SIZE * LINE_HEIGHT))
        };
        let dark = theme.mode.is_dark();
        let (fg, bg) = (theme.foreground, theme.background);
        let session = self.sessions.get(&sid);
        let status = session.map(|s| s.status.clone());
        let mut lines: Vec<AnyElement> = Vec::new();
        if let Some(s) = session {
            let content = s.term.renderable_content();
            let cursor = content.cursor.point;
            let show_cursor = content.display_offset == 0
                && matches!(s.status, Status::Live)
                && self.focus.is_focused(window);
            let mut row_text = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            let mut current_line = None;
            let flush =
                |text: &mut String, runs: &mut Vec<TextRun>, lines: &mut Vec<AnyElement>| {
                    let t = std::mem::take(text);
                    let r = std::mem::take(runs);
                    lines.push(
                        div()
                            .h(cell.height)
                            .whitespace_nowrap()
                            .child(StyledText::new(t).with_runs(r))
                            .into_any_element(),
                    );
                };
            for indexed_cell in content.display_iter {
                let line = indexed_cell.point.line;
                if current_line != Some(line) {
                    if current_line.is_some() {
                        flush(&mut row_text, &mut runs, &mut lines);
                    }
                    current_line = Some(line);
                }
                let c = &indexed_cell.cell;
                if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let mut cfg = color(c.fg, fg, bg, dark);
                let mut cbg = color(c.bg, fg, bg, dark);
                if c.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut cfg, &mut cbg);
                }
                let at_cursor = show_cursor && indexed_cell.point == cursor;
                if at_cursor {
                    std::mem::swap(&mut cfg, &mut cbg);
                    cbg = theme.foreground;
                    cfg = theme.background;
                }
                if c.flags.contains(Flags::DIM) {
                    cfg = cfg.opacity(0.7);
                }
                let ch = if c.c == '\0' { ' ' } else { c.c };
                let len = ch.len_utf8();
                row_text.push(ch);
                let mut f = font.clone();
                if c.flags.contains(Flags::BOLD) {
                    f.weight = FontWeight::BOLD;
                }
                if c.flags.contains(Flags::ITALIC) {
                    f.style = FontStyle::Italic;
                }
                let run = TextRun {
                    len,
                    font: f,
                    color: cfg,
                    background_color: (cbg != bg).then_some(cbg),
                    underline: c.flags.contains(Flags::UNDERLINE).then(|| UnderlineStyle {
                        thickness: px(1.),
                        color: Some(cfg),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                match runs.last_mut() {
                    Some(last)
                        if last.font == run.font
                            && last.color == run.color
                            && last.background_color == run.background_color
                            && last.underline == run.underline =>
                    {
                        last.len += len
                    }
                    _ => runs.push(run),
                }
            }
            if current_line.is_some() {
                flush(&mut row_text, &mut runs, &mut lines);
            }
        }
        let view = cx.entity().downgrade();
        let fit = canvas(
            move |bounds, _, cx| {
                let _ = view.update(cx, |this, cx| this.fit(bounds, cell, cx));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let banner = match status {
            Some(Status::Connecting) => Some(("Connecting to the terminal…".to_string(), false)),
            Some(Status::Exited(code)) => Some((
                format!(
                    "The shell exited{}.",
                    code.map(|c| format!(" with {c}")).unwrap_or_default()
                ),
                true,
            )),
            Some(Status::Failed(e)) => Some((format!("Terminal unavailable: {e}"), true)),
            _ => None,
        };
        v_flex()
            .size_full()
            .bg(bg)
            .when_some(banner, |el, (text, retry)| {
                el.child(
                    h_flex()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().flex_1().child(text))
                        .child(
                            Button::new("terminal-new-shell")
                                .label("New shell")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| this.restart_shell(cx))),
                        )
                        .when(retry, |el| {
                            el.child(
                                Button::new("terminal-reconnect")
                                    .label("Reconnect")
                                    .xsmall()
                                    .outline()
                                    .on_click(cx.listener(|this, _, _, cx| this.reconnect(cx))),
                            )
                        }),
                )
            })
            .child(
                div()
                    .id("terminal")
                    .track_focus(&self.focus)
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .py_1()
                    .overflow_hidden()
                    .cursor_text()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(font_size)
                    .line_height(cell.height)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.focus.focus(window, cx)),
                    )
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| this.on_key(ev, cx)))
                    .on_scroll(cx.listener(move |this, ev: &ScrollEvent, _, cx| {
                        let dy = ev.delta.pixel_delta(cell.height).y;
                        let lines = (dy / cell.height).round() as i32;
                        if lines != 0 {
                            this.scroll(lines, cx);
                        }
                    }))
                    .child(div().absolute().inset_0().child(fit))
                    .children(lines),
            )
            .into_any_element()
    }
}

use gpui_kit::prelude::FluentBuilder as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(s: &'static str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[::core::prelude::v1::test]
    fn keys_map_to_terminal_bytes() {
        assert_eq!(key_bytes(&ks("enter"), false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes(&ks("ctrl-c"), false), Some(vec![3]));
        assert_eq!(key_bytes(&ks("up"), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes(&ks("up"), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes(&ks("cmd-k"), false), None);
        assert_eq!(key_bytes(&ks("ctrl-shift-t"), false), None);
    }
}
