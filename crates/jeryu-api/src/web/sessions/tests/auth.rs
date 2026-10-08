use super::*;

#[test]
fn resolve_agent_program_maps_ids_command_and_env_override() {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    let none = |_: &str| None;

    // An explicit command always wins over the id mapping.
    assert_eq!(
        super::super::resolve_agent_program_with("codex", Some("/custom/bin"), none),
        PathBuf::from("/custom/bin")
    );

    // Each known agent_id resolves to its installed CLI path.
    assert_eq!(
        super::super::resolve_agent_program_with("codex", None, none),
        PathBuf::from("/home/ubuntu/.npm-global/bin/codex")
    );
    assert_eq!(
        super::super::resolve_agent_program_with("claude", None, none),
        PathBuf::from("/home/ubuntu/.local/bin/claude")
    );
    assert_eq!(
        super::super::resolve_agent_program_with("jekko", None, none),
        PathBuf::from("/home/ubuntu/.local/bin/jekko")
    );

    // The env override (id upper-cased, non-alnum -> '_') beats the baked-in path.
    let mut env = BTreeMap::new();
    env.insert(
        "JERYU_AGENT_CODEX_BIN".to_string(),
        "/echo/codex".to_string(),
    );
    assert_eq!(
        super::super::resolve_agent_program_with("codex", None, |key| env.get(key).cloned()),
        PathBuf::from("/echo/codex")
    );

    // An unknown id with no override falls back to the standard agent prefix.
    assert_eq!(
        super::super::resolve_agent_program_with("ghost", None, none),
        PathBuf::from("/opt/jeryu/bin/agent")
    );
}

#[test]
fn session_auth_uses_owned_fixture_home() {
    let state = WebState::new(ForgeCore::new());
    let workspace = tempfile::tempdir().expect("session workspace");
    let auth_home = state.session_auth_home.path();
    assert!(
        std::fs::read_dir(auth_home)
            .expect("fixture home")
            .next()
            .is_none()
    );
    std::fs::create_dir(auth_home.join(".codex")).expect("fixture codex directory");
    let marker = r#"{"source":"jeryu-unit-test-fixture"}"#;
    std::fs::write(auth_home.join(".codex/auth.json"), marker).expect("fixture auth marker");

    super::super::seed_agent_auth(workspace.path(), "codex", Some(auth_home));

    assert_eq!(
        std::fs::read_to_string(workspace.path().join(".agent-home/.codex/auth.json"))
            .expect("seeded fixture marker"),
        marker
    );
}

#[test]
fn seed_agent_auth_copies_claude_state_and_marks_onboarding_complete() {
    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    std::fs::create_dir_all(host.path().join(".claude")).expect("create claude dir");
    std::fs::write(
        host.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"test-token"}}"#,
    )
    .expect("write credentials");
    std::fs::write(host.path().join(".claude/settings.json"), "{}").expect("write settings");
    std::fs::write(
        host.path().join(".claude.json"),
        r#"{"hasCompletedOnboarding":false,"oauthAccount":{"email":"test@example.com"}}"#,
    )
    .expect("write claude state");

    super::super::seed_agent_auth_from_home(workspace.path(), "claude", host.path());

    let agent_home = workspace.path().join(".agent-home");
    assert!(
        agent_home.join(".claude/.credentials.json").is_file(),
        "Claude credentials must be copied into the agent home"
    );
    let state_path = agent_home.join(".claude.json");
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(&state_path).expect("read seeded claude state"),
    )
    .expect("parse seeded claude state");
    assert_eq!(state["hasCompletedOnboarding"], json!(true));
    assert_eq!(state["theme"], json!("dark"));
    assert_eq!(state["oauthAccount"]["email"], json!("test@example.com"));
    assert!(
        agent_home.join(".claude/.claude.json").is_file(),
        "nested Claude state is seeded for older Claude Code builds"
    );
}

#[test]
fn seed_agent_auth_seeds_auth_files_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    std::fs::create_dir_all(host.path().join(".claude")).expect("create claude dir");
    std::fs::write(
        host.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"refreshToken":"host-refresh-token"}}"#,
    )
    .expect("write host auth file");
    // World-readable on the host: the seeded copy must still be owner-only.
    std::fs::set_permissions(
        host.path().join(".claude/.credentials.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("loosen host auth file mode");
    std::fs::write(host.path().join(".claude.json"), "{}").expect("write claude state");

    super::super::seed_agent_auth_from_home(workspace.path(), "claude", host.path());

    let agent_home = workspace.path().join(".agent-home");
    let seeded =
        std::fs::metadata(agent_home.join(".claude/.credentials.json")).expect("seeded auth file");
    assert_eq!(
        seeded.permissions().mode() & 0o777,
        0o400,
        "a seeded host token file must be owner read-only"
    );
    let state = std::fs::metadata(agent_home.join(".claude.json")).expect("seeded claude state");
    assert_eq!(
        state.permissions().mode() & 0o777,
        0o600,
        "seeded Claude state must be owner-only"
    );
}

#[test]
fn seed_agent_auth_refuses_a_symlinked_agent_home() {
    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    let target = tempfile::tempdir().expect("symlink target");
    std::fs::create_dir_all(host.path().join(".claude")).expect("create claude dir");
    std::fs::write(
        host.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"refreshToken":"host-refresh-token"}}"#,
    )
    .expect("write host auth file");
    // A checkout that plants `.agent-home` as a symlink must not redirect the
    // host operator's tokens outside the workspace.
    std::os::unix::fs::symlink(target.path(), workspace.path().join(".agent-home"))
        .expect("plant agent-home symlink");

    super::super::seed_agent_auth_from_home(workspace.path(), "claude", host.path());

    assert!(
        std::fs::read_dir(target.path())
            .expect("read symlink target")
            .next()
            .is_none(),
        "seeding must refuse a symlinked .agent-home instead of writing through it"
    );
}

#[test]
fn seed_agent_auth_refuses_a_symlinked_directory_under_agent_home() {
    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    let target = tempfile::tempdir().expect("symlink target");
    std::fs::create_dir_all(host.path().join(".claude")).expect("create claude dir");
    std::fs::write(
        host.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"refreshToken":"host-refresh-token"}}"#,
    )
    .expect("write host auth file");
    let agent_home = workspace.path().join(".agent-home");
    std::fs::create_dir_all(&agent_home).expect("create agent home");
    std::os::unix::fs::symlink(target.path(), agent_home.join(".claude"))
        .expect("plant claude dir symlink");

    super::super::seed_agent_auth_from_home(workspace.path(), "claude", host.path());

    assert!(
        std::fs::read_dir(target.path())
            .expect("read symlink target")
            .next()
            .is_none(),
        "a symlinked directory under .agent-home must be refused too"
    );
}

#[test]
fn purge_seeded_agent_auth_removes_the_seeded_files() {
    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    std::fs::create_dir_all(host.path().join(".claude")).expect("create claude dir");
    std::fs::write(
        host.path().join(".claude/.credentials.json"),
        r#"{"claudeAiOauth":{"refreshToken":"host-refresh-token"}}"#,
    )
    .expect("write host auth file");

    super::super::seed_agent_auth_from_home(workspace.path(), "claude", host.path());
    let agent_home = workspace.path().join(".agent-home");
    assert!(agent_home.join(".claude/.credentials.json").is_file());

    super::super::purge_seeded_agent_auth(workspace.path());

    assert!(
        !agent_home.exists(),
        "the session end must remove the seeded host auth copies"
    );
}

#[test]
fn purge_seeded_agent_auth_leaves_a_symlinked_agent_home_alone() {
    let workspace = tempfile::tempdir().expect("session workspace");
    let target = tempfile::tempdir().expect("symlink target");
    std::fs::write(target.path().join("host-file"), "keep me").expect("write host file");
    std::os::unix::fs::symlink(target.path(), workspace.path().join(".agent-home"))
        .expect("plant agent-home symlink");

    super::super::purge_seeded_agent_auth(workspace.path());

    assert!(
        target.path().join("host-file").is_file(),
        "purging must never delete through a planted symlink"
    );
}

#[test]
fn seed_agent_auth_trusts_codex_container_and_native_workspace_paths() {
    let host = tempfile::tempdir().expect("host auth home");
    let workspace = tempfile::tempdir().expect("session workspace");
    std::fs::create_dir_all(host.path().join(".codex")).expect("create codex dir");
    std::fs::write(host.path().join(".codex/config.toml"), "model = \"test\"\n")
        .expect("write codex config");

    super::super::seed_agent_auth_from_home(workspace.path(), "codex", host.path());

    let config = std::fs::read_to_string(workspace.path().join(".agent-home/.codex/config.toml"))
        .expect("read seeded codex config");
    assert!(
        config.contains("[projects.\"/workspace\"]"),
        "docker-mounted workspace must be trusted: {config}"
    );
    assert!(
        config.contains(&format!(
            "[projects.\"{}\"]",
            workspace.path().to_string_lossy()
        )),
        "native session workspace must be trusted: {config}"
    );
}
