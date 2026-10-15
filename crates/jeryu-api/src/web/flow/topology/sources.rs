//! Where the delivery topology is read from: the split manifests the server
//! was given, each repository's pin lock and branch rule, and the recorded
//! deliveries that are only consulted where configuration is silent.
//!
//! Reads are deliberately bounded and reused: the manifests are parsed once per
//! [`CONFIG_TTL`], and so are the repository facts of a family scope, so a Flow
//! read does not re-run a git command per repository on every refresh. See
//! `docs/flow.md`, "Family, product, repository".

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Deserialize;

use super::{
    CONFIG_TTL, DEFAULT_PIN_LOCK, FamilyConfig, Inputs, ProductConfig, Protection, RepoFacts,
};
use crate::web::WebState;
use crate::web::family::canonical;

#[derive(Debug, Deserialize)]
struct Manifest {
    repo_family: Option<String>,
    #[serde(default)]
    product: Vec<ProductConfig>,
    #[serde(default)]
    repo: Vec<ManifestRepo>,
}

#[derive(Debug, Deserialize)]
struct ManifestRepo {
    github_slug: Option<String>,
    jeryu_slug: Option<String>,
    /// The products this repository contributes to. More than one makes it a
    /// shared dependency of those products.
    #[serde(default)]
    product: Vec<String>,
}

/// Parse the `[[product]]` tables of every manifest, keyed by family.
///
/// A manifest the server cannot read or parse contributes nothing: the family
/// then falls back to its single default product, which is also what every
/// manifest written before products existed does.
pub(crate) fn parse_manifests(paths: &[PathBuf]) -> Vec<FamilyConfig> {
    let mut families: BTreeMap<String, Vec<ProductConfig>> = BTreeMap::new();
    for path in paths {
        let Some((family, products)) = parse_manifest(path) else {
            continue;
        };
        families.entry(family).or_default().extend(products);
    }
    families
        .into_iter()
        .map(|(family, products)| FamilyConfig { family, products })
        .collect()
}

fn parse_manifest(path: &Path) -> Option<(String, Vec<ProductConfig>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let manifest: Manifest = toml::from_str(&text).ok()?;
    let family = canonical(manifest.repo_family.as_deref().unwrap_or("jeryu-split"));
    let mut products: Vec<ProductConfig> = manifest
        .product
        .into_iter()
        .filter(|product| !product.id.trim().is_empty())
        .collect();
    // A repository may name its products instead, which is how one repository
    // declares that it is shared by several of them.
    for repo in manifest.repo {
        let Some(slug) = repo
            .jeryu_slug
            .or(repo.github_slug)
            .map(|slug| slug.to_ascii_lowercase())
        else {
            continue;
        };
        for id in repo.product {
            let id = id.trim().to_string();
            if id.is_empty() {
                continue;
            }
            match products.iter_mut().find(|product| product.id == id) {
                Some(product) => product.repos.push(slug.clone()),
                None => products.push(ProductConfig {
                    id,
                    repos: vec![slug.clone()],
                    ..ProductConfig::default()
                }),
            }
        }
    }
    if products.is_empty() {
        return None;
    }
    Some((family, products))
}

/// Parsed manifests and read pin locks, reused across requests so a page read
/// does not re-run a git command per repository.
///
/// What one cache entry holds: the parsed manifests, and the repository facts
/// read for one family scope, with the instant they were read.
#[derive(Default)]
struct Cached {
    config: Option<(Instant, Vec<FamilyConfig>)>,
    /// Keyed by the family scope the facts were read for; the whole forge is
    /// the empty key. Bounded so a forge with many families cannot grow it.
    facts: BTreeMap<String, (Instant, Vec<RepoFacts>)>,
}

/// How many family scopes' repository facts are retained.
const MAX_CACHED_SCOPES: usize = 8;

#[derive(Clone, Default)]
pub(crate) struct Cache(Arc<Mutex<Cached>>);

impl Cache {
    fn held(&self) -> std::sync::MutexGuard<'_, Cached> {
        self.0.lock().unwrap_or_else(|poisoned| {
            self.0.clear_poison();
            poisoned.into_inner()
        })
    }

    fn config(&self, paths: &[PathBuf]) -> Vec<FamilyConfig> {
        let mut held = self.held();
        if let Some((read, config)) = held.config.as_ref()
            && read.elapsed() < CONFIG_TTL
        {
            return config.clone();
        }
        let config = parse_manifests(paths);
        held.config = Some((Instant::now(), config.clone()));
        config
    }

    /// The repository facts read for one scope, reused for [`CONFIG_TTL`] so a
    /// page read does not re-run a git command per repository every time.
    fn facts(&self, scope: Option<&str>, read: impl FnOnce() -> Vec<RepoFacts>) -> Vec<RepoFacts> {
        let key = scope.unwrap_or_default().to_string();
        {
            let held = self.held();
            if let Some((at, facts)) = held.facts.get(&key)
                && at.elapsed() < CONFIG_TTL
            {
                return facts.clone();
            }
        }
        let facts = read();
        let mut held = self.held();
        held.facts.retain(|_, (at, _)| at.elapsed() < CONFIG_TTL);
        if held.facts.len() >= MAX_CACHED_SCOPES
            && let Some(oldest) = held
                .facts
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
        {
            held.facts.remove(&oldest);
        }
        held.facts.insert(key, (Instant::now(), facts.clone()));
        facts
    }
}

/// Read the `[[repo]]` names out of one pin lock.
fn lock_pins(text: &str, owner: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Lock {
        #[serde(default)]
        repo: Vec<LockRepo>,
    }
    #[derive(Deserialize)]
    struct LockRepo {
        name: Option<String>,
        jeryu_slug: Option<String>,
        github_slug: Option<String>,
    }
    let Ok(lock) = toml::from_str::<Lock>(text) else {
        return Vec::new();
    };
    lock.repo
        .into_iter()
        .filter_map(|repo| {
            // The lock's own slug when it has one; otherwise the name under the
            // owner the lock itself is hosted under.
            repo.jeryu_slug
                .or(repo
                    .github_slug
                    .filter(|slug| slug.split('/').next().is_some_and(|at| at == owner)))
                .or_else(|| repo.name.map(|name| format!("{owner}/{name}")))
        })
        .map(|slug| slug.to_ascii_lowercase())
        .collect()
}

/// A lock a product may name: a plain path inside the repository tree. A name
/// with a traversal segment, a leading dash or a root is not read at all.
pub(crate) fn lock_name_ok(lock: &str) -> bool {
    !lock.is_empty()
        && !lock.starts_with('-')
        && !lock.starts_with('/')
        && lock.len() <= 200
        && lock.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
}

/// The pin lock of a repository's default branch, read with git.
fn read_lock(state: &WebState, repo: &str, branch: &str, lock: &str) -> Result<Option<String>, ()> {
    if !lock_name_ok(lock) {
        return Err(());
    }
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(());
    };
    let Ok(resolved) = state.repo_manager.open_parts(owner, name) else {
        return Err(());
    };
    let output = Command::new(&state.repo_manager.config().git_bin)
        .arg("-C")
        .arg(resolved.path)
        .args(["show", "--end-of-options", &format!("{branch}:{lock}")])
        .output()
        .map_err(|_| ())?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    // git says "path does not exist" for a repository that holds no lock,
    // which is a configured fact. Anything else is unreadable.
    let message = String::from_utf8_lossy(&output.stderr);
    if message.contains("does not exist") || message.contains("exists on disk, but not in") {
        Ok(None)
    } else {
        Err(())
    }
}

/// Gather the configuration the topology is derived from.
pub(crate) fn inputs(state: &WebState, scope: Option<&str>) -> Inputs {
    let config = state.flow_topology.config(&state.split_manifests);
    let locks: BTreeSet<String> = config
        .iter()
        .flat_map(|family| family.products.iter())
        .filter_map(|product| product.pin_lock.clone())
        .chain([DEFAULT_PIN_LOCK.to_string()])
        .collect();
    let repos = state
        .flow_topology
        .clone()
        .facts(scope, || read_repo_facts(state, scope, &locks));
    let (release_evidence, pin_evidence) =
        state.events.flow_route_evidence(scope).unwrap_or_default();
    Inputs {
        scope: scope.map(str::to_string),
        config,
        repos,
        release_evidence,
        pin_evidence,
    }
}

/// Read every configured repository of the scope: its route and its pin lock.
fn read_repo_facts(
    state: &WebState,
    scope: Option<&str>,
    locks: &BTreeSet<String>,
) -> Vec<RepoFacts> {
    let mut repos = Vec::new();
    for repo in state.github.core().list_repositories(None) {
        let Some(family) = crate::web::repositories::effective_family(state, &repo) else {
            continue;
        };
        if scope.is_some_and(|scope| scope != family) {
            continue;
        }
        let slug = format!(
            "{}/{}",
            repo.owner.to_ascii_lowercase(),
            repo.name.to_ascii_lowercase()
        );
        let protection = state
            .core
            .get_branch_protection(&repo.owner, &repo.name, &repo.default_branch)
            .ok()
            .map(|rule| Protection {
                approvals: rule.required_approving_review_count,
                required_checks: rule.required_status_checks,
            });
        let mut pins: Option<Vec<String>> = None;
        let mut lock_unreadable = false;
        for lock in locks {
            match read_lock(state, &slug, &repo.default_branch, lock) {
                Ok(Some(text)) => {
                    let found = lock_pins(&text, &repo.owner.to_ascii_lowercase());
                    pins.get_or_insert_default().extend(found);
                }
                Ok(None) => {}
                Err(()) => lock_unreadable = true,
            }
        }
        repos.push(RepoFacts {
            repo: slug,
            family,
            default_branch: repo.default_branch.clone(),
            protection,
            pins,
            lock_unreadable,
        });
    }
    repos
}
