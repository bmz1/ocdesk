#!/usr/bin/env python3
"""Generate assets/themes/opencode.json (a gpui-component theme set) from the
`@opencode/ui` tokens that Rune ships as data (anomalyco/rune,
themes/opencode/assets/theme.json, MIT).

    python3 dev/gen-opencode-theme.py path/to/rune/themes/opencode/assets/theme.json \
        assets/themes/opencode.json path/to/gpui-component/src/theme/default-theme.json

Only semantic tokens are mapped; the result is committed, so the build does
not need Rune.
"""
import json
import sys

src = json.load(open(sys.argv[1]))
out_path = sys.argv[2] if len(sys.argv) > 2 else "assets/themes/opencode.json"
# Syntax colours: gpui-component's default dark/light highlight styles
# (gpui-component/src/theme/default-theme.json), when given.
highlights = {}
if len(sys.argv) > 3:
    for t in json.load(open(sys.argv[3]))["themes"]:
        if "highlight" in t:
            highlights.setdefault(t["mode"], t["highlight"])


def hexa(v):
    # "#rrggbbaa" -> keep; gpui-component parses #rrggbbaa.
    return v


def theme(mode):
    t = src[mode]
    c = lambda k: hexa(t[k])
    colors = {
        "background": c("background-base"),
        "foreground": c("text-strong"),
        "border": c("border-weak-base"),
        "input.border": c("border-weak-base"),
        "ring": c("border-interactive-focus"),
        "caret": c("text-strong"),
        "selection.background": c("border-interactive-focus")[:7] + "55",
        "muted.background": c("surface-base"),
        "muted.foreground": c("text-base"),
        "secondary.background": c("surface-raised-base"),
        "secondary.foreground": c("text-strong"),
        "secondary.hover.background": c("surface-raised-base-hover"),
        "secondary.active.background": c("surface-raised-strong"),
        "accent.background": c("surface-raised-base-hover"),
        "accent.foreground": c("text-strong"),
        # Primary buttons are opencode's contrast buttons.
        "primary.background": c("button-primary-base"),
        "primary.foreground": c("background-base"),
        "primary.hover.background": c("text-strong"),
        "primary.active.background": c("text-base"),
        "popover.background": c("surface-float-base") if mode == "dark" else c("background-base"),
        "popover.foreground": c("text-strong"),
        "list.background": c("background-base"),
        "list.hover.background": c("surface-base-hover"),
        "list.active.background": c("surface-raised-base"),
        "list.active.border": c("border-weak-base"),
        "sidebar.background": c("background-stronger") if mode == "dark" else c("background-weak"),
        "sidebar.foreground": c("text-strong"),
        "sidebar.border": c("border-weaker-base"),
        "sidebar.accent.background": c("surface-raised-base"),
        "sidebar.accent.foreground": c("text-strong"),
        "sidebar.primary.background": c("button-primary-base"),
        "sidebar.primary.foreground": c("background-base"),
        "title_bar.background": c("background-base"),
        "title_bar.border": c("border-weaker-base"),
        "tab_bar.background": c("background-weak"),
        "tab.active.background": c("background-base"),
        "tab.active.foreground": c("text-strong"),
        "tab.foreground": c("text-base"),
        "danger.background": c("icon-critical-base"),
        "danger.foreground": "#ffffffff",
        "success.background": c("icon-success-base"),
        "success.foreground": "#ffffffff",
        "warning.background": c("icon-warning-base"),
        "warning.foreground": "#000000ff",
        "info.background": c("icon-interactive-base"),
        "info.foreground": "#ffffffff",
        "link.foreground": c("text-interactive-base"),
        "link.hover.foreground": c("text-strong"),
        "link.active.foreground": c("text-strong"),
        "scrollbar.background": "#00000000",
        "scrollbar.thumb.background": c("border-weak-hover"),
        "scrollbar.thumb.hover.background": c("border-strong-base"),
        "skeleton.background": c("surface-base"),
        "switch.background": c("surface-weaker"),
        "progress_bar.background": c("text-base"),
        "table.background": c("background-base"),
        "table.head.background": c("surface-base"),
        "table.row.border": c("border-weaker-base"),
        "status_bar.background": c("background-base"),
        "window.border": c("border-weak-base"),
        "created.background": c("surface-diff-add-base"),
        "deleted.background": c("surface-diff-delete-base"),
    }
    return {
        "name": f"opencode {'Dark' if mode == 'dark' else 'Light'}",
        "mode": mode,
        "is_default": False,
        "font.family": "Inter",
        "mono_font.family": "IBM Plex Mono",
        # A notch above the web UI's 14 px: the transcript is long-form reading.
        "font.size": src["numbers"]["font-size-base"] + 1,
        "mono_font.size": src["numbers"]["font-size-small"],
        "radius": src["numbers"]["radius-md"],
        "radius.lg": src["numbers"]["radius-lg"],
        "colors": colors,
        **({"highlight": highlights[mode]} if mode in highlights else {}),
    }


doc = {
    "name": "opencode",
    "author": "generated from @opencode/ui tokens (via anomalyco/rune)",
    "themes": [theme("dark"), theme("light")],
}
json.dump(doc, open(out_path, "w"), indent=2)
print(out_path)
