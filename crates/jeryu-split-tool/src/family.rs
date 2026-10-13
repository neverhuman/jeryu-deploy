//! The split-family authority manifest, read where its owner publishes it.
//!
//! `jeryu-release-ops` owns `repos.manifest.toml` and is the single authority
//! for the family's membership and release identity. Every other repository
//! reads that file instead of keeping a copy of it, so this module only
//! locates and parses it.
//!
//! The path comes from `JERYU_FAMILY_MANIFEST` when set, else from a control
//! plane checked out beside this repository
//! (`../jeryu-release-ops/repos.manifest.toml`, searched up the ancestor
//! chain). Nothing here names a host.
//!
//! Validation is limited to what the shape itself promises: the control plane
//! lists itself under `required_repos` and `[control_plane]` and has no
//! `[[repo]]` row, every other member is one `[[repo]]` row, and
//! `required_repos` must name exactly the active members. The authority's own
//! canonical-identity rules (which tag, which remote) stay with the authority.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// File name the authority publishes at the control plane's root.
pub const MANIFEST_FILE: &str = "repos.manifest.toml";
/// Directory name of the control-plane checkout that owns the manifest.
pub const CONTROL_PLANE_DIR: &str = "jeryu-release-ops";
/// Environment variable naming the authority manifest outright.
pub const MANIFEST_ENV: &str = "JERYU_FAMILY_MANIFEST";

/// The authority manifest's path, from the environment or a sibling checkout.
pub fn locate(start: &Path) -> Result<PathBuf> {
    if let Some(value) = env::var_os(MANIFEST_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let start = fs::canonicalize(start).with_context(|| {
        format!(
            "resolve {} to look for the family authority",
            start.display()
        )
    })?;
    for ancestor in start.ancestors() {
        let candidate = ancestor.join(CONTROL_PLANE_DIR).join(MANIFEST_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "no family authority manifest found: set {MANIFEST_ENV}, or check out \
         {CONTROL_PLANE_DIR} beside this repository"
    )
}

/// One repository the authority governs, control plane included.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Member {
    pub name: String,
    pub path: PathBuf,
    pub jeryu_slug: String,
    pub required_check: String,
    pub runtime_authority: String,
    /// The tag this member's release identity is bound to, if it is bound.
    pub tag: Option<String>,
}

/// A parsed, validated authority manifest.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Family {
    pub repo_family: String,
    pub split_root: PathBuf,
    #[serde(skip)]
    paths_resolved: bool,
    /// Active members in manifest order, the control plane first.
    pub members: Vec<Member>,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    schema_version: String,
    repo_family: String,
    #[serde(default)]
    split_root: Option<String>,
    required_repos: Vec<String>,
    control_plane: RawControlPlane,
    #[serde(default)]
    repo: Vec<RawRepository>,
}

#[derive(Debug, Deserialize)]
struct RawRepository {
    name: String,
    path: String,
    jeryu_slug: String,
    required_check: String,
    default_branch: String,
    identity_status: IdentityStatus,
    current_tag: Option<String>,
    inventory_status: String,
    runtime_authority: String,
}

#[derive(Debug, Deserialize)]
struct RawControlPlane {
    name: String,
    path: String,
    jeryu_slug: String,
    required_check: String,
    default_branch: String,
    identity_status: IdentityStatus,
    predecessor_tag: String,
    inventory_status: String,
    runtime_authority: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum IdentityStatus {
    Pending,
    Bound,
}

/// Parse and validate the authority manifest at `path`, resolving member
/// paths from the family root. The binary resolves paths through
/// [`read_with_root`]; this is the tests' shorthand.
#[cfg(test)]
pub fn read(path: &Path) -> Result<Family> {
    read_with_root(path, None)
}

/// Parse and validate the authority manifest at `path`. Member paths resolve
/// from one root: `site_root` when the operator supplies it (`--site-root`),
/// otherwise an absolute legacy `split_root`, otherwise the family root the
/// manifest's location implies (the manifest must then live in its named
/// control-plane directory, so a copy elsewhere cannot relocate the family).
/// Whichever root is used, every member path must stay inside it: the text is
/// checked by [`parse`] (no dot segments, no escape), and an existing path is
/// canonicalized and refused if it leaves the physical root.
pub fn read_with_root(path: &Path, site_root: Option<&Path>) -> Result<Family> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("read the family authority from {}", path.display()))?;
    let mut family =
        parse(&source).with_context(|| format!("family authority {}", path.display()))?;
    let manifest = fs::canonicalize(path)?;

    let root = if let Some(site_root) = site_root {
        fs::canonicalize(site_root)
            .with_context(|| format!("resolve the site root {}", site_root.display()))?
    } else if family.split_root.is_absolute() {
        fs::canonicalize(&family.split_root)?
    } else {
        let control = family
            .members
            .first()
            .context("manifest has no control plane")?;
        if manifest.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new(&control.name))
        {
            bail!("family manifest must live in its named control-plane directory");
        }
        manifest
            .parent()
            .and_then(Path::parent)
            .context("manifest has no family root")?
            .to_path_buf()
    };
    let legacy_root = family
        .split_root
        .is_absolute()
        .then(|| family.split_root.clone());
    family.split_root = root.clone();
    for member in &mut family.members {
        // A legacy absolute path is re-based onto the chosen root.
        if let Some(legacy) = legacy_root.as_ref()
            && let Ok(relative) = member.path.strip_prefix(legacy)
        {
            member.path = relative.to_path_buf();
        }
        member.path = root.join(&member.path);
        if let Ok(physical) = fs::canonicalize(&member.path) {
            if !physical.starts_with(&root) {
                bail!("{} path escapes the physical family root", member.name);
            }
            member.path = physical;
        }
    }
    family.paths_resolved = true;
    Ok(family)
}

/// Parse and validate authority-manifest text.
pub fn parse(source: &str) -> Result<Family> {
    let raw: RawManifest = toml::from_str(source).context("parse the family authority")?;
    if raw.schema_version != "1" {
        bail!("unsupported schema_version {}", raw.schema_version);
    }
    for (field, value) in [("repo_family", raw.repo_family.as_str())] {
        if value.trim().is_empty() {
            bail!("manifest missing {field}");
        }
    }

    let mut members = vec![control_plane_member(&raw.control_plane)?];
    for repo in &raw.repo {
        if repo.inventory_status.trim().is_empty() {
            bail!("{} missing inventory_status", repo.name);
        }
        if repo.inventory_status != "active" {
            continue;
        }
        members.push(repository_member(repo)?);
    }

    if raw
        .split_root
        .as_deref()
        .is_some_and(|root| root.trim().is_empty())
    {
        bail!("manifest split_root must not be blank");
    }
    let legacy_root = raw.split_root.as_deref().map(PathBuf::from);
    if legacy_root.as_ref().is_some_and(|root| !root.is_absolute()) {
        bail!("legacy split_root must be absolute");
    }
    for member in &members {
        let relative = if member.path.is_absolute() {
            let root = legacy_root
                .as_ref()
                .context("absolute member path requires legacy split_root")?;
            member
                .path
                .strip_prefix(root)
                .context("absolute member path escapes legacy split_root")?
        } else {
            member.path.as_path()
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            bail!(
                "{} path must stay inside the family root without dot segments",
                member.name
            );
        }
    }

    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut slugs = BTreeSet::new();
    for member in &members {
        if !names.insert(member.name.clone())
            || !paths.insert(member.path.clone())
            || !slugs.insert(member.jeryu_slug.clone())
        {
            bail!("duplicate member name, path, or slug: {}", member.name);
        }
    }
    let required: BTreeSet<String> = raw.required_repos.iter().cloned().collect();
    if required.len() != raw.required_repos.len() {
        bail!("required_repos lists a repository twice");
    }
    let missing: Vec<&str> = required
        .difference(&names)
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "required_repos names no active member: {}",
            missing.join(" ")
        );
    }
    let unlisted: Vec<&str> = names.difference(&required).map(String::as_str).collect();
    if !unlisted.is_empty() {
        bail!(
            "active members are missing from required_repos: {}",
            unlisted.join(" ")
        );
    }

    Ok(Family {
        repo_family: raw.repo_family,
        split_root: legacy_root.unwrap_or_else(|| PathBuf::from(".")),
        paths_resolved: false,
        members,
    })
}

fn control_plane_member(control: &RawControlPlane) -> Result<Member> {
    let tag = match (control.identity_status, control.predecessor_tag.trim()) {
        (IdentityStatus::Bound, tag) if !tag.is_empty() => Some(tag.to_owned()),
        (IdentityStatus::Bound, _) => bail!("{} is bound without a tag", control.name),
        (IdentityStatus::Pending, _) => {
            bail!("the control plane {} cannot be pending", control.name)
        }
    };
    if control.inventory_status != "active" {
        bail!("the control plane {} must stay active", control.name);
    }
    member(
        &control.name,
        &control.path,
        &control.jeryu_slug,
        &control.required_check,
        &control.default_branch,
        &control.runtime_authority,
        tag,
    )
}

fn repository_member(repo: &RawRepository) -> Result<Member> {
    let tag = match (repo.identity_status, repo.current_tag.as_deref()) {
        (IdentityStatus::Bound, Some(tag)) if !tag.trim().is_empty() => Some(tag.trim().to_owned()),
        (IdentityStatus::Bound, _) => bail!("{} is bound without a current_tag", repo.name),
        (IdentityStatus::Pending, None) => None,
        (IdentityStatus::Pending, Some(_)) => {
            bail!("{} is pending but carries a release tag", repo.name)
        }
    };
    member(
        &repo.name,
        &repo.path,
        &repo.jeryu_slug,
        &repo.required_check,
        &repo.default_branch,
        &repo.runtime_authority,
        tag,
    )
}

fn member(
    name: &str,
    path: &str,
    jeryu_slug: &str,
    required_check: &str,
    default_branch: &str,
    runtime_authority: &str,
    tag: Option<String>,
) -> Result<Member> {
    if name.trim().is_empty() {
        bail!("a member has no name");
    }
    for (field, value) in [
        ("path", path),
        ("jeryu_slug", jeryu_slug),
        ("required_check", required_check),
        ("runtime_authority", runtime_authority),
    ] {
        if value.trim().is_empty() {
            bail!("{name} missing {field}");
        }
    }
    if default_branch != "main" {
        bail!("{name} default_branch must be main");
    }
    Ok(Member {
        name: name.to_owned(),
        path: PathBuf::from(path),
        jeryu_slug: jeryu_slug.to_owned(),
        required_check: required_check.to_owned(),
        runtime_authority: runtime_authority.to_owned(),
        tag,
    })
}

impl Family {
    /// Prove every member is checked out where the authority says it is, with
    /// the agent maps a governed repository must carry.
    pub fn check_paths(&self) -> Result<()> {
        if !self.paths_resolved {
            bail!(
                "read the authority manifest from its path before checking physical member paths"
            );
        }
        for member in &self.members {
            if !member.path.is_dir() {
                bail!("{} path missing: {}", member.name, member.path.display());
            }
            // A member that was absent when the authority was read is contained
            // again now that it exists.
            let physical = fs::canonicalize(&member.path)?;
            if !physical.starts_with(&self.split_root) {
                bail!("{} path escapes the physical family root", member.name);
            }
            for required in ["AGENTS.md", "agent/owner-map.json", "agent/test-map.json"] {
                if !physical.join(required).is_file() {
                    bail!("{} missing {required}", member.name);
                }
            }
        }
        Ok(())
    }
}
