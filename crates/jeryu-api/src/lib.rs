//! Typed API facade for Phase 10 endpoints plus the GitHub-compatible REST edge.

#[cfg(feature = "web")]
mod autonomy_bridge;
#[cfg(feature = "web")]
mod ci_bridge;
pub mod discovery;
#[cfg(feature = "web")]
mod git_materializer;
pub mod git_remote;
#[cfg(feature = "web")]
mod git_transport;
pub mod github;
#[cfg(feature = "web")]
pub mod github_mirror;
#[cfg(feature = "web")]
mod read_model;
pub mod routes;
#[cfg(all(test, feature = "web"))]
mod test_git;
#[cfg(feature = "web")]
pub mod web;

pub use github::{
    GithubRouter, JERYU_API_VERSION, JERYU_BUILD_COMMIT, JERYU_WEB_COMMIT, Method, V3_ROUTES,
};
pub use routes::{ApiState, Response, Router};
