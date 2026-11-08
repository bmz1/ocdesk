//! Small shared components, named after their rune-kit / `@opencode/ui`
//! counterparts so screens read the same and a later port is mechanical.
//! Everything reads the active theme; nothing here knows about opencode.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex};
use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;

/// A row's state, shown in one fixed slot at its trailing edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    None,
    /// Working: a spinner.
    Running,
    /// Waiting on the user (approval or question): amber dot.
    NeedsYou,
    /// Finished in the background and not seen yet: accent dot.
    Unread,
    /// Last turn failed: red dot.
    Failed,
}

impl RowStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Running => "Working",
            Self::NeedsYou => "Needs your approval or answer",
            Self::Unread => "New activity",
            Self::Failed => "Last turn failed",
        }
    }
}

/// The status slot: spinner or a coloured dot, 16 px wide so titles align.
pub fn status_slot(status: RowStatus, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let dot = |c: Hsla| div().size(px(7.)).rounded_full().bg(c).into_any_element();
    let inner = match status {
        RowStatus::None => div().into_any_element(),
        RowStatus::Running => Spinner::new()
            .xsmall()
            .color(theme.muted_foreground)
            .into_any_element(),
        RowStatus::NeedsYou => dot(theme.warning),
        RowStatus::Unread => dot(theme.primary),
        RowStatus::Failed => dot(theme.danger),
    };
    h_flex()
        .w(px(16.))
        .flex_none()
        .justify_center()
        .child(inner)
        .into_any_element()
}

/// A project's coloured initial (Rune's sidebar avatar).
pub fn monogram(name: &str, size: f32, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "·".into());
    // A stable hue per name.
    let hash = name
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (hash % 360) as f32 / 360.;
    let bg = hsla(
        hue,
        0.55,
        if theme.mode.is_dark() { 0.42 } else { 0.55 },
        1.,
    );
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(size * 0.28))
        .bg(bg)
        .text_color(white())
        .text_size(px(size * 0.62))
        .font_weight(FontWeight::SEMIBOLD)
        .flex()
        .items_center()
        .justify_center()
        .child(initial)
        .into_any_element()
}

/// Context usage ring: accent below 75 %, warning to 90 %, then danger.
pub fn context_ring(id: impl Into<ElementId>, fraction: f32, cx: &App) -> ProgressCircle {
    let theme = cx.theme();
    let color = if fraction > 0.9 {
        theme.danger
    } else if fraction > 0.75 {
        theme.warning
    } else {
        theme.muted_foreground
    };
    ProgressCircle::new(id)
        .value((fraction * 100.).clamp(0., 100.))
        .color(color)
        .xsmall()
}

/// A quiet text dropdown trigger, as in the composer's bottom row
/// (`Build ⌄  Claude Opus 5.5 ⌄  High ⌄`).
pub fn quiet_select(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id)
        .label(label)
        .ghost()
        .xsmall()
        .dropdown_caret(true)
}

/// A callback that gets the window, for toast actions and the like.
pub type WindowAction = std::rc::Rc<dyn Fn(&mut Window, &mut App)>;

/// A short-lived toast (bottom of the window), optionally with one action
/// such as Undo.
pub fn toast(
    window: &mut Window,
    cx: &mut App,
    message: impl Into<SharedString>,
    action: Option<(&'static str, WindowAction)>,
) {
    let mut note = Notification::new().message(message);
    if let Some((label, f)) = action {
        note = note
            .action(move |_, _, _| {
                let f = f.clone();
                Button::new("toast-action")
                    .label(label)
                    .small()
                    .primary()
                    .on_click(move |_, window, cx| f(window, cx))
            })
            .autohide(true);
    }
    window.push_notification(note, cx);
}

/// A muted section header (sidebar groups, palette-like lists).
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}
