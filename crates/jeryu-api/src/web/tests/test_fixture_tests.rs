//! The test fixtures themselves: a test state must leave nothing behind in the
//! system temp directory. Leaked sqlite files once filled a forge host's root
//! disk, which refused production uploads.

use super::*;

#[test]
fn a_test_web_state_owns_its_databases_and_removes_them_when_it_drops() {
    let state = WebState::new(ForgeCore::new());
    let data_dir = state.test_data_dir.path().to_path_buf();
    let temp_dir = std::env::temp_dir();
    assert_ne!(
        data_dir, temp_dir,
        "the state must own a directory of its own, not the shared temp directory"
    );
    for database in ["codegraph.sqlite", "work.sqlite", "shift.sqlite"] {
        let path = data_dir.join(database);
        assert!(
            path.is_file(),
            "{database} must live in the directory the state owns"
        );
    }

    // The sqlite files a state opens, and the `-wal`/`-shm` siblings sqlite
    // writes next to them, all sit in this one directory. Asserting the
    // directory is gone therefore covers every file the state created; a
    // count over the shared temp directory could not, because other
    // processes add and remove files there while this test runs.
    drop(state);

    assert!(
        !data_dir.exists(),
        "dropping the state must remove the directory holding its databases"
    );
}
