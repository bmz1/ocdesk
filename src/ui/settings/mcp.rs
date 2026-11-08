//! Project-scoped MCP status and a short path to configuring a real server.
use super::*;
use crate::api::model::McpServer;
use std::rc::Rc;

type Configure = Rc<dyn Fn(Option<PathBuf>, &mut Window, &mut App)>;
const LOCAL_EXAMPLE: &str = r#"{
  "mcp": {
    "servers": {
      "playwright": {
        "type": "local",
        "command": ["npx", "-y", "@playwright/mcp@latest"]
      }
    }
  }
}"#;
const REMOTE_EXAMPLE: &str = r#"{
  "mcp": {
    "servers": {
      "my-server": {
        "type": "remote",
        "url": "https://your-server.example/mcp"
      }
    }
  }
}"#;

pub struct McpPanel {
    controller: Entity<Controller>,
    projects: Vec<PathBuf>,
    project: Option<PathBuf>,
    servers: Vec<McpServer>,
    loaded: bool,
    loading: bool,
    pending: Option<String>,
    error: Option<String>,
    configure: Configure,
    _sub: Subscription,
}

impl McpPanel {
    pub fn new(
        controller: Entity<Controller>,
        prefs: &Settings,
        configure: Configure,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = controller.read(cx).view_store();
        let project = store
            .read(cx)
            .target_folder()
            .or_else(|| prefs.folders.first().cloned());
        let sub = cx.subscribe(&store, |this, _, event, cx| {
            if this.loaded
                && matches!(
                    event,
                    crate::store::StoreEvent::FolderInfoChanged
                        | crate::store::StoreEvent::ConnectionChanged
                )
            {
                this.refresh(cx);
            }
        });
        Self {
            controller,
            projects: prefs.folders.clone(),
            project,
            servers: Vec::new(),
            loaded: false,
            loading: false,
            pending: None,
            error: None,
            configure,
            _sub: sub,
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request(None, cx);
    }

    fn request(&mut self, change: Option<(String, bool)>, cx: &mut Context<Self>) {
        if self.loading || self.pending.is_some() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let rx = match self
            .controller
            .read(cx)
            .settings_mcp_servers(project, change.clone())
        {
            Ok(rx) => rx,
            Err(e) => {
                self.error = Some(format!("Could not load MCP servers: {e:#}"));
                cx.notify();
                return;
            }
        };
        self.loading = true;
        self.pending = change.map(|c| c.0);
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = rx
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("MCP request was interrupted")));
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                this.pending = None;
                match result {
                    Ok(mut servers) => {
                        servers.sort_by(|a, b| {
                            b.connected()
                                .cmp(&a.connected())
                                .then_with(|| a.name.cmp(&b.name))
                        });
                        this.servers = servers;
                        this.loaded = true;
                    }
                    Err(e) => this.error = Some(format!("MCP: {e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn status(server: &McpServer) -> &str {
    match server.status.as_str() {
        "connected" => "Connected",
        "pending" => "Connecting…",
        "disabled" => "Disabled",
        "failed" => "Connection failed",
        "needs_auth" => "Authentication required",
        other => other,
    }
}

impl Render for McpPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let projects = self.projects.clone();
        let selected = self.project.clone();
        let weak = cx.weak_entity();
        let chooser = Button::new("mcp-project")
            .label(
                self.project
                    .as_deref()
                    .map(super::super::folder_name)
                    .unwrap_or_else(|| "Choose a project".into()),
            )
            .outline()
            .small()
            .dropdown_caret(true)
            .disabled(self.loading || projects.is_empty())
            .dropdown_menu(move |mut menu, _, _| {
                for project in &projects {
                    let weak = weak.clone();
                    let project = project.clone();
                    menu = menu.item(
                        PopupMenuItem::new(project.display().to_string())
                            .checked(selected.as_ref() == Some(&project))
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    this.project = Some(project.clone());
                                    this.servers.clear();
                                    this.loaded = false;
                                    this.error = None;
                                    this.refresh(cx);
                                });
                            }),
                    );
                }
                menu
            });
        let active = self.servers.iter().filter(|s| s.connected()).count();
        let mut list =
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(chooser)
                        .child(
                            Button::new("mcp-refresh")
                                .label(if self.loading {
                                    "Refreshing…"
                                } else {
                                    "Refresh"
                                })
                                .ghost()
                                .small()
                                .disabled(self.loading || self.project.is_none())
                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                        ),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(
                    "Shows servers available to this project, including global configuration.",
                ));
        if self.loaded {
            list = list.child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!(
                        "{active} active · {} configured",
                        self.servers.len()
                    )),
            );
        }
        if let Some(error) = &self.error {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if self.project.is_none() {
            list = list.child(
                div()
                    .text_sm()
                    .child("Add a project to inspect its MCP connections."),
            );
        } else if self.loaded && self.servers.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .text_sm()
                    .child("No MCP servers configured for this project yet."),
            );
        }
        for server in &self.servers {
            let name = server.name.clone();
            let connect = !server.connected();
            let pending = self.pending.as_ref() == Some(&name);
            let color = if server.connected() {
                theme.success
            } else if server.error.is_some() {
                theme.danger
            } else {
                theme.muted_foreground
            };
            list = list.child(v_flex().gap_1().p_3().rounded_lg().border_1().border_color(theme.border)
                .child(h_flex().gap_3().items_center()
                    .child(div().size(px(7.)).rounded_full().bg(color))
                    .child(v_flex().flex_1().min_w_0().gap_0p5().child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name.clone()))
                        .child(div().text_xs().text_color(color).child(if pending { if connect { "Connecting…" } else { "Disconnecting…" } } else { status(server) }.to_string())))
                    .child(Button::new(SharedString::from(format!("mcp-toggle-{name}"))).label(if connect { "Connect" } else { "Disconnect" }).outline().small()
                        .disabled(self.loading || server.status == "pending")
                        .on_click(cx.listener(move |this, _, _, cx| this.request(Some((name.clone(), connect)), cx)))))
                .when_some(server.error.clone(), |el, error| el.child(div().text_xs().text_color(theme.danger).child(error)))
                .when(server.status == "needs_auth", |el| el.child(div().text_xs().text_color(theme.muted_foreground).child("Complete this server's authentication in OpenCode, then refresh its status."))));
        }
        let configure_global = self.configure.clone();
        let configure_project = self.configure.clone();
        let project = self.project.clone();
        list.child(v_flex().gap_2().mt_3()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Set up an MCP server"))
            .child(div().text_xs().text_color(theme.muted_foreground).child("1. Open global config to share a server across projects, or project config to keep it local."))
            .child(h_flex().gap_2()
                .child(Button::new("mcp-global-config").label("Open global config").small().outline().on_click(move |_, window, cx| configure_global(None, window, cx)))
                .child(Button::new("mcp-project-config").label("Open project config").small().outline().disabled(project.is_none())
                    .on_click(move |_, window, cx| configure_project(project.clone(), window, cx))))
            .child(div().text_xs().text_color(theme.muted_foreground).child("2. Add a uniquely named entry under mcp.servers. Merge it into your existing config."))
            .child(super::super::tool_card::text_panel("mcp-local-example", "Local example · Playwright (requires Node.js / npx)", LOCAL_EXAMPLE, cx))
            .child(super::super::tool_card::text_panel("mcp-remote-example", "Remote example · replace the URL", REMOTE_EXAMPLE, cx))
            .child(div().text_xs().text_color(theme.muted_foreground).child("3. Save configuration and reload OpenCode when sessions are idle. Return here and Refresh to check the connection. OAuth servers may require authentication; keep API keys in environment variables."))
            .child(Button::new("mcp-documentation").label("MCP setup and authentication guide ↗").ghost().small()
                .on_click(|_, _, cx| cx.open_url("https://opencode.ai/v2/docs/mcp-servers"))))
    }
}
