//! Public Firecracker refusal, with separately named metadata and legacy components.
//! Fixtures record explicit state and Git data; none stand in for a launch.
use super::{
    AgentRunIntent, AgentRunState, JobRequest, NetworkPolicy, RefService, RunnerClass, SandboxPlan,
    SecretPolicy, SessionRecordInit, TokenPolicy, TrustTier, WebState, select_runner,
};
use axum::{
    body::Bytes,
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    response::Response as AxumResponse,
};
use fixtures::*;
use jeryu_core::{CreateRepositoryRequest, ForgeCore};
use jeryu_runner_oci::{FakeContainerRuntime, LifecycleOp};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
mod admission;
mod auth;
mod fixtures;
mod metadata;
mod publication;
mod runtime;
fn every_row() -> Query<crate::web::paging::PageParams> {
    Query(crate::web::paging::PageParams::default())
}
