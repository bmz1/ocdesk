//! agent-browser (<https://agent-browser.dev>): the browser automation CLI
//! agents drive through the shell. ocdesk shows the browser they drive.
//!
//! Everything here is read-only towards agent-browser. ocdesk never runs
//! the CLI against a session: a CLI call with launch options different
//! from the agent's (env, `--allow-file-access`, …) relaunches the browser
//! and loses the page. Instead (verified on agent-browser 0.41.2):
//!
//! - Each live session's daemon writes `<base>/<session>.stream` (the
//!   WebSocket stream port) and `<session>.pid`; `close` removes the
//!   `.stream` file. `<base>` is `$AGENT_BROWSER_SOCKET_DIR`, else
//!   `$XDG_RUNTIME_DIR/agent-browser`, else `~/.agent-browser`; a
//!   `--namespace NS` session lives in `<base>/namespaces/<ns>/run/`
//!   (lowercased). The agent's shell inherits ocdesk's environment through
//!   the opencode sidecar, so both resolve the same base.
//! - `ws://127.0.0.1:<port>/?maxFps=N` streams JSON messages: `frame`
//!   (base64 JPEG + `metadata.deviceWidth/Height`), `url`, `tabs`, `status`.
//!   It accepts `input_mouse` / `input_keyboard` messages in page CSS
//!   pixels. A native client (no browser `Origin`) is accepted.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::channel::mpsc::{Receiver, Sender, channel};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;

/// The session agent-browser uses without `--session`.
pub const DEFAULT_SESSION: &str = "default";

/// A running agent-browser session with a stream server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSession {
    pub name: String,
    /// Lowercased, as agent-browser stores it.
    pub namespace: Option<String>,
    pub port: u16,
}

impl LiveSession {
    pub fn key(&self) -> SessionKey {
        SessionKey {
            name: self.name.clone(),
            namespace: self.namespace.clone(),
        }
    }
}

/// Which browser session a command targets.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub name: String,
    pub namespace: Option<String>,
}

impl SessionKey {
    pub fn label(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Socket directories, in the order agent-browser picks the first of.
pub fn socket_base() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    env("AGENT_BROWSER_SOCKET_DIR")
        .or_else(|| env("XDG_RUNTIME_DIR").map(|d| d.join("agent-browser")))
        .or_else(|| dirs::home_dir().map(|h| h.join(".agent-browser")))
}

/// Live sessions, sorted by namespace then name.
pub fn live_sessions() -> Vec<LiveSession> {
    socket_base()
        .map(|b| sessions_in(&b, pid_alive))
        .unwrap_or_default()
}

fn sessions_in(base: &Path, alive: impl Fn(i32) -> bool) -> Vec<LiveSession> {
    let mut out = Vec::new();
    let mut scan = |dir: &Path, namespace: Option<String>| {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "stream") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(port) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
                .filter(|p| *p != 0)
            else {
                continue;
            };
            // A crashed daemon can leave its files behind.
            let pid = std::fs::read_to_string(path.with_extension("pid"))
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok());
            if pid.is_some_and(|p| !alive(p)) {
                continue;
            }
            out.push(LiveSession {
                name: name.to_string(),
                namespace: namespace.clone(),
                port,
            });
        }
    };
    scan(base, None);
    if let Ok(rd) = std::fs::read_dir(base.join("namespaces")) {
        for e in rd.flatten() {
            if let Some(ns) = e.file_name().to_str() {
                scan(&e.path().join("run"), Some(ns.to_string()));
            }
        }
    }
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    out
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks existence; EPERM means it exists under another user.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// One `agent-browser …` invocation found in a shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub session: SessionKey,
    /// The subcommand and its arguments, global flags removed
    /// (`open example.com`, `click @e3`).
    pub action: String,
}

/// Global flags that take a value (so their value isn't the subcommand).
const VALUE_FLAGS: &[&str] = &[
    "--session",
    "--namespace",
    "--executable-path",
    "--extension",
    "--init-script",
    "--enable",
    "--args",
    "--user-agent",
    "--proxy",
    "--proxy-bypass",
    "--profile",
    "--state",
    "--restore",
    "--restore-save",
    "--session-name",
    "--headers",
    "--cdp",
    "--cdp-headers",
    "--idle-timeout",
    "--color-scheme",
    "--download-path",
    "--screenshot-dir",
    "--screenshot-format",
    "--screenshot-quality",
    "--provider",
    "--device",
    "--engine",
    "--input-mode",
];

/// The agent-browser invocations in a shell command line, in order.
/// Understands `VAR=… agent-browser`, `npx/bunx/pnpm dlx agent-browser`,
/// `--session x` / `--session=x`, `--namespace`, and `&&`, `;`, `|` chains.
pub fn invocations(command: &str) -> Vec<Invocation> {
    let mut out = Vec::new();
    for segment in split_segments(command) {
        let words = shell_words(&segment);
        let mut env_session = None;
        let mut env_ns = None;
        let mut i = 0;
        // Leading `VAR=value` assignments.
        while let Some(w) = words.get(i) {
            match w.split_once('=') {
                Some((k, v))
                    if !k.is_empty()
                        && k.chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') =>
                {
                    match k {
                        "AGENT_BROWSER_SESSION" => env_session = Some(v.to_string()),
                        "AGENT_BROWSER_NAMESPACE" => env_ns = Some(v.to_string()),
                        _ => {}
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        let Some(at) = words[i..]
            .iter()
            .position(|w| w.rsplit('/').next() == Some("agent-browser"))
            .map(|p| p + i)
        else {
            continue;
        };
        // Only a launcher may precede it (`npx agent-browser`), not `echo`.
        let launcher_ok = words[i..at].iter().all(|w| {
            matches!(
                w.as_str(),
                "npx"
                    | "bunx"
                    | "pnpm"
                    | "dlx"
                    | "yarn"
                    | "-y"
                    | "--yes"
                    | "exec"
                    | "command"
                    | "env"
            )
        });
        if !launcher_ok {
            continue;
        }
        let (mut session, mut namespace) = (
            env_session
                .clone()
                .or_else(|| std::env::var("AGENT_BROWSER_SESSION").ok()),
            env_ns
                .clone()
                .or_else(|| std::env::var("AGENT_BROWSER_NAMESPACE").ok()),
        );
        let mut rest: Vec<&str> = Vec::new();
        let mut j = at + 1;
        while j < words.len() {
            let w = words[j].as_str();
            let (flag, inline) = match w.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
                _ => (w, None),
            };
            if rest.is_empty() && flag.starts_with('-') {
                let takes = VALUE_FLAGS.contains(&flag);
                let value = match (&inline, takes) {
                    (Some(v), _) => Some(v.clone()),
                    (None, true) => {
                        j += 1;
                        words.get(j).cloned()
                    }
                    (None, false) => None,
                };
                match flag {
                    "--session" => session = value,
                    "--namespace" => namespace = value,
                    _ => {}
                }
                j += 1;
                continue;
            }
            // Flags after the subcommand belong to it; a global --session
            // there still counts.
            if flag == "--session" || flag == "--namespace" {
                let value = inline.or_else(|| {
                    j += 1;
                    words.get(j).cloned()
                });
                if flag == "--session" {
                    session = value;
                } else {
                    namespace = value;
                }
                j += 1;
                continue;
            }
            rest.push(w);
            j += 1;
        }
        out.push(Invocation {
            session: SessionKey {
                name: session
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_SESSION.into()),
                namespace: namespace
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_lowercase()),
            },
            action: rest.join(" "),
        });
    }
    out
}

/// Split on `&&`, `||`, `;`, `|` and newlines outside quotes.
fn split_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), _) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), _) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, ';' | '\n') => out.push(std::mem::take(&mut cur)),
            (None, '&' | '|') => {
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.retain(|s| !s.trim().is_empty());
    out
}

/// Whitespace-separated words with simple quote and backslash handling.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some('"'), '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (Some(_), _) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                started = false;
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Whether `agent-browser` is on `PATH`.
pub fn installed() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        let p = d.join(if cfg!(windows) {
            "agent-browser.exe"
        } else {
            "agent-browser"
        });
        p.is_file()
    })
}

/// A decoded screencast frame.
pub struct Frame {
    pub image: Arc<gpui_kit::RenderImage>,
    /// Image pixels.
    pub width: u32,
    pub height: u32,
    /// The page viewport in CSS pixels (input coordinates).
    pub device_width: f32,
    pub device_height: f32,
}

/// One open tab.
#[derive(Clone, Debug, PartialEq)]
pub struct Tab {
    pub title: String,
    pub url: String,
    pub active: bool,
}

pub enum StreamMsg {
    Frame(Frame),
    Url(String),
    Tabs(Vec<Tab>),
    Status { connected: bool },
    Closed(Option<String>),
}

/// Decode one server message; frames are decoded to BGRA here, off the UI
/// thread.
fn parse_message(text: &str) -> Option<StreamMsg> {
    let v: Value = serde_json::from_str(text).ok()?;
    match v.get("type")?.as_str()? {
        "frame" => {
            let jpeg = base64::engine::general_purpose::STANDARD
                .decode(v.get("data")?.as_str()?)
                .ok()?;
            let mut rgba = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
                .ok()?
                .into_rgba8();
            // gpui wants BGRA.
            for px in rgba.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
            let (width, height) = rgba.dimensions();
            let meta = v.get("metadata");
            let num = |k: &str, d: u32| {
                meta.and_then(|m| m.get(k))
                    .and_then(Value::as_f64)
                    .map_or(d as f32, |x| x as f32)
            };
            Some(StreamMsg::Frame(Frame {
                image: Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(rgba)])),
                width,
                height,
                device_width: num("deviceWidth", width),
                device_height: num("deviceHeight", height),
            }))
        }
        "url" => Some(StreamMsg::Url(v.get("url")?.as_str()?.to_string())),
        "tabs" => Some(StreamMsg::Tabs(
            v.get("tabs")?
                .as_array()?
                .iter()
                .map(|t| Tab {
                    title: t
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    url: t
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    active: t.get("active").and_then(Value::as_bool).unwrap_or(false),
                })
                .collect(),
        )),
        "status" => Some(StreamMsg::Status {
            connected: v.get("connected").and_then(Value::as_bool).unwrap_or(false),
        }),
        _ => None,
    }
}

/// Watch a session's stream. Returns a sender for input messages (JSON
/// text) and the stream of decoded messages. Frames are dropped, not
/// queued, when the UI falls behind.
pub async fn connect(port: u16, max_fps: u32) -> Result<(Sender<String>, Receiver<StreamMsg>)> {
    use tokio_tungstenite::tungstenite::Message as Ws;
    let url = format!("ws://127.0.0.1:{port}/?maxFps={max_fps}");
    let (socket, _) = tokio_tungstenite::connect_async_with_config(
        url.as_str(),
        Some(
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(32 * 1024 * 1024))
                .max_frame_size(Some(32 * 1024 * 1024)),
        ),
        false,
    )
    .await
    .with_context(|| format!("connect to {url}"))?;
    let (mut sink, mut stream) = socket.split();
    let (in_tx, mut in_rx) = channel::<String>(64);
    let (mut out_tx, out_rx) = channel::<StreamMsg>(4);
    tokio::spawn(async move {
        while let Some(text) = in_rx.next().await {
            if sink.send(Ws::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    tokio::spawn(async move {
        let reason = loop {
            match stream.next().await {
                Some(Ok(Ws::Text(t))) => {
                    let text = t.to_string();
                    let Some(msg) = tokio::task::spawn_blocking(move || parse_message(&text))
                        .await
                        .ok()
                        .flatten()
                    else {
                        continue;
                    };
                    let sent = match msg {
                        // A newer frame follows; drop this one if the UI
                        // hasn't taken the previous ones yet.
                        StreamMsg::Frame(_) => match out_tx.try_send(msg) {
                            Ok(()) => Ok(()),
                            Err(e) if e.is_full() => Ok(()),
                            Err(_) => Err(()),
                        },
                        msg => out_tx.send(msg).await.map_err(|_| ()),
                    };
                    if sent.is_err() {
                        return;
                    }
                }
                Some(Ok(Ws::Close(c))) => break c.map(|c| c.reason.to_string()),
                Some(Ok(_)) => {}
                Some(Err(e)) => break Some(e.to_string()),
                None => break None,
            }
        };
        let _ = out_tx.send(StreamMsg::Closed(reason)).await;
    });
    Ok((in_tx, out_rx))
}

/// A click at page CSS pixel `(x, y)`.
pub fn click(x: f32, y: f32) -> [String; 2] {
    ["mousePressed", "mouseReleased"].map(|ev| {
        serde_json::json!({
            "type": "input_mouse", "eventType": ev, "x": x, "y": y,
            "button": "left", "clickCount": 1
        })
        .to_string()
    })
}

pub fn wheel(x: f32, y: f32, dx: f32, dy: f32) -> String {
    serde_json::json!({
        "type": "input_mouse", "eventType": "mouseWheel", "x": x, "y": y,
        "deltaX": dx, "deltaY": dy
    })
    .to_string()
}

/// Typed text (`char` events insert it at the focused element).
pub fn text(s: &str) -> String {
    serde_json::json!({"type": "input_keyboard", "eventType": "char", "text": s}).to_string()
}

/// A named key press (Enter, Backspace, Tab, arrows, …) as down + up.
pub fn key(key: &str, code: &str, key_code: u32) -> [String; 2] {
    ["keyDown", "keyUp"].map(|ev| {
        serde_json::json!({
            "type": "input_keyboard", "eventType": ev, "key": key, "code": code,
            "windowsVirtualKeyCode": key_code
        })
        .to_string()
    })
}

/// A project skill that teaches opencode agents to use agent-browser. The
/// CLI serves its own, version-matched usage guide (`skills get core`), so
/// this stays a short pointer.
pub const SKILL: &str = r#"---
name: agent-browser
description: Browser automation with the agent-browser CLI. Use it whenever a task needs a real browser - open or test a web app, click, fill forms, log in, read a rendered page, take screenshots, check a UI change. Prefer it over fetching HTML when the page needs JavaScript or interaction.
---

# agent-browser

Drive a real Chrome through the `agent-browser` CLI from the shell.

Before the first command in a task, load the usage guide that matches the
installed version:

```bash
agent-browser skills get core
```

The loop: `agent-browser open <url>`, then `agent-browser snapshot -i` to get
the interactive elements as refs (`@e1`, `@e2`, …), then act on refs
(`click @e2`, `fill @e3 "text"`, `press Enter`), and snapshot again after the
page changes. `agent-browser screenshot <path>` saves an image when you need
to look at layout.

- Keep one browser session per task. Pass `--session <name>` on every
  command when several tasks may run at once; otherwise use the default
  session.
- Don't pass launch options (`--headed`, `--allow-file-access`, `--profile`,
  …) on some commands and not others: a command with different launch
  options restarts the browser and loses the page.
- `agent-browser close` when done, unless the user wants to keep looking.
- The user watches this browser live in the desktop app's Browser panel and
  may click in it; re-snapshot before acting if they might have.
"#;

/// `<folder>/.opencode/skill/agent-browser/SKILL.md`
pub fn skill_path(folder: &Path) -> PathBuf {
    folder
        .join(".opencode")
        .join("skill")
        .join("agent-browser")
        .join("SKILL.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inv(cmd: &str) -> Vec<(String, Option<String>, String)> {
        invocations(cmd)
            .into_iter()
            .map(|i| (i.session.name, i.session.namespace, i.action))
            .collect()
    }

    #[test]
    fn plain_and_default_session() {
        assert_eq!(
            inv("agent-browser open https://example.com"),
            [("default".into(), None, "open https://example.com".into())]
        );
        assert!(inv("echo agent-browser open x").is_empty());
        assert!(inv("ls -la").is_empty());
    }

    #[test]
    fn sessions_namespaces_and_env() {
        assert_eq!(
            inv("agent-browser --session qa --json snapshot -i"),
            [("qa".into(), None, "snapshot -i".into())]
        );
        assert_eq!(
            inv("agent-browser --session=qa --namespace NsA click @e2"),
            [("qa".into(), Some("nsa".into()), "click @e2".into())]
        );
        assert_eq!(
            inv("AGENT_BROWSER_SESSION=login npx -y agent-browser fill @e3 'a b'"),
            [("login".into(), None, "fill @e3 a b".into())]
        );
        assert_eq!(
            inv("agent-browser open x.com --session late"),
            [("late".into(), None, "open x.com".into())]
        );
        assert_eq!(
            inv("/usr/local/bin/agent-browser --headed --executable-path /x/chrome open a.dev"),
            [("default".into(), None, "open a.dev".into())]
        );
    }

    #[test]
    fn chains() {
        let got = inv(
            "agent-browser --session s open a.dev && agent-browser --session s snapshot -i; echo done | agent-browser get url",
        );
        assert_eq!(
            got,
            [
                ("s".into(), None, "open a.dev".into()),
                ("s".into(), None, "snapshot -i".into()),
                ("default".into(), None, "get url".into()),
            ]
        );
        // Operators inside quotes don't split.
        assert_eq!(
            inv("agent-browser eval 'a && b; c'"),
            [("default".into(), None, "eval a && b; c".into())]
        );
    }

    #[test]
    fn finds_live_sessions_and_skips_dead_ones() {
        let base = std::env::temp_dir().join(format!("ocdesk-ab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ns = base.join("namespaces/nsa/run");
        std::fs::create_dir_all(&ns).unwrap();
        std::fs::write(base.join("default.stream"), "37561\n").unwrap();
        std::fs::write(base.join("default.pid"), "100").unwrap();
        std::fs::write(base.join("dead.stream"), "4000").unwrap();
        std::fs::write(base.join("dead.pid"), "200").unwrap();
        std::fs::write(base.join("zero.stream"), "0").unwrap();
        std::fs::write(base.join("default.sock"), "").unwrap();
        std::fs::write(ns.join("x3.stream"), "5000").unwrap();
        let got = sessions_in(&base, |pid| pid == 100);
        assert_eq!(
            got,
            [
                LiveSession {
                    name: "default".into(),
                    namespace: None,
                    port: 37561
                },
                LiveSession {
                    name: "x3".into(),
                    namespace: Some("nsa".into()),
                    port: 5000
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parses_stream_messages() {
        // A 2x1 JPEG.
        let mut jpeg = Vec::new();
        image::RgbImage::from_pixel(2, 1, image::Rgb([200, 10, 10]))
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        let msg = format!(
            r#"{{"type":"frame","seq":1,"data":"{data}","metadata":{{"deviceWidth":1280,"deviceHeight":720}}}}"#
        );
        match parse_message(&msg) {
            Some(StreamMsg::Frame(f)) => {
                assert_eq!((f.width, f.height), (2, 1));
                assert_eq!((f.device_width, f.device_height), (1280., 720.));
                // BGRA: blue channel first is low, red last is high.
                let px = f.image.as_bytes(0).unwrap();
                assert!(px[2] > 150 && px[0] < 60, "{:?}", &px[..4]);
            }
            _ => panic!("not a frame"),
        }
        assert!(matches!(
            parse_message(r#"{"type":"url","url":"https://a.dev/"}"#),
            Some(StreamMsg::Url(u)) if u == "https://a.dev/"
        ));
        match parse_message(
            r#"{"type":"tabs","tabs":[{"active":true,"title":"A","url":"https://a.dev/"}]}"#,
        ) {
            Some(StreamMsg::Tabs(t)) => assert!(t[0].active && t[0].title == "A"),
            _ => panic!("not tabs"),
        }
        assert!(parse_message(r#"{"type":"console"}"#).is_none());
        assert!(parse_message("not json").is_none());
    }

    #[test]
    fn input_messages() {
        let [down, up] = click(10., 20.);
        assert!(down.contains("\"mousePressed\"") && up.contains("\"mouseReleased\""));
        assert!(wheel(1., 2., 0., 120.).contains("\"deltaY\":120"));
        assert!(text("hi").contains("\"char\""));
        assert!(key("Enter", "Enter", 13)[0].contains("\"keyDown\""));
    }
}
