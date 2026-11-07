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

## Surprises

- Linux build needs `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev
  libvulkan-dev libx11-xcb-dev libxcb-xkb-dev libfontconfig-dev libssl-dev
  libasound2-dev libzstd-dev`. Rendering is wgpu (Vulkan); headless CI works
  with Xvfb + mesa llvmpipe (`mesa-vulkan-drivers`).
- Under Xvfb gpui logs `Found no xinput mouse pointers` (scroll-device
  detection), but `xdotool` clicks still reach the window.
- `.SystemUIFont` falls back to DejaVu Sans on a bare Linux box.
- A clean build of the gpui stack takes ~10 min in this container; keep
  `[profile.dev.package."*"] opt-level = 2` so dependencies are fast in debug.
