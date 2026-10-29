//! Close executable fixture writers before publishing their paths.

use std::io::Write;
use std::path::Path;

pub(crate) fn write(path: &Path, body: &str) -> std::io::Result<()> {
    // NamedTempFile uses CLOEXEC. Close the writer before the rename so
    // callers never see a destination with an open writer in this process.
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().expect("script parent"))?;
    staged.write_all(body.as_bytes())?;
    let staged = staged.into_temp_path();
    staged.persist(path).map_err(|error| error.error)
}

#[test]
fn published_scripts_can_be_executed_and_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("script");
    let mut previous_writer = None;
    for text in ["first", "second"] {
        write(&path, &format!("#!/bin/sh\nprintf '{text}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(&path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, text.as_bytes());
        // Replacing a fixture must also work while its previous inode has a
        // writer held elsewhere. An in-place write would fail the next exec.
        previous_writer = Some(std::fs::OpenOptions::new().write(true).open(&path).unwrap());
    }
    drop(previous_writer);
}
