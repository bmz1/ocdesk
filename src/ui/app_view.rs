//! Root layout: sidebar | conversation.

use gpui_kit::component::{ActiveTheme as _, Theme, h_flex};
use gpui_kit::*;

use crate::controller::Controller;
use crate::store::Store;
use crate::ui::conversation::ConversationView;
use crate::ui::sidebar::Sidebar;

pub struct AppView {
    sidebar: Entity<Sidebar>,
    conversation: Entity<ConversationView>,
    first_frame_logged: bool,
    _subs: Vec<Subscription>,
}

impl AppView {
    pub fn new(
        rt: tokio::runtime::Handle,
        working_dir: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Theme::sync_system_appearance(Some(window), cx);
        let store = cx.new(|_| Store::default());
        let controller = cx.new(|cx| Controller::new(rt, store.clone(), working_dir, cx));
        if let Ok(p) = std::env::var("OCDESK_PROBE_PROMPT") {
            controller.update(cx, |c, cx| c.probe_on_connect(p, cx));
        }
        let sidebar = cx.new(|cx| Sidebar::new(store.clone(), controller.clone(), cx));
        let conversation = cx.new(|cx| ConversationView::new(store, controller, window, cx));
        let subs = vec![cx.observe_window_appearance(window, |_, window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        })];
        Self {
            sidebar,
            conversation,
            first_frame_logged: false,
            _subs: subs,
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            if let Some(t) = crate::STARTED.get() {
                tracing::info!(ms = t.elapsed().as_millis() as u64, "first frame rendered");
            }
        }
        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.sidebar.clone())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.conversation.clone()),
            )
    }
}
