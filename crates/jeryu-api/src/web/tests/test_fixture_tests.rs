//! The test fixtures themselves: a test state must leave nothing behind in the
//! system temp directory. Leaked sqlite files once filled a forge host's root
//! disk, which refused production uploads.

use super::*;

/// Files a test state would leak if it did not own its databases. Counted
/// rather than asserted absent, so stale files from an older build of the
/// suite cannot fail an otherwise clean run.
fn web_sqlite_files_in_temp_dir() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("jeryu-web-") && name.contains(".sqlite")
        })
        .count()
}

#[test]
fn a_test_web_state_owns_its_databases_and_removes_them_when_it_drops() {
    let before = web_sqlite_files_in_temp_dir();
    let state = WebState::new(ForgeCore::new());
    let data_dir = state.test_data_dir.path().to_path_buf();
    for database in ["codegraph.sqlite", "work.sqlite", "shift.sqlite"] {
        assert!(
            data_dir.join(database).is_file(),
            "{database} must live in the directory the state owns, not the system temp directory"
        );
    }

    drop(state);

    assert!(
        !data_dir.exists(),
        "dropping the state must remove the directory holding its databases"
    );
    assert_eq!(
        web_sqlite_files_in_temp_dir(),
        before,
        "a test state must not write jeryu-web-*.sqlite files into the system temp directory"
    );
}
