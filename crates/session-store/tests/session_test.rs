//! Integration tests for session-store.

use session_store::{SessionMode, SessionStore, UserSession, now_secs};
use tempfile::TempDir;

fn setup() -> (TempDir, SessionStore) {
    let tmp = TempDir::new().unwrap();
    let store = SessionStore::new(tmp.path());
    (tmp, store)
}

#[test]
fn load_fresh_session() {
    let (_tmp, store) = setup();
    let s = store.load("user1");
    assert_eq!(s.user_id, "user1");
    assert_eq!(s.mode, SessionMode::Coding);
    assert!(s.pending_steps.is_empty());
    assert!(s.current_task.is_none());
}

#[test]
fn save_and_reload() {
    let (_tmp, store) = setup();
    let session = UserSession {
        user_id: "user1".into(),
        mode: SessionMode::Planning,
        current_task: Some("write tests".into()),
        pending_steps: vec!["step a".into(), "step b".into()],
        last_result: Some("ok".into()),
        last_action: Some("test".into()),
        updated_at: now_secs(),
    };
    store.save(&session).unwrap();
    let loaded = store.load("user1");
    assert_eq!(loaded.mode, SessionMode::Planning);
    assert_eq!(loaded.current_task.as_deref(), Some("write tests"));
    assert_eq!(loaded.pending_steps, vec!["step a", "step b"]);
    assert_eq!(loaded.last_action.as_deref(), Some("test"));
}

#[test]
fn clear_deletes_session() {
    let (_tmp, store) = setup();
    let session = UserSession {
        user_id: "user2".into(),
        current_task: Some("some task".into()),
        ..Default::default()
    };
    store.save(&session).unwrap();
    store.clear("user2");
    let loaded = store.load("user2");
    assert!(loaded.current_task.is_none());
}

#[test]
fn double_clear_is_noop() {
    let (_tmp, store) = setup();
    store.clear("nobody");
    store.clear("nobody"); // should not panic
}

#[test]
fn path_sanitization_stays_in_base_dir() {
    let (tmp, store) = setup();
    let session = UserSession {
        user_id: "../../evil".into(),
        ..Default::default()
    };
    store.save(&session).unwrap();
    let canonical_base = tmp.path().canonicalize().unwrap();
    for entry in std::fs::read_dir(tmp.path()).unwrap() {
        let path = entry.unwrap().path().canonicalize().unwrap();
        assert!(
            path.starts_with(&canonical_base),
            "session file escaped base dir: {path:?}"
        );
    }
}

#[test]
fn pending_steps_ordering() {
    let (_tmp, store) = setup();
    let mut session = UserSession {
        user_id: "u3".into(),
        pending_steps: vec!["first".into(), "second".into(), "third".into()],
        ..Default::default()
    };
    store.save(&session).unwrap();
    let mut loaded = store.load("u3");
    let first = loaded.pending_steps.remove(0);
    assert_eq!(first, "first");
    assert_eq!(loaded.pending_steps.len(), 2);
    store.save(&loaded).unwrap();
    let reloaded = store.load("u3");
    assert_eq!(reloaded.pending_steps[0], "second");
}

#[test]
fn snapshot_save_list_load_roundtrip() {
    let (_tmp, store) = setup();
    let session = UserSession {
        user_id: "traveler".into(),
        mode: SessionMode::Planning,
        current_task: Some("remote-work".into()),
        pending_steps: vec!["step1".into(), "step2".into()],
        last_result: Some("ok".into()),
        last_action: Some("plan".into()),
        updated_at: now_secs(),
    };

    let id = store.save_snapshot(&session, Some("trip")).unwrap();
    assert!(id.contains("trip"));

    let list = store.list_snapshots("traveler").unwrap();
    assert!(!list.is_empty());
    assert_eq!(list[0].current_task.as_deref(), Some("remote-work"));

    let loaded = store.load_snapshot("traveler", &id).unwrap();
    assert_eq!(loaded.user_id, "traveler");
    assert_eq!(loaded.pending_steps.len(), 2);
}
