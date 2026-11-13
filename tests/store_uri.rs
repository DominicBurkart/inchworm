use inchworm::store::{DaemonLock, Store, StoreError};
use std::path::Path;

#[test]
fn in_memory_and_file_uri_names_are_never_locked_and_leave_no_files() {
    let scratch = std::env::var_os("TMPDIR").unwrap_or_else(|| std::env::temp_dir().into());
    let dir = tempfile::tempdir_in(scratch).unwrap();
    std::env::set_current_dir(dir.path()).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let other = DaemonLock::acquire(&elsewhere.path().join("other.db")).unwrap();

    for name in [
        ":memory:",
        "",
        "file::memory:",
        "file:memdb1?mode=memory&cache=shared",
        "file:%3Amemory%3A",
        "file:%3amemory%3a",
        "file:x?mode=%6Demory",
        "file:x?%6Dode=memory",
    ] {
        assert!(
            matches!(
                DaemonLock::acquire(Path::new(name)),
                Err(StoreError::RecoveryNeedsFileStore)
            ),
            "{name:?}"
        );
        let store = Store::open(Path::new(name)).unwrap();
        assert!(
            matches!(
                store.recover_after_crash(&other, 1),
                Err(StoreError::RecoveryNeedsFileStore)
            ),
            "{name:?}"
        );
    }

    for name in [
        "file:state.db",
        "file:/abs/state.db?cache=shared",
        "file:%2Fstate.db",
    ] {
        match DaemonLock::acquire(Path::new(name)) {
            Err(StoreError::LockIo { path, source }) => {
                assert_eq!(path, Path::new(name));
                assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
            }
            other => panic!("{name:?}: expected LockIo, got {other:?}"),
        }
    }

    let stray: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(stray.is_empty(), "stray files: {stray:?}");
}
