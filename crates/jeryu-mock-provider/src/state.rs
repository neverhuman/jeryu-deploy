//! The mock provider's state machine (lane A mock-provider-v1). Pure logic:
//! the HTTP layer only parses requests and maps [`Refusal`] to status codes.
//! Token values never appear in counters, events, or `Debug` output.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Server flags; clients cannot change them.
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub access_ttl: Duration,
    pub refresh_ttl: Duration,
    pub device_ttl: Duration,
    pub rotate_refresh: bool,
    /// The next refresh persist fails once: nothing is minted and the old
    /// refresh token stays valid.
    pub fail_persist_once: bool,
    /// When set, every refresh rotation is written and fsynced here before
    /// any access token is returned.
    pub state_file: Option<PathBuf>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            access_ttl: Duration::from_secs(300),
            refresh_ttl: Duration::from_secs(3600),
            device_ttl: Duration::from_secs(600),
            rotate_refresh: true,
            fail_persist_once: false,
            state_file: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    ProviderUnknown,
    AuthorizationPending,
    ExpiredToken,
    InvalidRequest,
    ProviderNotConnected,
    ConnectionGenerationStale,
    RefreshPersistFailed,
    AccessExpired,
    Unauthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    RefreshPersisted,
    AccessLeaseIssued,
    GenerationBumped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Counters {
    pub connections_issued: u64,
    pub concurrent_sessions_peak: u64,
    pub refreshes_persisted: u64,
    pub access_leases_issued: u64,
    pub refresh_before_lease_ok: u64,
    pub stale_generation_refusals: u64,
    pub chat_authorized: u64,
    pub chat_rejected: u64,
    /// The mock has no HTTP client and no resolver; this stays 0.
    pub outbound_dial_attempts: u64,
}

pub struct DeviceGrant {
    pub device_code: String,
    pub user_code: String,
    pub expires_in: u64,
}

/// Tokens returned to the vault. Debug never shows the values.
pub struct TokenGrant {
    pub connection_id: String,
    pub connection_generation: u64,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

impl std::fmt::Debug for TokenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenGrant")
            .field("connection_id", &self.connection_id)
            .field("connection_generation", &self.connection_generation)
            .finish_non_exhaustive()
    }
}

pub struct SessionLease {
    pub session_id: String,
    pub access_token: String,
    pub expires_in: u64,
}

struct Device {
    user_code: String,
    approved: bool,
    expires: Instant,
    provider_account: String,
}

#[derive(PartialEq, Eq)]
enum ConnectionState {
    Connected,
    Disconnected,
}

struct Connection {
    state: ConnectionState,
    generation: u64,
    refresh_token: Option<String>,
    refresh_expires: Instant,
    /// When the last refresh minted access; sessions may lease until then.
    access_expires: Instant,
    sessions: usize,
}

struct AccessToken {
    connection_id: String,
    generation: u64,
    expires: Instant,
}

#[derive(Default)]
struct Inner {
    devices: HashMap<String, Device>,
    connections: HashMap<String, Arc<Mutex<Connection>>>,
    access: HashMap<String, AccessToken>,
    counters: Counters,
    events: Vec<Event>,
    fail_persist_once: bool,
}

/// The whole mock. One mutex per connection serializes refresh; the outer
/// mutex is held only briefly.
pub struct MockState {
    config: MockConfig,
    inner: Mutex<Inner>,
}

impl MockState {
    #[must_use]
    pub fn new(config: MockConfig) -> Self {
        let inner = Inner {
            fail_persist_once: config.fail_persist_once,
            ..Inner::default()
        };
        Self {
            config,
            inner: Mutex::new(inner),
        }
    }

    pub fn start_device(
        &self,
        provider: &str,
        provider_account: &str,
    ) -> Result<DeviceGrant, Refusal> {
        if !matches!(provider, "claude" | "codex" | "grok") {
            return Err(Refusal::ProviderUnknown);
        }
        let device_code = token("dev");
        let user_code = token("user")[..13].to_uppercase();
        self.inner.lock().devices.insert(
            device_code.clone(),
            Device {
                user_code: user_code.clone(),
                approved: false,
                expires: Instant::now() + self.config.device_ttl,
                provider_account: provider_account.to_string(),
            },
        );
        Ok(DeviceGrant {
            device_code,
            user_code,
            expires_in: self.config.device_ttl.as_secs(),
        })
    }

    pub fn approve(&self, user_code: &str) -> Result<(), Refusal> {
        let mut inner = self.inner.lock();
        let device = inner
            .devices
            .values_mut()
            .find(|device| device.user_code == user_code)
            .ok_or(Refusal::InvalidRequest)?;
        device.approved = true;
        Ok(())
    }

    /// Device-code grant: the first approved poll creates the connection at
    /// generation 1, persisting the refresh token before returning access.
    pub fn device_token(&self, device_code: &str) -> Result<TokenGrant, Refusal> {
        let mut inner = self.inner.lock();
        let device = inner
            .devices
            .get(device_code)
            .ok_or(Refusal::InvalidRequest)?;
        if device.expires <= Instant::now() {
            return Err(Refusal::ExpiredToken);
        }
        if !device.approved {
            return Err(Refusal::AuthorizationPending);
        }
        let _account = inner
            .devices
            .remove(device_code)
            .map(|d| d.provider_account);
        let connection_id = format!("mockconn-{}", uuid::Uuid::new_v4().simple());
        let refresh_token = token("mock-refresh");
        let now = Instant::now();
        let connection = Connection {
            state: ConnectionState::Connected,
            generation: 1,
            refresh_token: Some(refresh_token.clone()),
            refresh_expires: now + self.config.refresh_ttl,
            access_expires: now + self.config.access_ttl,
            sessions: 0,
        };
        self.persist(&connection_id, &refresh_token)
            .map_err(|()| Refusal::RefreshPersistFailed)?;
        inner.event(EventKind::RefreshPersisted);
        inner.counters.connections_issued += 1;
        inner
            .connections
            .insert(connection_id.clone(), Arc::new(Mutex::new(connection)));
        let access_token = inner.mint_access(&connection_id, 1, self.config.access_ttl);
        Ok(TokenGrant {
            connection_id,
            connection_generation: 1,
            access_token,
            refresh_token,
            expires_in: self.config.access_ttl.as_secs(),
        })
    }

    /// Refresh grant. Serialized per connection; the rotated refresh token and
    /// the new generation are durable before any access token exists.
    pub fn refresh(&self, connection_id: &str, presented: &str) -> Result<TokenGrant, Refusal> {
        let connection = self.connection(connection_id)?;
        let mut connection = connection.lock();
        if connection.state == ConnectionState::Disconnected {
            return Err(Refusal::ProviderNotConnected);
        }
        if connection.refresh_token.as_deref() != Some(presented) {
            return Err(Refusal::ConnectionGenerationStale);
        }
        if connection.refresh_expires <= Instant::now() {
            return Err(Refusal::ExpiredToken);
        }
        let rotated = if self.config.rotate_refresh {
            token("mock-refresh")
        } else {
            presented.to_string()
        };
        let fail = std::mem::take(&mut self.inner.lock().fail_persist_once);
        if fail || self.persist(connection_id, &rotated).is_err() {
            return Err(Refusal::RefreshPersistFailed);
        }
        connection.refresh_token = Some(rotated.clone());
        connection.generation += 1;
        connection.refresh_expires = Instant::now() + self.config.refresh_ttl;
        connection.access_expires = Instant::now() + self.config.access_ttl;
        let generation = connection.generation;
        let mut inner = self.inner.lock();
        inner.event(EventKind::RefreshPersisted);
        inner.event(EventKind::GenerationBumped);
        inner.counters.refreshes_persisted += 1;
        let access_token = inner.mint_access(connection_id, generation, self.config.access_ttl);
        inner.counters.refresh_before_lease_ok += 1;
        Ok(TokenGrant {
            connection_id: connection_id.to_string(),
            connection_generation: generation,
            access_token,
            refresh_token: rotated,
            expires_in: self.config.access_ttl.as_secs(),
        })
    }

    /// A new session's access lease, without another refresh, while the
    /// connection's access window is open.
    pub fn lease_session(
        &self,
        connection_id: &str,
        generation: u64,
    ) -> Result<SessionLease, Refusal> {
        let connection = self.connection(connection_id)?;
        let mut connection = connection.lock();
        if connection.state == ConnectionState::Disconnected {
            return Err(Refusal::ProviderNotConnected);
        }
        if connection.generation != generation {
            self.inner.lock().counters.stale_generation_refusals += 1;
            return Err(Refusal::ConnectionGenerationStale);
        }
        let left = connection
            .access_expires
            .saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refusal::AccessExpired);
        }
        connection.sessions += 1;
        let sessions = connection.sessions as u64;
        let mut inner = self.inner.lock();
        inner.counters.concurrent_sessions_peak =
            inner.counters.concurrent_sessions_peak.max(sessions);
        let access_token = inner.mint_access(connection_id, generation, left);
        Ok(SessionLease {
            session_id: format!("mocksess-{}", uuid::Uuid::new_v4().simple()),
            access_token,
            expires_in: left.as_secs(),
        })
    }

    /// Disconnect: bump the generation, drop the refresh token, end sessions.
    pub fn disconnect(&self, connection_id: &str) -> Result<u64, Refusal> {
        let connection = self.connection(connection_id)?;
        let mut connection = connection.lock();
        if connection.state == ConnectionState::Connected {
            connection.state = ConnectionState::Disconnected;
            connection.generation += 1;
            connection.refresh_token = None;
            connection.sessions = 0;
            self.inner.lock().event(EventKind::GenerationBumped);
        }
        Ok(connection.generation)
    }

    /// The chat endpoints: authorized only by a live, injected access token.
    pub fn authorize_chat(&self, authorization: Option<&str>) -> Result<(), Refusal> {
        let verdict = self.check_bearer(authorization);
        let mut inner = self.inner.lock();
        match verdict {
            Ok(()) => inner.counters.chat_authorized += 1,
            Err(_) => inner.counters.chat_rejected += 1,
        }
        verdict
    }

    fn check_bearer(&self, authorization: Option<&str>) -> Result<(), Refusal> {
        let bearer = authorization
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Refusal::Unauthorized)?;
        let (connection_id, generation, expires) = {
            let inner = self.inner.lock();
            let access = inner.access.get(bearer).ok_or(Refusal::Unauthorized)?;
            (
                access.connection_id.clone(),
                access.generation,
                access.expires,
            )
        };
        if expires <= Instant::now() {
            return Err(Refusal::Unauthorized);
        }
        let connection = self.connection(&connection_id)?;
        let connection = connection.lock();
        if connection.state != ConnectionState::Connected || connection.generation != generation {
            return Err(Refusal::Unauthorized);
        }
        Ok(())
    }

    #[must_use]
    pub fn counters(&self) -> Counters {
        self.inner.lock().counters.clone()
    }

    #[must_use]
    pub fn events(&self) -> Vec<Event> {
        self.inner.lock().events.clone()
    }

    fn connection(&self, id: &str) -> Result<Arc<Mutex<Connection>>, Refusal> {
        self.inner
            .lock()
            .connections
            .get(id)
            .cloned()
            .ok_or(Refusal::ProviderNotConnected)
    }

    /// Write and fsync the current refresh token of one connection.
    fn persist(&self, connection_id: &str, refresh_token: &str) -> Result<(), ()> {
        let Some(path) = &self.config.state_file else {
            return Ok(());
        };
        let line = format!("{connection_id} {refresh_token}\n");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|_| ())?;
        file.write_all(line.as_bytes()).map_err(|_| ())?;
        file.sync_all().map_err(|_| ())
    }
}

impl Inner {
    fn event(&mut self, kind: EventKind) {
        let seq = self.events.len() as u64 + 1;
        self.events.push(Event { seq, kind });
    }

    fn mint_access(&mut self, connection_id: &str, generation: u64, ttl: Duration) -> String {
        let access_token = token("mock-access");
        self.access.insert(
            access_token.clone(),
            AccessToken {
                connection_id: connection_id.to_string(),
                generation,
                expires: Instant::now() + ttl,
            },
        );
        self.counters.access_leases_issued += 1;
        self.event(EventKind::AccessLeaseIssued);
        access_token
    }
}

fn token(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}
