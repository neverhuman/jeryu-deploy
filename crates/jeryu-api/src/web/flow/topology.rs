//! The delivery topology of a family: which repositories contribute to which
//! product, where their changes converge, and what each repository's route
//! actually requires.
//!
//! Everything here is read from configuration and recorded evidence, never
//! from a repository's name:
//!
//! * **Products** come from the `[[product]]` tables of the split manifests the
//!   server was given (`--split-manifest`). A family that declares none gets
//!   one product holding every configured repository, which is what most
//!   families are (`docs/flow.md`, "Family, product, repository").
//! * **Contributing repositories** are the hosted repositories of the family,
//!   so a configured repository with no work in flight still appears.
//! * **The convergence point** is the repository whose default branch holds the
//!   product's pin lock (`jeryu-split.lock.toml` unless the product names
//!   another): the repositories that lock names are the ones pinned into it. A
//!   product may also declare `integration_repo` outright, which wins.
//! * **The route** of a repository is its default branch's protection rule:
//!   how many approvals it requires and which checks. A repository the forge
//!   holds no rule for requires nothing by it, which is reported as a
//!   configured no-gate route rather than as unknown; a repository with no
//!   recorded default branch has no rule to read and stays unknown.
//!
//! What cannot be derived is disclosed in [`Topology::unknown`] with a reason
//! instead of being guessed, and nothing here infers that work was delivered.

pub(crate) mod sources;

pub(crate) use sources::{Cache, inputs};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::closure::OpenVisit;

/// The pin lock a product uses when it names none.
pub(crate) const DEFAULT_PIN_LOCK: &str = "jeryu-split.lock.toml";
/// How many families one answer describes, so an unscoped read stays bounded.
const MAX_FAMILIES: usize = 8;
/// How long parsed manifests and read pin locks are reused.
const CONFIG_TTL: Duration = Duration::from_secs(60);

/// A `[[product]]` table of a split manifest.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct ProductConfig {
    pub id: String,
    pub label: Option<String>,
    /// The repositories whose changes this product delivers, as `owner/name`.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Where those changes converge. Declared here it wins over the pin lock.
    pub integration_repo: Option<String>,
    /// What is staged and deployed; the integration repository by default.
    pub release_repo: Option<String>,
    /// The lock file that records this product's pins.
    pub pin_lock: Option<String>,
}

/// One family's declared products, as the manifests spell them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FamilyConfig {
    pub family: String,
    pub products: Vec<ProductConfig>,
}

/// What one repository's configuration says about its route and its pins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RepoFacts {
    pub repo: String,
    pub family: String,
    pub default_branch: String,
    /// `Some` when the forge holds a protection rule for the default branch.
    /// `None` is "no rule", which requires nothing.
    pub protection: Option<Protection>,
    /// The repositories the default branch's pin lock names, when it holds one.
    pub pins: Option<Vec<String>>,
    /// True when the lock could not be read at all, which is not "no lock".
    pub lock_unreadable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Protection {
    pub approvals: u64,
    pub required_checks: Vec<String>,
}

/// Everything [`plan`] reasons over: configuration first, recorded evidence
/// only where configuration is silent.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    /// The family the report was asked for, when it was asked for one.
    pub scope: Option<String>,
    pub config: Vec<FamilyConfig>,
    pub repos: Vec<RepoFacts>,
    /// Repositories that have recorded a staging or production deployment.
    pub release_evidence: BTreeSet<String>,
    /// Repositories that have recorded a pin bump pull request.
    pub pin_evidence: BTreeSet<String>,
}

/// A repository's configured review and check requirements.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Route {
    /// `automated`, `human_only`, `no_gate` or `unknown`.
    pub kind: &'static str,
    /// `branch_protection`, `no_protection_rule` or `unreadable`.
    pub source: &'static str,
    pub approvals: Option<u64>,
    pub required_checks: Option<Vec<String>>,
}

impl Route {
    fn of(facts: &RepoFacts) -> Self {
        match &facts.protection {
            None if facts.default_branch.is_empty() => Self {
                kind: "unknown",
                source: "unreadable",
                approvals: None,
                required_checks: None,
            },
            None => Self {
                kind: "no_gate",
                source: "no_protection_rule",
                approvals: Some(0),
                required_checks: Some(Vec::new()),
            },
            Some(rule) => Self {
                kind: match (rule.approvals, rule.required_checks.is_empty()) {
                    (0, true) => "no_gate",
                    (_, true) => "human_only",
                    _ => "automated",
                },
                source: "branch_protection",
                approvals: Some(rule.approvals),
                required_checks: Some(rule.required_checks.clone()),
            },
        }
    }
}

/// One contributing repository of a product.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RepoNode {
    pub repo: String,
    /// `source` for a repository whose changes converge elsewhere,
    /// `integration` for the repository they converge into.
    pub role: &'static str,
    /// True when more than one product of the family delivers this repository.
    pub shared: bool,
    /// Where its merges are proposed next, and what says so.
    pub pins_into: Vec<String>,
    /// `pin_lock`, `declared`, `recorded_pin_events`, `none` or `unknown`.
    pub pin_source: &'static str,
    pub route: Route,
    /// Distinct lots with an open recorded visit in this repository.
    pub active_lots: u32,
    /// True when the repository is configured but has no open recorded visit.
    pub no_current_work: bool,
}

/// Distinct work, counted once per lot however many stations or repositories
/// its visits touch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Work {
    pub active_lots: u32,
    /// One bucket per lot, by its furthest recorded evidence.
    pub changes: u32,
    pub pull_requests: u32,
    pub pin_pull_requests: u32,
    pub release_artifacts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Release {
    pub repo: String,
    /// `declared`, `integration_repo` or `recorded_deliveries`.
    pub source: &'static str,
    pub stations: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Product {
    pub id: String,
    pub label: String,
    pub family: String,
    /// `declared` for a `[[product]]` table, `family_default` for the one
    /// product a family with no declaration gets.
    pub source: &'static str,
    pub repos: Vec<RepoNode>,
    /// The repository the product's contributions converge into, when
    /// configuration or the pin lock says which.
    pub integration: Option<String>,
    pub release: Option<Release>,
    pub work: Work,
}

/// Something configuration or evidence does not say, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct UnknownNote {
    /// The family, product or repository the gap is about.
    pub scope: String,
    pub field: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FamilySummary {
    pub family: String,
    pub products: u32,
    /// Configured contributing repositories, counted once even when shared.
    pub repos: u32,
    pub work: Work,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Topology {
    pub families: Vec<FamilySummary>,
    pub products: Vec<Product>,
    /// Distinct active lots whose visits name no repository, so they belong to
    /// no product. Counted in no product's totals.
    pub unscoped_active_lots: u32,
    pub unknown: Vec<UnknownNote>,
    /// True when more families exist than one answer describes.
    pub families_truncated: bool,
}

/// What a lot's furthest open visit says it is.
fn kind_of(stations: &BTreeSet<String>, has_pr: bool, in_integration: bool) -> &'static str {
    if stations.contains("stage") || stations.contains("deploy") {
        "release_artifact"
    } else if stations.contains("pin") || (in_integration && has_pr) {
        "pin_pull_request"
    } else if has_pr {
        "pull_request"
    } else {
        "change"
    }
}

/// One lot's open visits, gathered so a lot is counted once.
#[derive(Default)]
struct LotFacts {
    repos: BTreeSet<String>,
    stations: BTreeSet<String>,
    has_pr: bool,
}

/// The topology one family scope describes, and the distinct work on it.
pub(crate) fn plan(inputs: &Inputs, open: &[OpenVisit]) -> Topology {
    let mut facts_by_repo: BTreeMap<String, &RepoFacts> = BTreeMap::new();
    for facts in &inputs.repos {
        facts_by_repo.insert(facts.repo.clone(), facts);
    }
    let mut lots: BTreeMap<String, LotFacts> = BTreeMap::new();
    for visit in open {
        let lot = lots.entry(visit.lot.clone()).or_default();
        lot.stations.insert(visit.station.clone());
        lot.has_pr |= visit.pr.is_some();
        if let Some(repo) = visit.repo.as_ref() {
            lot.repos.insert(repo.clone());
        }
    }

    let mut topology = Topology::default();
    let mut families: Vec<String> = inputs
        .repos
        .iter()
        .map(|facts| facts.family.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(scope) = inputs.scope.as_ref() {
        families.retain(|family| family == scope);
        if families.is_empty() {
            families.push(scope.clone());
        }
    }
    if families.len() > MAX_FAMILIES {
        families.truncate(MAX_FAMILIES);
        topology.families_truncated = true;
    }

    for family in families {
        let repos: Vec<&RepoFacts> = inputs
            .repos
            .iter()
            .filter(|facts| facts.family == family)
            .collect();
        if repos.is_empty() {
            topology.unknown.push(UnknownNote {
                scope: family.clone(),
                field: "repositories",
                reason: "no_configured_repository",
            });
        }
        let declared: Vec<ProductConfig> = inputs
            .config
            .iter()
            .filter(|config| config.family == family)
            .flat_map(|config| config.products.clone())
            .collect();
        let known: BTreeSet<&str> = repos.iter().map(|facts| facts.repo.as_str()).collect();
        let mut product_repos: Vec<(ProductConfig, Vec<String>, &'static str)> = Vec::new();
        if declared.is_empty() {
            // Most families are one product. The default holds every
            // configured repository, so nothing is lost by not declaring it.
            product_repos.push((
                ProductConfig {
                    id: family.clone(),
                    label: Some(family.clone()),
                    ..ProductConfig::default()
                },
                repos.iter().map(|facts| facts.repo.clone()).collect(),
                "family_default",
            ));
        } else {
            for product in declared {
                let mut members: Vec<String> = Vec::new();
                for repo in product
                    .repos
                    .iter()
                    .chain(product.integration_repo.iter())
                    .chain(product.release_repo.iter())
                {
                    let repo = repo.to_ascii_lowercase();
                    if !known.contains(repo.as_str()) {
                        topology.unknown.push(UnknownNote {
                            scope: repo.clone(),
                            field: "repository",
                            reason: "declared_but_not_hosted",
                        });
                        continue;
                    }
                    if !members.contains(&repo) {
                        members.push(repo);
                    }
                }
                product_repos.push((product, members, "declared"));
            }
            let claimed: BTreeSet<&String> = product_repos
                .iter()
                .flat_map(|(_, members, _)| members.iter())
                .collect();
            for facts in &repos {
                if !claimed.contains(&facts.repo) {
                    topology.unknown.push(UnknownNote {
                        scope: facts.repo.clone(),
                        field: "product",
                        reason: "no_declared_product",
                    });
                }
            }
        }

        let mut shared_count: BTreeMap<&String, usize> = BTreeMap::new();
        for (_, members, _) in &product_repos {
            for repo in members {
                *shared_count.entry(repo).or_default() += 1;
            }
        }

        let mut family_lots: BTreeSet<&String> = BTreeSet::new();
        let mut family_repos: BTreeSet<&String> = BTreeSet::new();
        let mut product_count = 0_u32;
        for (config, members, source) in &product_repos {
            product_count += 1;
            let integration = integration_of(config, members, &facts_by_repo);
            let mut product = Product {
                label: config
                    .label
                    .clone()
                    .filter(|label| !label.trim().is_empty())
                    .unwrap_or_else(|| config.id.clone()),
                id: config.id.clone(),
                family: family.clone(),
                source,
                repos: Vec::new(),
                integration: integration.clone(),
                release: None,
                work: Work::default(),
            };
            if integration.is_none() {
                topology.unknown.push(UnknownNote {
                    scope: product.id.clone(),
                    field: "integration_repo",
                    reason: "no_pin_lock_or_declaration",
                });
            }
            let mut product_lots: BTreeMap<&String, &LotFacts> = BTreeMap::new();
            for repo in members {
                family_repos.insert(repo);
                let facts = facts_by_repo.get(repo.as_str());
                let mut lots_here: BTreeSet<&String> = BTreeSet::new();
                for (lot, lot_facts) in &lots {
                    if lot_facts.repos.contains(repo) {
                        lots_here.insert(lot);
                        product_lots.insert(lot, lot_facts);
                        family_lots.insert(lot);
                    }
                }
                let (pins_into, pin_source) =
                    pins_of(repo, config, members, &facts_by_repo, inputs);
                if pin_source == "unknown" {
                    topology.unknown.push(UnknownNote {
                        scope: repo.clone(),
                        field: "pins_into",
                        reason: "pin_lock_unreadable",
                    });
                }
                let route = facts.map(|facts| Route::of(facts)).unwrap_or(Route {
                    kind: "unknown",
                    source: "unreadable",
                    approvals: None,
                    required_checks: None,
                });
                if route.kind == "unknown" {
                    topology.unknown.push(UnknownNote {
                        scope: repo.clone(),
                        field: "route",
                        reason: "protection_unreadable",
                    });
                }
                product.repos.push(RepoNode {
                    repo: repo.clone(),
                    role: if Some(repo) == integration.as_ref() {
                        "integration"
                    } else {
                        "source"
                    },
                    shared: shared_count.get(repo).copied().unwrap_or(0) > 1,
                    pins_into,
                    pin_source,
                    route,
                    active_lots: u32::try_from(lots_here.len()).unwrap_or(u32::MAX),
                    no_current_work: lots_here.is_empty(),
                });
            }
            product.work = work_of(
                product_lots.iter().map(|(lot, facts)| (*lot, *facts)),
                integration.as_deref(),
            );
            product.release = release_of(config, members, integration.as_deref(), inputs);
            if product.release.is_none() {
                topology.unknown.push(UnknownNote {
                    scope: product.id.clone(),
                    field: "release_repo",
                    reason: "no_release_route_configured",
                });
            }
            topology.products.push(product);
        }
        let work = work_of(
            family_lots
                .iter()
                .filter_map(|lot| lots.get_key_value(*lot)),
            None,
        );
        topology.families.push(FamilySummary {
            family,
            products: product_count,
            repos: u32::try_from(family_repos.len()).unwrap_or(u32::MAX),
            work,
        });
    }

    topology.unscoped_active_lots = u32::try_from(
        lots.iter()
            .filter(|(_, facts)| facts.repos.is_empty())
            .count(),
    )
    .unwrap_or(u32::MAX);
    if topology.unscoped_active_lots > 0 {
        topology.unknown.push(UnknownNote {
            scope: "family".into(),
            field: "lot_repository",
            reason: "visits_name_no_repository",
        });
    }
    // The same note can be raised once per product, not only back to back.
    let mut kept: Vec<UnknownNote> = Vec::new();
    for note in std::mem::take(&mut topology.unknown) {
        if !kept.contains(&note) {
            kept.push(note);
        }
    }
    topology.unknown = kept;
    topology
}

/// Distinct lots, each counted once in the bucket its furthest evidence names.
fn work_of<'a>(
    lots: impl Iterator<Item = (&'a String, &'a LotFacts)>,
    integration: Option<&str>,
) -> Work {
    let mut work = Work::default();
    for (_, facts) in lots {
        work.active_lots += 1;
        let in_integration =
            integration.is_some_and(|repo| facts.repos.iter().any(|at| at == repo));
        match kind_of(&facts.stations, facts.has_pr, in_integration) {
            "release_artifact" => work.release_artifacts += 1,
            "pin_pull_request" => work.pin_pull_requests += 1,
            "pull_request" => work.pull_requests += 1,
            _ => work.changes += 1,
        }
    }
    work
}

/// The repository a product's contributions converge into: the declaration if
/// it has one, else the member repository whose pin lock names other members.
fn integration_of(
    config: &ProductConfig,
    members: &[String],
    facts: &BTreeMap<String, &RepoFacts>,
) -> Option<String> {
    if let Some(declared) = config
        .integration_repo
        .as_ref()
        .map(|repo| repo.to_ascii_lowercase())
        .filter(|repo| members.contains(repo))
    {
        return Some(declared);
    }
    members
        .iter()
        .find(|repo| {
            facts
                .get(repo.as_str())
                .and_then(|facts| facts.pins.as_ref())
                .is_some_and(|pins| pins.iter().any(|pinned| members.contains(pinned)))
        })
        .cloned()
}

/// Where one repository's merges are proposed next, and what says so.
fn pins_of(
    repo: &String,
    config: &ProductConfig,
    members: &[String],
    facts: &BTreeMap<String, &RepoFacts>,
    inputs: &Inputs,
) -> (Vec<String>, &'static str) {
    let consumers: Vec<String> = members
        .iter()
        .filter(|consumer| *consumer != repo)
        .filter(|consumer| {
            facts
                .get(consumer.as_str())
                .and_then(|facts| facts.pins.as_ref())
                .is_some_and(|pins| pins.contains(repo))
        })
        .cloned()
        .collect();
    if !consumers.is_empty() {
        return (consumers, "pin_lock");
    }
    if let Some(declared) = config
        .integration_repo
        .as_ref()
        .map(|target| target.to_ascii_lowercase())
        .filter(|target| target != repo && members.contains(target))
    {
        return (vec![declared], "declared");
    }
    // A member whose lock could not be read may well pin this repository.
    if members.iter().any(|member| {
        facts
            .get(member.as_str())
            .is_some_and(|facts| facts.lock_unreadable)
    }) {
        return (Vec::new(), "unknown");
    }
    if inputs.pin_evidence.contains(repo) {
        // This repository receives pin bumps itself; it proposes none onward.
        return (Vec::new(), "none");
    }
    (Vec::new(), "none")
}

/// What is staged and deployed for a product.
fn release_of(
    config: &ProductConfig,
    members: &[String],
    integration: Option<&str>,
    inputs: &Inputs,
) -> Option<Release> {
    let stations = vec!["stage", "deploy"];
    if let Some(declared) = config.release_repo.as_ref() {
        return Some(Release {
            repo: declared.to_ascii_lowercase(),
            source: "declared",
            stations,
        });
    }
    if let Some(repo) = integration {
        return Some(Release {
            repo: repo.to_string(),
            source: "integration_repo",
            stations,
        });
    }
    inputs
        .release_evidence
        .iter()
        .find(|repo| members.contains(repo))
        .map(|repo| Release {
            repo: repo.clone(),
            source: "recorded_deliveries",
            stations,
        })
}
