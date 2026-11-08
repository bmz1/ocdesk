//! Session terminals: opencode's persistent PTYs (`/api/experimental/…`).
//!
//! Protocol, as opencode's own desktop client speaks it
//! (`packages/client/src/solid/pty.ts`):
//! - create with `POST /api/experimental/session/{id}/terminal`;
//! - `GET …/persistent-pty/{id}/snapshot` gives the screen as a base64
//!   VT `checkpoint` plus the output cursor to resume from;
//! - `POST …/connect-token` (header `x-opencode-ticket: 1`) gives a one-use
//!   ticket for the WebSocket at `…/connect?ticket&cursor&attachment_id&takeover&input_protocol=1`;
//! - binary frames from the server are terminal output, text frames are
//!   JSON control messages (`attached`, `resized`, `exited`, …);
//! - frames to the server are `[has_data u8][cols u16 BE][rows u16 BE][bytes…]`.

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use futures::channel::mpsc::{Receiver, Sender, channel};
use futures::{SinkExt as _, StreamExt as _};
use reqwest::Method;
use serde_json::{Value, json};

use super::{Api, id};

#[derive(Clone, Debug, PartialEq)]
pub struct PtyInfo {
    pub id: String,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    /// Output cursor to resume streaming from.
    pub tail: u64,
    pub exit_code: Option<i64>,
}

impl PtyInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: v.get("id")?.as_str()?.to_string(),
            title: v
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Terminal")
                .to_string(),
            cols: v
                .pointer("/size/cols")
                .and_then(Value::as_u64)
                .unwrap_or(80) as u16,
            rows: v
                .pointer("/size/rows")
                .and_then(Value::as_u64)
                .unwrap_or(24) as u16,
            tail: v
                .pointer("/output/tail")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            exit_code: v.get("exitCode").and_then(Value::as_i64),
        })
    }
}

/// What the socket delivers.
#[derive(Debug)]
pub enum PtyMsg {
    Output(Vec<u8>),
    /// A JSON control message.
    Control(Value),
    Closed(Option<String>),
}

/// A frame for the server: optional input, always with the viewport size.
pub fn frame(cols: u16, rows: u16, data: Option<&[u8]>) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.map_or(0, <[u8]>::len));
    f.push(u8::from(data.is_some()));
    f.extend_from_slice(&cols.to_be_bytes());
    f.extend_from_slice(&rows.to_be_bytes());
    if let Some(d) = data {
        f.extend_from_slice(d);
    }
    f
}

pub fn decode_checkpoint(b64: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .unwrap_or_default()
}

impl Api {
    /// The session's terminals.
    pub async fn session_terminals(&self, session_id: &str) -> Result<Vec<PtyInfo>> {
        let path = format!("/api/experimental/session/{}/terminal", id(session_id));
        let v = self.send(self.req(Method::GET, &path)).await?;
        Ok(v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(PtyInfo::from_json).collect())
            .unwrap_or_default())
    }

    /// Start the user's shell in the session's directory.
    pub async fn create_terminal(&self, session_id: &str, cols: u16, rows: u16) -> Result<PtyInfo> {
        let path = format!("/api/experimental/session/{}/terminal", id(session_id));
        let body = json!({
            "args": [],
            "title": "Terminal",
            "env": {},
            "size": { "cols": cols, "rows": rows },
        });
        let v = self.send(self.req(Method::POST, &path).json(&body)).await?;
        PtyInfo::from_json(&v["data"]).ok_or_else(|| anyhow!("unexpected terminal response: {v}"))
    }

    /// The current screen (VT bytes to replay) and where output resumes.
    pub async fn pty_snapshot(&self, pty_id: &str) -> Result<(PtyInfo, Vec<u8>)> {
        let path = format!("/api/experimental/persistent-pty/{}/snapshot", id(pty_id));
        let v = self.send(self.req(Method::GET, &path)).await?;
        let d = &v["data"];
        let info = PtyInfo::from_json(&d["info"]).ok_or_else(|| anyhow!("bad snapshot: {v}"))?;
        let checkpoint = decode_checkpoint(d["checkpoint"].as_str().unwrap_or_default());
        Ok((info, checkpoint))
    }

    pub async fn remove_pty(&self, pty_id: &str) -> Result<()> {
        let path = format!("/api/experimental/persistent-pty/{}", id(pty_id));
        self.send(self.req(Method::DELETE, &path)).await?;
        Ok(())
    }

    /// Open the PTY's socket. Returns the sender for frames (see [`frame`])
    /// and the receiver of output and control messages.
    pub async fn connect_pty(
        &self,
        pty_id: &str,
        cursor: u64,
        attachment_id: &str,
    ) -> Result<(Sender<Vec<u8>>, Receiver<PtyMsg>)> {
        use tokio_tungstenite::tungstenite::{
            Message as Ws, client::IntoClientRequest as _, http::HeaderValue,
        };
        let token_path = format!(
            "/api/experimental/persistent-pty/{}/connect-token",
            id(pty_id)
        );
        let v = self
            .send(
                self.req(Method::POST, &token_path)
                    .header("x-opencode-ticket", "1")
                    .json(&json!({})),
            )
            .await?;
        let ticket = v
            .pointer("/data/ticket")
            .and_then(Value::as_str)
            .context("no connect ticket")?
            .to_string();
        let mut url = reqwest::Url::parse(&format!(
            "{}/api/experimental/persistent-pty/{}/connect",
            self.base,
            id(pty_id)
        ))?;
        url.query_pairs_mut()
            .append_pair("ticket", &ticket)
            .append_pair("cursor", &cursor.to_string())
            .append_pair("attachment_id", attachment_id)
            .append_pair("takeover", "true")
            .append_pair("input_protocol", "1");
        let ws_url = url.as_str().replacen("http", "ws", 1);
        let mut request = ws_url.into_client_request()?;
        let auth = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            crate::sidecar::USERNAME,
            self.password
        ));
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Basic {auth}"))?,
        );
        let (socket, _) = tokio_tungstenite::connect_async_with_config(
            request,
            Some(tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(1024 * 1024))
                .max_frame_size(Some(1024 * 1024))),
            false,
        ).await?;
        let (mut sink, mut stream) = socket.split();
        let (in_tx, mut in_rx) = channel::<Vec<u8>>(32);
        let (mut out_tx, out_rx) = channel::<PtyMsg>(32);
        tokio::spawn(async move {
            while let Some(bytes) = in_rx.next().await {
                if sink.send(Ws::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        tokio::spawn(async move {
            let reason = loop {
                match stream.next().await {
                    Some(Ok(Ws::Binary(b))) => {
                        if out_tx.send(PtyMsg::Output(b.to_vec())).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Ws::Text(t))) => {
                        let v = serde_json::from_str(t.as_str())
                            .unwrap_or(Value::String(t.to_string()));
                        if out_tx.send(PtyMsg::Control(v)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Ws::Close(c))) => break c.map(|c| c.reason.to_string()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Some(e.to_string()),
                    None => break None,
                }
            };
            let _ = out_tx.send(PtyMsg::Closed(reason)).await;
        });
        Ok((in_tx, out_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_size_and_optional_input() {
        assert_eq!(frame(80, 24, None), vec![0, 0, 80, 0, 24]);
        assert_eq!(frame(300, 2, Some(b"ls")), vec![1, 1, 44, 0, 2, b'l', b's']);
    }

    /// The client's half of the protocol against a local server: ticket
    /// request, WebSocket query and auth, binary output, JSON control and
    /// the framed input it sends.
    #[tokio::test]
    // The handshake callback's error type is tungstenite's, not ours.
    #[allow(clippy::result_large_err)]
    async fn connects_streams_and_frames_input() {
        use futures::{SinkExt as _, StreamExt as _};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio_tungstenite::tungstenite::Message as Ws;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            // 1. connect-token (plain HTTP).
            let (mut http, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = http.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.starts_with("POST /api/experimental/persistent-pty/pty_1/connect-token"));
            assert!(req.to_lowercase().contains("x-opencode-ticket: 1"));
            let body = r#"{"data":{"ticket":"t123","expires_in":30}}"#;
            http.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            drop(http);
            // 2. the socket.
            let (tcp, _) = listener.accept().await.unwrap();
            let mut uri = String::new();
            let mut auth = String::new();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                |r: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                    uri = r.uri().to_string();
                    auth = r
                        .headers()
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default();
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            ws.send(Ws::Binary(b"hi".to_vec().into())).await.unwrap();
            ws.send(Ws::Text(r#"{"type":"exited","exitCode":0}"#.into()))
                .await
                .unwrap();
            let Some(Ok(Ws::Binary(frame_in))) = ws.next().await else {
                panic!("expected an input frame")
            };
            (uri, auth, frame_in.to_vec())
        });
        let api = Api {
            http: reqwest::Client::new(),
            base: format!("http://127.0.0.1:{port}"),
            password: "pw".into(),
        };
        let (mut tx, mut rx) = api.connect_pty("pty_1", 42, "att").await.unwrap();
        assert!(matches!(rx.next().await, Some(PtyMsg::Output(b)) if b == b"hi"));
        assert!(matches!(rx.next().await, Some(PtyMsg::Control(v)) if v["type"] == "exited"));
        tx.send(frame(80, 24, Some(b"ls\r"))).await.unwrap();
        let (uri, auth, frame_in) = server.await.unwrap();
        assert!(uri.contains("ticket=t123"), "{uri}");
        assert!(
            uri.contains("cursor=42") && uri.contains("input_protocol=1"),
            "{uri}"
        );
        assert!(auth.starts_with("Basic "));
        assert_eq!(frame_in, frame(80, 24, Some(b"ls\r")));
    }

    #[test]
    fn info_parses_size_and_cursor() {
        let i = PtyInfo::from_json(
            &json!({"id":"pty_1","title":"zsh","size":{"cols":120,"rows":30},
            "output":{"head":0,"tail":42}}),
        )
        .unwrap();
        assert_eq!((i.cols, i.rows, i.tail), (120, 30, 42));
    }
}

