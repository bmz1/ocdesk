//! A minimal Chrome DevTools Protocol client over `--remote-debugging-pipe`.
//!
//! Chrome reads commands from fd 3 and writes replies/events to fd 4, each
//! message a JSON object terminated by NUL. A pipe (not a debugging port)
//! means no other local process can drive this browser. Targets are used in
//! flattened mode: commands for a page carry its `sessionId`.
//!
//! The two pipe ends are serviced by plain threads; callers await replies
//! through oneshot channels and subscribe to events through a broadcast.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// A protocol event (`Page.loadEventFired`, …), with the page session it
/// belongs to (`None` for browser-level events).
#[derive(Clone, Debug)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

#[derive(Clone)]
pub struct Cdp {
    out: std::sync::mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next: Arc<AtomicU64>,
    events: broadcast::Sender<CdpEvent>,
    closed: Arc<AtomicBool>,
}

impl Cdp {
    /// Service `write_fd` (Chrome's fd 3) and `read_fd` (Chrome's fd 4).
    pub fn new(write_fd: OwnedFd, read_fd: OwnedFd) -> Self {
        let (out, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let pending: Pending = Arc::default();
        let (events, _) = broadcast::channel(1024);
        let closed = Arc::new(AtomicBool::new(false));

        std::thread::Builder::new()
            .name("cdp-write".into())
            .spawn(move || {
                let mut w = std::fs::File::from(write_fd);
                while let Ok(mut msg) = out_rx.recv() {
                    msg.push(0);
                    if w.write_all(&msg).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn cdp writer");

        let (p, ev, cl) = (pending.clone(), events.clone(), closed.clone());
        std::thread::Builder::new()
            .name("cdp-read".into())
            .spawn(move || {
                let mut r = BufReader::new(std::fs::File::from(read_fd));
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match r.read_until(0, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    if buf.last() == Some(&0) {
                        buf.pop();
                    }
                    let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                        continue;
                    };
                    dispatch(v, &p, &ev);
                }
                cl.store(true, Ordering::SeqCst);
                // Fail everything still waiting.
                for (_, tx) in p.lock().unwrap().drain() {
                    let _ = tx.send(Err(anyhow!("the browser closed")));
                }
                let _ = ev.send(CdpEvent {
                    method: "ocdesk.closed".into(),
                    params: Value::Null,
                    session: None,
                });
            })
            .expect("spawn cdp reader");

        Self {
            out,
            pending,
            next: Arc::new(AtomicU64::new(1)),
            events,
            closed,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.events.subscribe()
    }

    /// Send a command and wait for its result.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        if self.is_closed() {
            bail!("the browser closed");
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.out.send(serde_json::to_vec(&msg)?).is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("the browser closed");
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r.with_context(|| method.to_string()),
            Ok(Err(_)) => Err(anyhow!("the browser closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("{method} timed out"))
            }
        }
    }
}

fn dispatch(v: Value, pending: &Pending, events: &broadcast::Sender<CdpEvent>) {
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let Some(tx) = pending.lock().unwrap().remove(&id) else {
            return;
        };
        let r = match v.get("error") {
            Some(e) => Err(anyhow!(
                "{}",
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("CDP error")
            )),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = tx.send(r);
    } else if let Some(method) = v.get("method").and_then(Value::as_str) {
        let _ = events.send(CdpEvent {
            method: method.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
            session: v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
}

/// A close-on-exec pipe: `(read end, write end)`.
pub fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: plain syscalls on a local array; the fds are owned right after.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe");
        }
        for fd in fds {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake "browser": echoes each command's id with a result, and emits
    /// one event first.
    #[tokio::test]
    async fn calls_and_events_over_pipes() {
        let (to_browser_r, to_browser_w) = pipe().unwrap();
        let (from_browser_r, from_browser_w) = pipe().unwrap();
        let cdp = Cdp::new(to_browser_w, from_browser_r);
        let mut events = cdp.subscribe();
        let fake = std::thread::spawn(move || {
            let mut r = BufReader::new(std::fs::File::from(to_browser_r));
            let mut w = std::fs::File::from(from_browser_w);
            let mut buf = Vec::new();
            r.read_until(0, &mut buf).unwrap();
            buf.pop();
            let cmd: Value = serde_json::from_slice(&buf).unwrap();
            assert_eq!(cmd["method"], "Page.navigate");
            assert_eq!(cmd["sessionId"], "S1");
            let ev = json!({"method": "Page.loadEventFired", "params": {}, "sessionId": "S1"});
            w.write_all(&[serde_json::to_vec(&ev).unwrap(), vec![0]].concat())
                .unwrap();
            let reply = json!({"id": cmd["id"], "result": {"frameId": "F"}});
            w.write_all(&[serde_json::to_vec(&reply).unwrap(), vec![0]].concat())
                .unwrap();
            // Second command: an error reply, then close.
            buf.clear();
            r.read_until(0, &mut buf).unwrap();
            buf.pop();
            let cmd: Value = serde_json::from_slice(&buf).unwrap();
            let reply = json!({"id": cmd["id"], "error": {"code": -32000, "message": "No node"}});
            w.write_all(&[serde_json::to_vec(&reply).unwrap(), vec![0]].concat())
                .unwrap();
        });
        let r = cdp
            .call("Page.navigate", json!({"url": "about:blank"}), Some("S1"))
            .await
            .unwrap();
        assert_eq!(r["frameId"], "F");
        let ev = events.recv().await.unwrap();
        assert_eq!(
            (ev.method.as_str(), ev.session.as_deref()),
            ("Page.loadEventFired", Some("S1"))
        );
        let err = cdp
            .call("DOM.focus", json!({}), Some("S1"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("No node"));
        fake.join().unwrap();
        // The fake dropped its ends: the client notices.
        let closed = events.recv().await.unwrap();
        assert_eq!(closed.method, "ocdesk.closed");
        assert!(cdp.call("X.y", json!({}), None).await.is_err());
    }
}
