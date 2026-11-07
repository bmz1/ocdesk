# GPUI notes

Pinned: **`gpui-kit =0.9.0`**, which re-exports `gpui` as **`gpui-pre =0.3.11`**,
a crates.io snapshot of **zed@f816e1f** (from the crate description), plus
`gpui-pre-platform` (X11 + Wayland + font-kit on Linux) and
`gpui-component 0.9.0`. Sources to read before using any API:
`~/.cargo/registry/src/*/gpui-pre-0.3.11/` and `.../gpui-component-0.9.0/`.

## Why not a Zed git rev

The plan asked for `gpui` from a pinned Zed git rev. gpui-component 0.9.0
(now published from the `longbridge/gpui-kit` repo) depends on
`gpui-pre =0.3.11` from crates.io, not on Zed git. Mixing a git `gpui` with
gpui-component would compile two incompatible copies of gpui. Using the
snapshot gives the same thing, an exact pinned Zed rev, plus working
components. To bump: move `gpui-kit` and check the `gpui-pre` description for
the new Zed rev.

## Patterns used so far (verified against source)

- Bootstrap: `gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| { gpui_kit::init(cx); gpui_kit::open_window(opts, cx, |w, cx| cx.new(...)) })`.
  `open_window` wraps the view in gpui-base's `Root` (needed by components).
- `Context::spawn(async move |this: WeakEntity<T>, cx: &mut AsyncApp| ...)`
  (async closures; `this.update(cx, |this, cx| ...)` returns `Result`, `Err`
  once the entity is gone, which is the loop's exit condition).
- Off-thread work: tokio runtime on its own threads; results come back over
  `futures::channel::{mpsc, oneshot}` (executor-agnostic), awaited inside
  `cx.spawn`, then `cx.notify()`.
- `App::on_app_quit(|cx| async {})` runs on Cmd-Q / quit; used to kill the
  sidecar. On Linux `run()` returns after `cx.quit()`.
- `QuitMode::Default` = Explicit on macOS, LastWindowClosed elsewhere.
- `Button::disabled` comes from the `Disableable` trait (`gpui_kit::component::Disableable`).

- **Chat list:** `gpui_component::message_scroller::{MessageScroller, MessageScrollerState}`
  is a virtualized `gpui::list` with tail-following, a "Jump to latest" button
  and a scrollbar. Keep its row count in sync with `splice` / `reset`, and call
  `remeasure_items(i..i+1)` when one row's content changes (only the streaming
  row is re-laid out).
- **Multi-line input:** `TextareaState::new(window, cx).auto_grow(1, 10).submit_on_enter(true)`
  emits `InputEvent::PressEnter { secondary: false, shift: false }` on Enter;
  Shift+Enter inserts a newline.
- **Dialogs:** `window.open_alert_dialog(cx, |alert, window, cx| alert.title(..).description(el).confirm().on_ok(..))`
  (`WindowExt`). The builder re-runs on every rebuild, and the dialog steals
  focus on open: focus an input only on the first run, guarded by a `Cell<bool>`.
- **Menus:** `Button::dropdown_menu(|menu, _, _| menu.item(PopupMenuItem::new("Rename").on_click(..)))`.
- Hover-revealed controls: `.group(name)` on the row, then
  `.invisible().group_hover(name, |s| s.visible())` on the child.

## Surprises

- Rows in a scrolling `v_flex` **shrink** below their `h_8()` once the list
  overflows (flex-shrink); add `.flex_none()`.
- `Button` centers its content and ignores `justify_start()`; for a
  left-aligned sidebar item build an `h_flex` row with `.on_click` instead.
- Not GPUI, but bit us: dropping a tokio `AbortHandle` does **not** cancel the
  task. Call `.abort()`.
- Under Xvfb the theme can't detect the system appearance and stays light.

- Linux build needs `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
  libvulkan-dev libx11-xcb-dev libxcb-xkb-dev libfontconfig-dev libssl-dev
  libasound2-dev libzstd-dev`. Rendering is wgpu (Vulkan); headless CI works
  with Xvfb + mesa llvmpipe (`mesa-vulkan-drivers`).
- Under Xvfb gpui logs `Found no xinput mouse pointers` (scroll-device
  detection), but `xdotool` clicks still reach the window.
- `.SystemUIFont` falls back to DejaVu Sans on a bare Linux box.
- A clean build of the gpui stack takes ~10 min in this container; keep
  `[profile.dev.package."*"] opt-level = 2` so dependencies are fast in debug.
- `cx.prompt_for_paths(PathPromptOptions { directories: true, .. })` is the
  native folder picker. On Linux it goes through xdg-desktop-portal over
  D-Bus; without a session bus it fails with a ZBus error, so ocdesk falls
  back to a typed-path dialog.
- Theme: `Theme::change(ThemeMode::Dark, Some(window), cx)` at startup;
  `Theme::sync_system_appearance` + `observe_window_appearance` to follow the OS.
