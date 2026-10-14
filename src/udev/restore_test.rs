use super::{restore_saved_permissions, tracked_device_name, SAVED_PERMISSIONS};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TestNode(PathBuf);
impl TestNode {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "inputplumber-restore-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        Self(directory.join("event123"))
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
    fn create_hidden(&self) {
        fs::write(&self.0, []).unwrap();
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0)).unwrap();
    }
    fn record(&self, mode: u32) {
        SAVED_PERMISSIONS
            .lock()
            .unwrap()
            .insert(self.path().into(), mode);
    }
    fn mode(&self) -> u32 {
        fs::metadata(&self.0).unwrap().permissions().mode() & 0o7777
    }
}
impl Drop for TestNode {
    fn drop(&mut self) {
        SAVED_PERMISSIONS.lock().unwrap().remove(self.path());
        let _ = fs::remove_dir_all(self.0.parent().unwrap());
    }
}

#[test]
fn restores_exact_recorded_mode_and_forgets_success() {
    let node = TestNode::new();
    node.create_hidden();
    node.record(0o640);
    restore_saved_permissions(node.path());
    assert_eq!(node.mode(), 0o640);
    assert!(!SAVED_PERMISSIONS.lock().unwrap().contains_key(node.path()));
}

#[test]
fn leaves_unmanaged_hidden_node_unchanged() {
    let node = TestNode::new();
    node.create_hidden();
    restore_saved_permissions(node.path());
    assert_eq!(node.mode(), 0);
    assert_eq!(tracked_device_name(node.path()), None);
}

#[test]
fn failed_restore_keeps_record_for_retry() {
    let node = TestNode::new();
    node.record(0o600);
    restore_saved_permissions(node.path());
    assert_eq!(
        SAVED_PERMISSIONS.lock().unwrap().get(node.path()),
        Some(&0o600)
    );
    node.create_hidden();
    restore_saved_permissions(node.path());
    assert_eq!(node.mode(), 0o600);
}

#[test]
fn fallback_name_requires_ownership_and_valid_node_name() {
    let node = TestNode::new();
    node.record(0o660);
    assert_eq!(tracked_device_name(node.path()), Some("event123".into()));
    assert_eq!(tracked_device_name("/dev/input/event-not-a-node"), None);
    assert_eq!(tracked_device_name("/dev/hidraw"), None);
}
