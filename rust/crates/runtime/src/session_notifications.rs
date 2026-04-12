use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

thread_local! {
    static ACTIVE_TOOL_SESSION_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn notification_registry() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn with_active_tool_session<R>(session_id: Option<&str>, f: impl FnOnce() -> R) -> R {
    if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
        ACTIVE_TOOL_SESSION_STACK.with(|stack| {
            stack.borrow_mut().push(session_id.to_string());
        });
        let _guard = SessionStackGuard;
        f()
    } else {
        f()
    }
}

struct SessionStackGuard;

impl Drop for SessionStackGuard {
    fn drop(&mut self) {
        ACTIVE_TOOL_SESSION_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
    }
}

#[must_use]
pub fn active_tool_session_id() -> Option<String> {
    ACTIVE_TOOL_SESSION_STACK.with(|stack| stack.borrow().last().cloned())
}

pub fn enqueue_session_notification(session_id: impl Into<String>, message: impl Into<String>) {
    let session_id = session_id.into();
    if session_id.trim().is_empty() {
        return;
    }
    let mut registry = notification_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.entry(session_id).or_default().push(message.into());
}

#[must_use]
pub fn drain_session_notifications(session_id: &str) -> Vec<String> {
    let mut registry = notification_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.remove(session_id).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        active_tool_session_id, drain_session_notifications, enqueue_session_notification,
        with_active_tool_session,
    };

    #[test]
    fn active_tool_session_id_tracks_nested_scope() {
        assert_eq!(active_tool_session_id(), None);

        with_active_tool_session(Some("outer"), || {
            assert_eq!(active_tool_session_id().as_deref(), Some("outer"));

            with_active_tool_session(Some("inner"), || {
                assert_eq!(active_tool_session_id().as_deref(), Some("inner"));
            });

            assert_eq!(active_tool_session_id().as_deref(), Some("outer"));
        });

        assert_eq!(active_tool_session_id(), None);
    }

    #[test]
    fn drains_notifications_per_session() {
        enqueue_session_notification("session-a", "first");
        enqueue_session_notification("session-a", "second");
        enqueue_session_notification("session-b", "third");

        assert_eq!(
            drain_session_notifications("session-a"),
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(
            drain_session_notifications("session-a"),
            Vec::<String>::new()
        );
        assert_eq!(
            drain_session_notifications("session-b"),
            vec!["third".to_string()]
        );
    }
}
