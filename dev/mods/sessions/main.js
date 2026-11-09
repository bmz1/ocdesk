// Example ocdesk mod: the sessions list, busy ones first, with the connection
// state on top. Click a row to select that session.
//
// Try it:  OCDESK_MODS_DIR=$PWD/dev/mods cargo run --features mods
// then open the Mods tab (plug icon) in the conversation header.
// Save this file to hot-reload it.

import { View, div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { sessions, selected, connection, theme, selectSession } from "ocdesk";

export default class Sessions extends View {
  init() {
    // Per-mod localStorage, kept in <config>/ocdesk/mod-data/sessions.json.
    this.picks = Number(localStorage.getItem("picks") ?? 0);
  }

  pick(id, cx) {
    this.picks += 1;
    localStorage.setItem("picks", String(this.picks));
    selectSession(id);
    cx.notify();
  }

  render(cx) {
    const t = theme();
    const current = selected();
    const list = [...sessions()].sort((a, b) => Number(b.busy) - Number(a.busy) || b.updated - a.updated);
    const busy = list.filter((s) => s.busy).length;

    const row = (s) =>
      h_flex()
        .id(`session-${s.id}`)
        .px(12)
        .py(6)
        .gap(8)
        .items_center()
        .cursor_pointer()
        .when(s.id === current, (el) => el.bg(t.listActive))
        .hover((el) => el.bg(t.listHover))
        .on_click((_event, cx) => this.pick(s.id, cx))
        .child(div().size(8).rounded(4).flex_none().bg(s.busy ? t.warning : s.unread ? t.primary : t.border))
        .child(div().flex_1().truncate().text_size(13).text_color(t.foreground).child(s.title || "Untitled"));

    return v_flex()
      .size_full()
      .bg(t.background)
      .child(
        h_flex()
          .px(12)
          .py(8)
          .gap(8)
          .items_center()
          .text_size(12)
          .text_color(t.mutedForeground)
          .child(div().flex_1().child(`${list.length} sessions · ${busy} busy`))
          .child(div().child(connection() ?? "offline")),
      )
      .children(list.map(row))
      .child(div().px(12).py(8).text_size(11).text_color(t.mutedForeground).child(`picked from here: ${this.picks}`));
  }
}
