use super::*;

#[test]
fn lock_rejects_missing_and_noncanonical_commits() {
    let valid: Value = toml::from_str(
        r#"
                [[repo]]
                name = "jeryu"
                github_slug = "neverhuman/jeryu"
                local_path = "/tmp/jeryu"
                tag = "jeryu-v5.0.0-split.0"
                commit = "0123456789abcdef0123456789abcdef01234567"
                required_check = "jeryu/required"
            "#,
    )
    .unwrap();
    verify_lock_value(&valid).unwrap();

    let invalid: Value = toml::from_str(
        r#"
                [[repo]]
                name = "jeryu"
                github_slug = "neverhuman/jeryu"
                local_path = "/tmp/jeryu"
                tag = "jeryu-v5.0.0-split.0"
                commit = "ABC"
                required_check = ""
            "#,
    )
    .unwrap();
    let error = verify_lock_value(&invalid).unwrap_err().to_string();
    assert!(error.contains("jeryu missing required_check"));
    assert!(error.contains("jeryu commit is not a sha: ABC"));
}

#[test]
fn verify_lock_pins_jeryu_web_by_commit_and_dist_hash() {
    let web = |commit: &str, dist: &str| -> Value {
        toml::from_str(&format!(
            r#"
                web_artifact = "pinned"
                [[repo]]
                name = "jeryu-web"
                github_slug = "neverhuman/jeryu-web"
                local_path = "/tmp/jeryu-web"
                commit = "{commit}"
                web_dist_sha256 = "{dist}"
                required_check = "jeryu-web/required"
            "#
        ))
        .unwrap()
    };
    let commit = "cdbef2cbf2fb93ca41ff780fc56f91d0b16620c2";
    let dist = "3afaf92cd37e5a657f83ded3f436de58387c02d8f1137f4c28893857083066fd";
    verify_lock_value(&web(commit, dist)).unwrap();

    let error = verify_lock_value(&web("PENDING", "abc"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("jeryu-web commit must be a full 40-hex sha: PENDING"));
    assert!(error.contains("jeryu-web web_dist_sha256 is not a sha256: abc"));

    let mut other = web(commit, dist);
    other["web_artifact"] = Value::String("local-or-pinned".to_string());
    let error = verify_lock_value(&other).unwrap_err().to_string();
    assert!(error.contains("web_artifact must be \"pinned\""));
    assert!(error.contains("jeryu-web missing tag"));
}

#[test]
fn the_repository_lock_pins_jeryu_web() {
    let lock = read_toml(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../jeryu-split.lock.toml"
    )))
    .unwrap();
    verify_lock_value(&lock).unwrap();
    assert_eq!(lock["web_artifact"].as_str(), Some("pinned"));
}

/// A one-member lock for an invented family, pinned to `commit`.
fn member_lock(commit: &str) -> Value {
    toml::from_str(&format!(
        r#"
                [[repo]]
                name = "pelago"
                github_slug = "pelago/pelago"
                local_path = "/srv/pelago-split/pelago"
                tag = "pelago-v5.0.0-split.4"
                commit = "{commit}"
                required_check = "pelago/required"
            "#
    ))
    .unwrap()
}

#[test]
fn lock_refuses_a_member_that_is_not_pinned_to_a_full_commit_id() {
    // A branch, a tag, a moving alias, a short id, an uppercase id and the
    // self sentinel on a member that is not the lock's own repository.
    for commit in [
        "main",
        "pelago-v5.0.0-split.4",
        "latest",
        "0123456789abcdef",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "PENDING",
    ] {
        let error = verify_lock_value(&member_lock(commit))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!("pelago commit is not a sha: {commit}"),
            "{commit} must not pass as a pin"
        );
    }
    // An empty value is a missing pin, reported as the missing field it is.
    let error = verify_lock_value(&member_lock("")).unwrap_err().to_string();
    assert!(error.contains("pelago missing commit"), "{error}");
    // The same lock passes once the member carries a full commit id.
    verify_lock_value(&member_lock("0123456789abcdef0123456789abcdef01234567")).unwrap();
}

#[test]
fn lock_refuses_an_empty_member_list() {
    for source in ["web_artifact = \"pinned\"\n", "repo = []\n", ""] {
        let value: Value = toml::from_str(source).unwrap();
        let error = verify_lock_value(&value).unwrap_err().to_string();
        assert_eq!(error, "lock must contain [[repo]] entries");
    }
}

#[test]
fn lock_allows_the_self_pin_only_once() {
    let mut lock = member_lock(SELF_PIN);
    verify_lock_value(&lock).unwrap();

    let second = member_lock(SELF_PIN)["repo"].as_array().unwrap()[0].clone();
    let mut second = second.as_table().unwrap().clone();
    second.insert("name".to_string(), Value::String("pelago-tool".to_string()));
    second.insert(
        "local_path".to_string(),
        Value::String("/srv/pelago-split/pelago-tool".to_string()),
    );
    second.insert(
        "required_check".to_string(),
        Value::String("pelago-tool/required".to_string()),
    );
    lock["repo"]
        .as_array_mut()
        .unwrap()
        .push(Value::Table(second));
    let error = verify_lock_value(&lock).unwrap_err().to_string();
    assert!(
        error.contains("pelago-tool is a second PENDING_SELF entry"),
        "{error}"
    );
}

/// An authority manifest for an invented family, in the shape
/// `jeryu-release-ops` publishes: the control plane under `[control_plane]`
/// with no `[[repo]]` row, every other member one `[[repo]]` row.
fn authority(control_path: &str, member_path: &str) -> String {
    format!(
        r#"
schema_version = "1"
repo_family = "pelago-split"
split_root = "/srv/pelago-split"
required_repos = ["pelago", "pelago-release-ops", "pelago-tool"]

[control_plane]
name = "pelago-release-ops"
path = "{control_path}"
jeryu_slug = "pelago/pelago-release-ops"
remote = "https://forge.invalid/git/pelago/pelago-release-ops.git"
required_check = "pelago-release-ops/required"
default_branch = "main"
identity_status = "bound"
predecessor_tag = "pelago-release-ops-v5.0.0-split.1"
inventory_status = "active"
runtime_authority = "control-plane"
lfs_required = false

[[repo]]
name = "pelago"
path = "{member_path}"
jeryu_slug = "pelago/pelago"
remote = "https://forge.invalid/git/pelago/pelago.git"
required_check = "pelago/required"
default_branch = "main"
identity_status = "bound"
current_tag = "pelago-v5.0.0-split.4"
inventory_status = "active"
runtime_authority = "library"
lfs_required = false

[[repo]]
name = "pelago-tool"
path = "/srv/pelago-split/pelago-tool"
jeryu_slug = "pelago/pelago-tool"
remote = "https://forge.invalid/git/pelago/pelago-tool.git"
required_check = "pelago-tool/required"
default_branch = "main"
identity_status = "pending"
inventory_status = "active"
runtime_authority = "library"
lfs_required = false

[[repo]]
name = "pelago-attic"
path = "/srv/pelago-split/pelago-attic"
jeryu_slug = "pelago/pelago-attic"
remote = "https://forge.invalid/git/pelago/pelago-attic.git"
required_check = "pelago-attic/required"
default_branch = "main"
identity_status = "bound"
current_tag = "pelago-attic-v5.0.0-split.0"
inventory_status = "withdrawn"
runtime_authority = "library"
lfs_required = false
"#
    )
}

#[test]
fn the_control_plane_is_a_member_and_inactive_rows_are_not() {
    let parsed = family::parse(&authority(
        "/srv/pelago-split/pelago-release-ops",
        "/srv/pelago-split/pelago",
    ))
    .unwrap();

    assert_eq!(parsed.repo_family, "pelago-split");
    assert_eq!(parsed.split_root, PathBuf::from("/srv/pelago-split"));
    let names: Vec<&str> = parsed
        .members
        .iter()
        .map(|member| member.name.as_str())
        .collect();
    assert_eq!(names, ["pelago-release-ops", "pelago", "pelago-tool"]);
    assert_eq!(
        parsed.members[0].tag.as_deref(),
        Some("pelago-release-ops-v5.0.0-split.1")
    );
    assert_eq!(
        parsed.members[1].tag.as_deref(),
        Some("pelago-v5.0.0-split.4")
    );
    assert_eq!(parsed.members[2].tag, None);
}

#[test]
fn required_repos_must_name_exactly_the_active_members() {
    let source = authority(
        "/srv/pelago-split/pelago-release-ops",
        "/srv/pelago-split/pelago",
    );

    let missing = source.replace(
        r#"required_repos = ["pelago", "pelago-release-ops", "pelago-tool"]"#,
        r#"required_repos = ["pelago", "pelago-cache", "pelago-release-ops", "pelago-tool"]"#,
    );
    assert!(
        family::parse(&missing)
            .unwrap_err()
            .to_string()
            .contains("required_repos names no active member: pelago-cache")
    );

    let unlisted = source.replace(
        r#"required_repos = ["pelago", "pelago-release-ops", "pelago-tool"]"#,
        r#"required_repos = ["pelago", "pelago-release-ops"]"#,
    );
    assert!(
        family::parse(&unlisted)
            .unwrap_err()
            .to_string()
            .contains("active members are missing from required_repos: pelago-tool")
    );
}

#[test]
fn a_bound_member_without_a_tag_is_rejected() {
    let source = authority(
        "/srv/pelago-split/pelago-release-ops",
        "/srv/pelago-split/pelago",
    )
    .replace("current_tag = \"pelago-v5.0.0-split.4\"\n", "");

    assert!(
        family::parse(&source)
            .unwrap_err()
            .to_string()
            .contains("pelago is bound without a current_tag")
    );
}

#[test]
fn a_duplicate_member_identity_is_rejected() {
    let source = authority(
        "/srv/pelago-split/pelago-release-ops",
        "/srv/pelago-split/pelago-release-ops",
    );

    assert!(
        family::parse(&source)
            .unwrap_err()
            .to_string()
            .contains("duplicate member name, path, or slug: pelago")
    );
}

#[test]
fn the_authority_is_located_from_the_environment_or_a_sibling_control_plane() {
    let temporary = tempfile::tempdir().unwrap();
    let split_root = temporary.path().join("split");
    let control = split_root.join(family::CONTROL_PLANE_DIR);
    fs::create_dir_all(&control).unwrap();
    let manifest = control.join(family::MANIFEST_FILE);
    fs::write(&manifest, "schema_version = \"1\"\n").unwrap();
    let member = split_root.join("consumer");
    fs::create_dir_all(&member).unwrap();

    assert_eq!(
        family::locate(&member).unwrap().canonicalize().unwrap(),
        manifest.canonicalize().unwrap()
    );
    let error = family::locate(temporary.path()).unwrap_err().to_string();
    assert!(error.contains(family::MANIFEST_ENV), "{error}");
    assert!(error.contains(family::CONTROL_PLANE_DIR), "{error}");
}

#[test]
fn path_validation_is_physical_over_the_members_the_authority_names() {
    let temporary = tempfile::tempdir().unwrap();
    let control = temporary.path().join("pelago-release-ops");
    let member = temporary.path().join("pelago");
    for root in [&control, &member] {
        fs::create_dir_all(root.join("agent")).unwrap();
        for path in ["AGENTS.md", "agent/owner-map.json", "agent/test-map.json"] {
            fs::write(root.join(path), b"{}\n").unwrap();
        }
    }
    let source = authority(
        &control.display().to_string(),
        &member.display().to_string(),
    );
    let parsed = family::parse(&source).unwrap();
    assert!(
        parsed
            .check_paths()
            .unwrap_err()
            .to_string()
            .contains("pelago-tool path missing: /srv/pelago-split/pelago-tool")
    );

    let two_members = source.replace(
        r#"
[[repo]]
name = "pelago-tool"
path = "/srv/pelago-split/pelago-tool"
jeryu_slug = "pelago/pelago-tool"
remote = "https://forge.invalid/git/pelago/pelago-tool.git"
required_check = "pelago-tool/required"
default_branch = "main"
identity_status = "pending"
inventory_status = "active"
runtime_authority = "library"
lfs_required = false
"#,
        "\n",
    );
    let two_members = two_members.replace(
        r#"required_repos = ["pelago", "pelago-release-ops", "pelago-tool"]"#,
        r#"required_repos = ["pelago", "pelago-release-ops"]"#,
    );
    family::parse(&two_members).unwrap().check_paths().unwrap();

    fs::remove_file(member.join("agent/test-map.json")).unwrap();
    assert!(
        family::parse(&two_members)
            .unwrap()
            .check_paths()
            .unwrap_err()
            .to_string()
            .contains("pelago missing agent/test-map.json")
    );
}

#[test]
fn fleet_plan_preserves_authority_order_and_selects_one_lane() {
    let parsed = family::parse(&authority(
        "/srv/pelago-split/pelago-release-ops",
        "/srv/pelago-split/pelago",
    ))
    .unwrap();

    let score = fleet_entries(&parsed, false);
    assert_eq!(
        score[0],
        (
            "pelago-release-ops".to_owned(),
            PathBuf::from("/srv/pelago-split/pelago-release-ops"),
            "score".to_owned()
        )
    );
    assert_eq!(
        score[1],
        (
            "pelago".to_owned(),
            PathBuf::from("/srv/pelago-split/pelago"),
            "score".to_owned()
        )
    );
    assert!(
        fleet_entries(&parsed, true)
            .iter()
            .all(|(_, _, lane)| lane == "check")
    );
}

#[test]
fn governed_process_failure_is_not_swallowed() {
    let error = run_process(ProcessCommand::new("sh").args(["-c", "exit 17"]), "fixture")
        .unwrap_err()
        .to_string();
    assert!(error.contains("governed command for fixture failed"));
    assert!(error.contains("17"));
}
