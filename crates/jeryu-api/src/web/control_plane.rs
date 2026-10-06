//! JMCP/control-plane intelligence read model.
//!
//! This module is deliberately pure aggregation over already-owned local
//! surfaces: [`ForgeCore`], the runner fleet snapshot, the live agent-run store,
//! and the auxiliary codegraph/tool-build store. GitHub mirror data is optional
//! read-only evidence and is represented here as explicit `missing` state until
//! a live mirror adapter supplies it. `docs/fleet-health-baseline.md` records
//! what each headline summary field measures — notably which `missing` states
//! are fixed defaults rather than observations.
//!
//! The implementation is split across focused submodules — request/response
//! [`types`], axum [`handlers`], the snapshot [`model`], the runner [`runner`]
//! fabric, [`priorities`] ranking, the repo [`graph`] builder and its [`depends`] edges, and the [`mcp`]
//! facade — while this file re-exports the same surface the rest of the web
//! edge already depends on.

mod checks;
mod depends;
mod gate_runners;
mod graph;
mod handlers;
pub(crate) mod live;
mod mcp;
mod model;
mod priorities;
mod runner;
mod types;

#[cfg(test)]
mod depends_tests;
#[cfg(test)]
mod tests;

const SCHEMA_VERSION: &str = "jeryu.control_plane/v1";
const RULES_VERSION: &str = "rules-v1";
const MIRROR_DOCS: &str = "docs/agent-native-standard.md";
const ARTIFACT_DOCS: &str = "scripts/release/README.md";
const FLEET_BASELINE_DOCS: &str = "docs/fleet-health-baseline.md";
/// How many repeated failure shapes the summary carries. Enough to tell "one
/// lane" from "many" without turning the summary into the check-run list.
const FAILING_CAUSE_LIMIT: usize = 5;
/// How many repository names each cause names before the count speaks for it.
const FAILING_CAUSE_REPO_LIMIT: usize = 5;

pub(super) use checks::*;
pub(super) use depends::*;
pub(super) use gate_runners::*;
pub(super) use graph::*;
pub(super) use handlers::*;
pub(super) use mcp::*;
pub(super) use model::*;
pub(super) use priorities::*;
pub(super) use runner::*;
pub(super) use types::*;
