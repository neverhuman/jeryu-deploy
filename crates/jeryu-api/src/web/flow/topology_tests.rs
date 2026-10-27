//! The delivery topology: products from configuration, convergence from the
//! pin lock, routes from branch protection, and work counted once.

use std::collections::BTreeSet;

use super::closure::OpenVisit;
use super::topology::sources::{lock_name_ok, parse_manifests};
use super::topology::{FamilyConfig, Inputs, ProductConfig, Protection, RepoFacts, plan};

fn repo(repo: &str, family: &str) -> RepoFacts {
    RepoFacts {
        repo: repo.into(),
        family: family.into(),
        default_branch: "main".into(),
        protection: None,
        protection_unreadable: false,
        pins: None,
        lock_unreadable: false,
    }
}

fn visit(lot: &str, station: &str, at: Option<&str>, pr: Option<u64>) -> OpenVisit {
    OpenVisit {
        lot: lot.into(),
        station: station.into(),
        entered: 1,
        repo: at.map(str::to_string),
        pr,
        family: Some("jeryu".into()),
        started: false,
        held: false,
        todo: false,
        landed: false,
        back_from: None,
    }
}

/// The Jeryu family declares no product, so it is one product holding every
/// configured repository -- including one with nothing in flight -- and the
/// chain is the one its pin lock records, not one its names imply.
#[test]
fn the_default_product_holds_every_configured_repo_and_the_locked_chain() {
    let mut deploy = repo("jeryu/jeryu-deploy", "jeryu");
    deploy.pins = Some(vec!["jeryu/jeryu-web".into(), "jeryu/jeryu-core".into()]);
    deploy.protection = Some(Protection {
        approvals: 1,
        required_checks: vec!["jeryu-deploy/required".into()],
    });
    let mut web = repo("jeryu/jeryu-web", "jeryu");
    web.protection = Some(Protection {
        approvals: 1,
        required_checks: vec!["jeryu-web/required".into()],
    });
    let inputs = Inputs {
        scope: Some("jeryu".into()),
        repos: vec![deploy, web, repo("jeryu/jeryu-core", "jeryu")],
        ..Inputs::default()
    };
    let open = vec![
        visit("todo-a", "review", Some("jeryu/jeryu-web"), Some(7)),
        visit("todo-a", "gate", Some("jeryu/jeryu-web"), Some(7)),
        visit("todo-b", "pin", Some("jeryu/jeryu-deploy"), Some(378)),
    ];
    let shape = plan(&inputs, &open);
    assert_eq!(shape.products.len(), 1);
    let product = &shape.products[0];
    assert_eq!(product.id, "jeryu");
    assert_eq!(product.source, "family_default");
    assert_eq!(product.integration.as_deref(), Some("jeryu/jeryu-deploy"));
    assert_eq!(
        product.release.as_ref().map(|r| r.repo.as_str()),
        Some("jeryu/jeryu-deploy")
    );
    assert_eq!(
        product.release.as_ref().map(|r| r.source),
        Some("integration_repo")
    );
    let names: Vec<&str> = product.repos.iter().map(|r| r.repo.as_str()).collect();
    assert_eq!(
        names,
        vec!["jeryu/jeryu-deploy", "jeryu/jeryu-web", "jeryu/jeryu-core"]
    );
    let web = product
        .repos
        .iter()
        .find(|node| node.repo == "jeryu/jeryu-web")
        .unwrap();
    assert_eq!(web.role, "source");
    assert_eq!(web.pins_into, vec!["jeryu/jeryu-deploy".to_string()]);
    assert_eq!(web.pin_source, "pin_lock");
    assert_eq!(web.route.kind, "automated");
    assert_eq!(web.route.approvals, Some(1));
    assert_eq!(web.active_lots, 1, "two parallel visits are one lot");
    let core = product
        .repos
        .iter()
        .find(|node| node.repo == "jeryu/jeryu-core")
        .unwrap();
    assert!(
        core.no_current_work,
        "a configured repo with no WIP is shown"
    );
    assert_eq!(core.route.kind, "no_gate");
    assert_eq!(core.route.source, "no_protection_rule");
    // Two lots, counted once each and in one bucket each.
    assert_eq!(product.work.active_lots, 2);
    assert_eq!(product.work.pull_requests, 1);
    assert_eq!(product.work.pin_pull_requests, 1);
    assert_eq!(shape.families.len(), 1);
    assert_eq!(shape.families[0].repos, 3);
    assert_eq!(shape.families[0].work.active_lots, 2);
}

/// veox-ai groups several products and shares a dependency between them. The
/// shared repository is marked, both products show it, and neither the family
/// repository count nor the family work total counts it twice.
#[test]
fn declared_products_share_a_dependency_without_duplicating_family_totals() {
    let mut portal_deploy = repo("veox-ai/veox-portal-deploy", "veox-ai");
    portal_deploy.pins = Some(vec![
        "veox-ai/veox-portal".into(),
        "veox-ai/veox-shared".into(),
    ]);
    let mut studio_deploy = repo("veox-ai/veox-studio-deploy", "veox-ai");
    studio_deploy.pins = Some(vec![
        "veox-ai/veox-studio".into(),
        "veox-ai/veox-shared".into(),
    ]);
    let inputs = Inputs {
        scope: Some("veox-ai".into()),
        config: vec![FamilyConfig {
            family: "veox-ai".into(),
            products: vec![
                ProductConfig {
                    id: "portal".into(),
                    label: Some("Veox portal".into()),
                    repos: vec![
                        "veox-ai/veox-portal".into(),
                        "veox-ai/veox-shared".into(),
                        "veox-ai/veox-portal-deploy".into(),
                    ],
                    ..ProductConfig::default()
                },
                ProductConfig {
                    id: "studio".into(),
                    label: Some("Veox studio".into()),
                    repos: vec![
                        "veox-ai/veox-studio".into(),
                        "veox-ai/veox-shared".into(),
                        "veox-ai/veox-studio-deploy".into(),
                    ],
                    ..ProductConfig::default()
                },
            ],
        }],
        repos: vec![
            repo("veox-ai/veox-portal", "veox-ai"),
            repo("veox-ai/veox-studio", "veox-ai"),
            repo("veox-ai/veox-shared", "veox-ai"),
            portal_deploy,
            studio_deploy,
        ],
        ..Inputs::default()
    };
    // One shared change, in flight in the shared repository only.
    let open = vec![
        visit("todo-shared", "gate", Some("veox-ai/veox-shared"), Some(5)),
        visit("todo-portal", "work", Some("veox-ai/veox-portal"), None),
    ];
    let shape = plan(&inputs, &open);
    assert_eq!(shape.products.len(), 2);
    let portal = &shape.products[0];
    let studio = &shape.products[1];
    assert_eq!(portal.label, "Veox portal");
    assert_eq!(portal.source, "declared");
    assert_eq!(
        portal.integration.as_deref(),
        Some("veox-ai/veox-portal-deploy")
    );
    assert_eq!(
        studio.integration.as_deref(),
        Some("veox-ai/veox-studio-deploy")
    );
    for product in [portal, studio] {
        let shared = product
            .repos
            .iter()
            .find(|node| node.repo == "veox-ai/veox-shared")
            .expect("both products deliver the shared repository");
        assert!(shared.shared);
        assert_eq!(shared.active_lots, 1);
    }
    // The shared lot is in both products' own totals, and once in the family's.
    assert_eq!(portal.work.active_lots, 2);
    assert_eq!(studio.work.active_lots, 1);
    assert_eq!(shape.families.len(), 1);
    assert_eq!(shape.families[0].products, 2);
    assert_eq!(
        shape.families[0].repos, 5,
        "the shared repo is counted once"
    );
    assert_eq!(
        shape.families[0].work.active_lots, 2,
        "the shared lot is not counted twice for the family"
    );
}

/// A human-only route and an unreadable pin lock are reported as they are.
#[test]
fn human_only_routes_and_unreadable_locks_are_disclosed_not_guessed() {
    let mut deploy = repo("acme/acme-deploy", "acme");
    deploy.lock_unreadable = true;
    let mut source = repo("acme/acme-app", "acme");
    source.protection = Some(Protection {
        approvals: 2,
        required_checks: Vec::new(),
    });
    let inputs = Inputs {
        scope: Some("acme".into()),
        repos: vec![deploy, source],
        release_evidence: BTreeSet::from(["acme/acme-deploy".to_string()]),
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[]);
    let product = &shape.products[0];
    let app = product
        .repos
        .iter()
        .find(|node| node.repo == "acme/acme-app")
        .unwrap();
    assert_eq!(app.route.kind, "human_only");
    assert_eq!(app.route.required_checks, Some(Vec::new()));
    assert_eq!(app.pin_source, "unknown");
    assert!(app.pins_into.is_empty());
    assert!(
        product.integration.is_none(),
        "no lock, no convergence claim"
    );
    assert!(
        shape.unknown.iter().any(|note| {
            note.scope == "acme/acme-app"
                && note.field == "pins_into"
                && note.reason == "pin_lock_unreadable"
        }),
        "{:?}",
        shape.unknown
    );
    assert!(
        shape
            .unknown
            .iter()
            .any(|note| note.field == "integration_repo"),
        "{:?}",
        shape.unknown
    );
    // With no convergence point configuration names, recorded deliveries are
    // the only evidence of a release route, and they are labelled as such.
    assert_eq!(
        product.release.as_ref().map(|r| r.source),
        Some("recorded_deliveries")
    );
}

/// A lot whose visits name no repository belongs to no product and is
/// disclosed rather than attributed to one.
#[test]
fn a_lot_with_no_repository_is_disclosed_outside_every_product() {
    let inputs = Inputs {
        scope: Some("jeryu".into()),
        repos: vec![repo("jeryu/jeryu-web", "jeryu")],
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[visit("todo-x", "queue", None, None)]);
    assert_eq!(shape.unscoped_active_lots, 1);
    assert_eq!(shape.products[0].work.active_lots, 0);
    assert_eq!(shape.families[0].work.active_lots, 0);
    assert!(
        shape
            .unknown
            .iter()
            .any(|note| note.field == "lot_repository")
    );
}

/// Products are read from the manifest the server was given, including a
/// repository that declares its own product membership.
#[test]
fn a_manifest_declares_products_and_shared_repositories() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("veox-split.toml");
    std::fs::write(
        &path,
        r#"
repo_family = "veox-ai-split"

[[product]]
id = "portal"
label = "Veox portal"
repos = ["veox-ai/veox-portal"]
integration_repo = "veox-ai/veox-portal-deploy"

[[product]]
id = "studio"
repos = ["veox-ai/veox-studio"]

[[repo]]
name = "veox-shared"
jeryu_slug = "veox-ai/veox-shared"
product = ["portal", "studio"]
"#,
    )
    .unwrap();
    let config = parse_manifests(&[path]);
    assert_eq!(config.len(), 1);
    assert_eq!(config[0].family, "veox-ai", "the -split alias is canonical");
    let portal = &config[0].products[0];
    assert_eq!(portal.label.as_deref(), Some("Veox portal"));
    assert_eq!(
        portal.integration_repo.as_deref(),
        Some("veox-ai/veox-portal-deploy")
    );
    assert_eq!(
        portal.repos,
        vec![
            "veox-ai/veox-portal".to_string(),
            "veox-ai/veox-shared".to_string()
        ]
    );
    assert_eq!(
        config[0].products[1].repos,
        vec![
            "veox-ai/veox-studio".to_string(),
            "veox-ai/veox-shared".to_string()
        ]
    );
    // A manifest with no product tables leaves the family on its default.
    let plain = dir.path().join("plain.toml");
    std::fs::write(
        &plain,
        "repo_family = 'jeryu-split'\n[[repo]]\nname='jeryu-web'\n",
    )
    .unwrap();
    assert!(parse_manifests(&[plain]).is_empty());
}

/// A lock name reaches a git argument, so only a plain path inside the tree is
/// read at all.
#[test]
fn only_a_plain_lock_path_is_read() {
    assert!(lock_name_ok("jeryu-split.lock.toml"));
    assert!(lock_name_ok("ops/pins.toml"));
    for refused in [
        "",
        "-ignore-all-space",
        "/etc/passwd",
        "../outside.toml",
        "ops/../../outside.toml",
        "ops//pins.toml",
        "pins toml",
    ] {
        assert!(!lock_name_ok(refused), "{refused:?} must be refused");
    }
}

/// An unscoped answer describes every family. The old cap of eight cut the
/// list alphabetically, so families late in the alphabet (and their
/// products) vanished from the forge-wide view while `families_truncated`
/// was the only trace.
#[test]
fn an_unscoped_answer_describes_every_family_however_many() {
    let names = [
        "acme", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
        "kilo", "lima",
    ];
    let inputs = Inputs {
        scope: None,
        repos: names
            .iter()
            .map(|family| repo(&format!("{family}/{family}-app"), family))
            .collect(),
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[]);
    let described: Vec<&str> = shape
        .families
        .iter()
        .map(|family| family.family.as_str())
        .collect();
    assert_eq!(described, names.to_vec());
    assert_eq!(shape.families_total, 12);
    assert!(!shape.families_truncated);
    assert_eq!(shape.products.len(), 12, "every family's product is listed");
}

/// A repository with no lock and no declaration that has recorded pin bump
/// pull requests says so, rather than reading the same as one nothing is known
/// about.
#[test]
fn recorded_pin_events_are_reported_as_the_pin_source() {
    let inputs = Inputs {
        scope: Some("acme".into()),
        repos: vec![repo("acme/acme-app", "acme"), repo("acme/acme-lib", "acme")],
        pin_evidence: BTreeSet::from(["acme/acme-app".to_string()]),
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[]);
    let node = |name: &str| {
        shape.products[0]
            .repos
            .iter()
            .find(|node| node.repo == name)
            .unwrap()
    };
    assert_eq!(node("acme/acme-app").pin_source, "recorded_pin_events");
    assert!(node("acme/acme-app").pins_into.is_empty());
    assert_eq!(node("acme/acme-lib").pin_source, "none");
}

/// A branch-protection rule that could not be read is unknown, not an
/// ungated branch.
#[test]
fn an_unreadable_protection_rule_is_not_reported_as_no_gate() {
    let mut app = repo("acme/acme-app", "acme");
    app.protection_unreadable = true;
    let inputs = Inputs {
        scope: Some("acme".into()),
        repos: vec![app, repo("acme/acme-lib", "acme")],
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[]);
    let node = |name: &str| {
        shape.products[0]
            .repos
            .iter()
            .find(|node| node.repo == name)
            .unwrap()
    };
    assert_eq!(node("acme/acme-app").route.kind, "unknown");
    assert_eq!(node("acme/acme-app").route.source, "unreadable");
    assert_eq!(node("acme/acme-lib").route.kind, "no_gate");
}

/// A release repository the forge does not host is reported as unknown, not
/// named as where the product is staged and deployed.
#[test]
fn a_declared_release_repo_the_forge_does_not_host_is_not_named() {
    let inputs = Inputs {
        scope: Some("acme".into()),
        config: vec![FamilyConfig {
            family: "acme".into(),
            products: vec![ProductConfig {
                id: "app".into(),
                repos: vec!["acme/acme-app".into()],
                release_repo: Some("acme/acme-missing".into()),
                ..ProductConfig::default()
            }],
        }],
        repos: vec![repo("acme/acme-app", "acme")],
        ..Inputs::default()
    };
    let shape = plan(&inputs, &[]);
    let product = &shape.products[0];
    assert!(product.release.is_none(), "{:?}", product.release);
    assert!(
        shape
            .unknown
            .iter()
            .any(|note| note.field == "release_repo")
    );
}
