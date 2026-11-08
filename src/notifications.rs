//! Unread notification routing and the macOS Dock badge.
/// Call on the GPUI foreground thread. The Dock badge clears with read state.
#[cfg(target_os = "macos")]
pub fn badge(count: usize) {
    use std::ffi::{CString, c_char, c_void};
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    // AppKit's NSApplication and NSDockTile are main-thread-only. GPUI owns
    // their lifetime; NSString's factory returns an autoreleased object.
    unsafe {
        let msg: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let string: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let set: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let app = msg(
            objc_getClass(c"NSApplication".as_ptr()),
            sel_registerName(c"sharedApplication".as_ptr()),
        );
        let tile = msg(app, sel_registerName(c"dockTile".as_ptr()));
        let value = CString::new(if count == 0 {
            String::new()
        } else {
            count.to_string()
        })
        .expect("numeric badge");
        let label = string(
            objc_getClass(c"NSString".as_ptr()),
            sel_registerName(c"stringWithUTF8String:".as_ptr()),
            value.as_ptr(),
        );
        set(tile, sel_registerName(c"setBadgeLabel:".as_ptr()), label);
    }
}
#[cfg(not(target_os = "macos"))]
pub fn badge(_: usize) {}

pub struct Attention {
    pub session: String,
    pub status: &'static str,
    pub key: String,
    pub title: String,
}
pub fn attention(event: &crate::api::Event, store: &crate::store::Store) -> Option<Attention> {
    use crate::api::{Event, events::Outcome};
    let (sid, status, key) = match event {
        Event::ExecutionEnded {
            session_id,
            outcome: Outcome::Succeeded,
        } => (session_id, "Thread finished", "finished".into()),
        Event::ExecutionEnded {
            session_id,
            outcome: Outcome::Failed { .. },
        } => (session_id, "Thread failed", "failed".into()),
        Event::PermissionAsked(r) => (
            &r.session_id,
            "Approval needed",
            format!("permission:{}", r.id),
        ),
        Event::FormAsked(r) => (&r.session_id, "Answer needed", format!("form:{}", r.id)),
        _ => return None,
    };
    let mut root = sid.clone();
    for _ in 0..32 {
        if let Some(parent) = store
            .children
            .iter()
            .find(|(_, kids)| kids.iter().any(|k| k.id == root))
            .map(|(id, _)| id.clone())
        {
            root = parent
        } else {
            break;
        }
    }
    if store.window_active && store.selected.as_ref() == Some(&root) {
        return None;
    }
    let title = store
        .session(&root)
        .map(|s| s.display_title())
        .unwrap_or_else(|| "ocdesk".into());
    Some(Attention {
        session: root,
        status,
        key: format!("{sid}:{key}"),
        title,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{Event, events::Outcome, model::SessionInfo},
        store::Store,
    };
    #[test]
    fn attention_respects_focus_and_routes_subagents_to_the_root_thread() {
        let mut store = Store::default();
        let child = SessionInfo::from_json(
            &serde_json::json!({"id":"child","parentID":"root","location":{"directory":"/tmp"}}),
        )
        .unwrap();
        store.children.insert("root".into(), vec![child]);
        store.selected = Some("root".into());
        store.window_active = true;
        let event = Event::ExecutionEnded {
            session_id: "child".into(),
            outcome: Outcome::Failed {
                error: "oops".into(),
            },
        };
        assert!(attention(&event, &store).is_none());
        store.window_active = false;
        let a = attention(&event, &store).unwrap();
        assert_eq!(a.session, "root");
        assert_eq!(a.status, "Thread failed");
        store.window_active = true;
        store.selected = Some("other".into());
        assert!(attention(&event, &store).is_some());
        assert!(
            attention(
                &Event::ExecutionEnded {
                    session_id: "child".into(),
                    outcome: Outcome::Interrupted {
                        reason: "user".into()
                    }
                },
                &store
            )
            .is_none()
        );
    }
}
