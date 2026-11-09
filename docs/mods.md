# Mods (experimental spike)

A mod is a folder with a `main.js` that draws a panel in ocdesk. It is
JavaScript that *describes* GPUI elements (`div()`, `h_flex()`, `.on_click`).
Rust draws them, so there is no webview, DOM, HTML or CSS. The runtime is
gpui-kit's [`gpui-shell`](https://crates.io/crates/gpui-shell) (QuickJS), the
same one herdr uses for its mods.

This is behind the off-by-default cargo feature `mods`:

```sh
OCDESK_MODS_DIR=$PWD/dev/mods cargo run --features mods
```

Open the **Mods** tab (plug icon) in the conversation header. Every
subfolder of `$OCDESK_MODS_DIR` (default `<config>/ocdesk/mods`) with a
`main.js` is loaded the first time the tab is shown. Saving a mod's file
reloads it.

## Writing one

```js
import { View, div } from "gpui-kit";
import { v_flex } from "gpui-base";
import { sessions, theme, selectSession } from "ocdesk";

export default class Hello extends View {
  render(cx) {
    const t = theme();
    return v_flex().children(
      sessions().map((s) =>
        div().id(s.id).child(s.title).text_color(t.foreground).on_click(() => selectSession(s.id)),
      ),
    );
  }
}
```

[`dev/mods/sessions`](../dev/mods/sessions/main.js) is a complete example.

## The `ocdesk` module

| Function | Returns | Re-renders on |
| --- | --- | --- |
| `sessions()` | top-level sessions `{id, title, directory, updated, busy, unread}` | `SessionsChanged`, `StatusChanged`, `FoldersChanged` |
| `selected()` | selected session id or `null` | `SelectionChanged` |
| `connection()` | `"starting" \| "connecting" \| "connected" \| "reconnecting" \| "down" \| null` | `ConnectionChanged` |
| `folders()` | project folder paths | `FoldersChanged` |
| `theme()` | ocdesk colours as `#rrggbbaa` | theme change |
| `selectSession(id)` | `undefined`; unknown ids are ignored | — |

Reads are tracked per mod (`src/mods/reads.rs`). A mod is refreshed only
when a `StoreEvent` changes something it read. Transcript events
(`MessageChanged`, …) refresh no mod.

## Boundaries

- **Read-only state plus intents.** Host reads come straight from the
  `Store`. Actions are queued on a channel and applied by the panel through
  the `Controller` after the script call returns. A host function must not
  re-enter the engine, and a synchronous store update would, because its
  events refresh mods.
- **No permission or question replies.** ocdesk never answers them on the
  user's behalf, and neither can a mod.
- **Presentation only.** Agent behaviour belongs in opencode's server-side
  plugins, not in ocdesk.
- **Capabilities.** A mod gets its own `localStorage` file
  (`<config>/ocdesk/mod-data/<name>.json`) and nothing else: no files,
  processes, network or clipboard.

## Known limitations (spike findings)

- Script dialogs, toasts and tooltips need the *window's* root to be a
  `ShellRoot`. In ocdesk it isn't, so they do nothing when called from an
  embedded mod.
- gpui-shell calls itself experimental ("M0"). Its API can change with any
  gpui-kit release, and it must be bumped in lockstep with `gpui-kit`.
- Adding the optional dependency changes `Cargo.lock` for every build. Cargo
  resolves optional dependencies too, and gpui-shell's optional `gpui-fast`
  path pins `taffy =0.15.0`, downgrading GPUI's layout engine from 0.15.1
  even with `mods` off.
- Loading a mod writes editor files into its folder: `gpui-kit.d.ts` (about
  800 KB, rewritten on every load, and it includes the `ocdesk` module's
  declarations from `src/mods/host.rs`) and, once, `jsconfig.json`.
- No `ocdesk mod check` command yet (gpui-shell has `ShellRuntime::check`).
