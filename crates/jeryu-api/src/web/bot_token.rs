//! HMAC-SHA256 bot access tokens. The forge is the only signer and verifier.
//!
//! Enrollment keys (`jbk_`) and refresh tokens (`jbr_`) are never accepted as
//! bearers. Tests sign with a fixed key so `WebState`'s constructor stays
//! untouched; production reads a mode-0600 file installed from `serve()`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::http::{HeaderMap, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use jeryu_core::{AccountSummary, BotSession, UserRole};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use super::{WebState, api_error};

const ACCESS_TTL_SECS: i64 = 900;
const CLOCK_SKEW_SECS: i64 = 60;
type HmacSha256 = Hmac<Sha256>;

static SIGNING_KEY_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct BotPrincipal {
    pub(crate) bot_id: Uuid,
    pub(crate) generation: u64,
    pub(crate) key_id: String,
}

/// Marks a request that arrived as `/mcp/bot` before the router rewrite.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BotMcpMount;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BearerClass {
    /// Payload says `token_use=bot_access`. A bad signature is still this class.
    BotAccess,
    /// Enrollment key or refresh token. Never a personal access token.
    Unusable,
    /// A scoped agent token (`jat_`), presented directly as a bearer.
    AgentToken,
    /// Anything else, including a personal access token that happens to contain dots.
    Other,
}

struct AccessClaims {
    bot_id: Uuid,
    generation: u64,
    key_id: String,
}

pub(super) fn invalid_credential() -> axum::response::Response {
    api_error(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "invalid bot credential",
    )
}

pub(super) fn classify_bearer(token: &str) -> BearerClass {
    if token.starts_with("jbk_") || token.starts_with("jbr_") {
        return BearerClass::Unusable;
    }
    if token.starts_with(jeryu_core::AGENT_TOKEN_PREFIX) {
        return BearerClass::AgentToken;
    }
    if is_access_jwt(token) && payload_says_bot_access(token) {
        return BearerClass::BotAccess;
    }
    BearerClass::Other
}

pub(super) fn is_access_jwt(token: &str) -> bool {
    token.starts_with("eyJ") && token.bytes().filter(|byte| *byte == b'.').count() == 2
}

/// Create or load the production signing key. Tests keep the fixed key and
/// do not touch the data directory.
pub(super) fn install_signing_key(data_dir: &Path) -> Result<(), String> {
    let path = std::env::var_os("JERYU_BOT_SIGNING_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("bot-signing.key"));
    let _ = SIGNING_KEY_PATH.set(path.clone());
    if cfg!(test) {
        return Ok(());
    }
    production_signing_key(&path).map(|_| ())
}

pub(super) fn issue_access_token(session: &BotSession, audience: &str) -> Result<String, String> {
    let Some(audience) = canonical_audience(audience) else {
        return Err("audience is not an acceptable forge origin".to_string());
    };
    let key = signing_key()?;
    let now = Utc::now().timestamp();
    let kind = session.bot.kind.as_str();
    let payload = json!({
        "iss": audience,
        "aud": audience,
        "sub": session.bot.id.to_string(),
        "token_use": "bot_access",
        "generation": session.generation,
        "key_id": session.key_id,
        "iat": now,
        "nbf": now,
        "exp": now + ACCESS_TTL_SECS,
        "kind": kind,
    });
    sign(&key, &payload)
}

pub(super) fn authenticate_bearer(
    state: &WebState,
    headers: &HeaderMap,
    token: &str,
) -> Option<(AccountSummary, BotPrincipal)> {
    let expected = request_audience(headers)?;
    let claims = verify_access_token(token, &expected).ok()?;
    let summary = state
        .core
        .authenticate_bot_access(claims.bot_id, claims.generation, &claims.key_id)
        .ok()?;
    let mut account = state.core.get_account(&summary.owner).ok()?;
    // Stored admin must not widen repository checks. Callers that still consult
    // `account.role` see a normal user; bot reach is `authorize_bot`'s job.
    account.role = UserRole::User;
    Some((
        account,
        BotPrincipal {
            bot_id: claims.bot_id,
            generation: claims.generation,
            key_id: claims.key_id,
        },
    ))
}

/// Resolve a scoped agent token to its bot. Like a bot access token, the
/// account is presented as a normal user: bot reach is `authorize_bot`'s job.
pub(super) fn authenticate_agent_token(
    state: &WebState,
    token: &str,
) -> Option<(AccountSummary, BotPrincipal)> {
    let auth = state.core.authenticate_bot_access_token(token).ok()?;
    let mut account = state.core.get_account(&auth.bot.owner).ok()?;
    account.role = UserRole::User;
    Some((
        account,
        BotPrincipal {
            bot_id: auth.bot.id,
            generation: auth.generation,
            key_id: auth.key_id,
        },
    ))
}

/// Origin the access token must name. Loopback hosts use `http` and every
/// other host uses `https`. The port is not part of the audience, so a token
/// minted for `http://127.0.0.1` matches `Host: 127.0.0.1:8787`.
pub(super) fn request_audience(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    let (name, _) = split_host_port(host)?;
    let loopback = matches!(name, "127.0.0.1" | "localhost" | "::1");
    let scheme = if loopback { "http" } else { "https" };
    let shown = if name == "::1" {
        "[::1]".to_string()
    } else {
        name.to_string()
    };
    Some(format!("{scheme}://{shown}"))
}

/// Base URL the setup script calls, including a non-default port from `Host`.
pub(super) fn request_base_url(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return None;
    }
    let (name, _) = split_host_port(host)?;
    let loopback = matches!(name, "127.0.0.1" | "localhost" | "::1");
    let scheme = if loopback { "http" } else { "https" };
    Some(format!("{scheme}://{host}"))
}

pub(super) fn normalize_audience(audience: &str) -> String {
    let trimmed = audience.trim();
    trimmed.strip_suffix('/').unwrap_or(trimmed).to_string()
}

/// Accept `https` origins and loopback `http`, then drop the port so the
/// stored audience matches [`request_audience`].
pub(super) fn canonical_audience(audience: &str) -> Option<String> {
    let audience = normalize_audience(audience);
    if !acceptable_audience(&audience) {
        return None;
    }
    let rest = audience
        .strip_prefix("https://")
        .or_else(|| audience.strip_prefix("http://"))?;
    let (host, _) = split_host_port(rest)?;
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "::1");
    let scheme = if loopback { "http" } else { "https" };
    let shown = if host == "::1" {
        "[::1]".to_string()
    } else {
        host.to_string()
    };
    Some(format!("{scheme}://{shown}"))
}

pub(super) fn acceptable_audience(audience: &str) -> bool {
    let audience = normalize_audience(audience);
    if audience.len() > 200
        || audience
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '?' | '#' | '\\' | '@'))
    {
        return false;
    }
    let (scheme, rest) = if let Some(rest) = audience.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = audience.strip_prefix("http://") {
        ("http", rest)
    } else {
        return false;
    };
    if rest.is_empty() || rest.contains('/') {
        return false;
    }
    let Some((host, _)) = split_host_port(rest) else {
        return false;
    };
    if scheme == "https" {
        return !host.is_empty();
    }
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        if after.is_empty() {
            return Some((host, None));
        }
        let port = after.strip_prefix(':')?;
        if port.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
        return Some((host, Some(port)));
    }
    if authority.matches(':').count() > 1 {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port))
            if !host.is_empty()
                && !port.is_empty()
                && port.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            Some((host, Some(port)))
        }
        Some(_) => None,
        None if !authority.is_empty() => Some((authority, None)),
        None => None,
    }
}

fn signing_key() -> Result<[u8; 32], String> {
    if cfg!(test) {
        return Ok(test_key());
    }
    let path = SIGNING_KEY_PATH
        .get()
        .ok_or_else(|| "bot signing key is not installed".to_string())?;
    production_signing_key(path)
}

fn test_key() -> [u8; 32] {
    *b"jeryu-bot-test-key-32-bytes!!!!!"
}

/// Load or create the production signing key. Refuses a group- or world-readable
/// file, including a symlink.
fn production_signing_key(path: &Path) -> Result<[u8; 32], String> {
    if path.exists() || std::fs::symlink_metadata(path).is_ok() {
        ensure_private(path)?;
        let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
        return key_bytes(&bytes);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut bytes = [0u8; 32];
    let mut urandom = std::fs::File::open("/dev/urandom").map_err(|err| err.to_string())?;
    std::io::Read::read_exact(&mut urandom, &mut bytes).map_err(|err| err.to_string())?;
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|err| err.to_string())?;
        file.write_all(&bytes).map_err(|err| err.to_string())?;
        file.sync_all().map_err(|err| err.to_string())?;
    }
    if ensure_private(path).is_err() {
        let _ = std::fs::remove_file(path);
        return Err("bot signing key must not be group- or world-readable".to_string());
    }
    Ok(bytes)
}

fn ensure_private(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|err| err.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("bot signing key must not be a symlink".to_string());
    }
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("bot signing key must not be group- or world-readable".to_string());
    }
    Ok(())
}

fn key_bytes(bytes: &[u8]) -> Result<[u8; 32], String> {
    if bytes.len() != 32 {
        return Err("bot signing key must be 32 bytes".to_string());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Ok(key)
}

fn sign(key: &[u8; 32], payload: &Value) -> Result<String, String> {
    let header = b64(&json!({ "alg": "HS256", "typ": "JWT" })
        .to_string()
        .into_bytes());
    let body = b64(&payload.to_string().into_bytes());
    let signing_input = format!("{header}.{body}");
    let mut mac = HmacSha256::new_from_slice(key).map_err(|err| err.to_string())?;
    mac.update(signing_input.as_bytes());
    let signature = b64(&mac.finalize().into_bytes());
    Ok(format!("{signing_input}.{signature}"))
}

fn verify_access_token(token: &str, expected_aud: &str) -> Result<AccessClaims, ()> {
    let key = signing_key().map_err(|_| ())?;
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(());
    };
    let header: Value = serde_json::from_slice(&b64_decode(header_b64)?).map_err(|_| ())?;
    if header.get("alg").and_then(Value::as_str) != Some("HS256") {
        return Err(());
    }
    let signing_input = format!("{header_b64}.{payload_b64}");
    let signature = b64_decode(signature_b64)?;
    let mut mac = HmacSha256::new_from_slice(&key).map_err(|_| ())?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&signature).map_err(|_| ())?;
    let payload: Value = serde_json::from_slice(&b64_decode(payload_b64)?).map_err(|_| ())?;
    if payload.get("token_use").and_then(Value::as_str) != Some("bot_access") {
        return Err(());
    }
    let aud = payload
        .get("aud")
        .and_then(Value::as_str)
        .and_then(canonical_audience)
        .ok_or(())?;
    let iss = payload
        .get("iss")
        .and_then(Value::as_str)
        .and_then(canonical_audience)
        .ok_or(())?;
    if aud != expected_aud || iss != aud {
        return Err(());
    }
    let iat = payload.get("iat").and_then(Value::as_i64).ok_or(())?;
    let nbf = payload.get("nbf").and_then(Value::as_i64).ok_or(())?;
    let exp = payload.get("exp").and_then(Value::as_i64).ok_or(())?;
    if exp != iat.checked_add(ACCESS_TTL_SECS).ok_or(())? || nbf != iat {
        return Err(());
    }
    let now = Utc::now().timestamp();
    if iat > now.saturating_add(CLOCK_SKEW_SECS)
        || nbf > now.saturating_add(CLOCK_SKEW_SECS)
        || now > exp.saturating_add(CLOCK_SKEW_SECS)
    {
        return Err(());
    }
    let bot_id = payload
        .get("sub")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(())?;
    let generation = payload
        .get("generation")
        .and_then(Value::as_u64)
        .ok_or(())?;
    let key_id = payload
        .get("key_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(())?
        .to_string();
    Ok(AccessClaims {
        bot_id,
        generation,
        key_id,
    })
}

fn payload_says_bot_access(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(_header), Some(payload_b64), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(bytes) = b64_decode(payload_b64) else {
        return false;
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&bytes) else {
        return false;
    };
    payload.get("token_use").and_then(Value::as_str) == Some("bot_access")
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn b64_decode(text: &str) -> Result<Vec<u8>, ()> {
    URL_SAFE_NO_PAD.decode(text).map_err(|_| ())
}

/// Rewrite `/mcp/bot` onto `/mcp` and mark the request so the bot mount can
/// hide forge-admin tools. The intelligence router only binds `/mcp`.
pub(super) fn rewrite_bot_mcp(mut request: axum::extract::Request) -> axum::extract::Request {
    let path = request.uri().path().to_string();
    let Some(rest) = path.strip_prefix("/mcp/bot") else {
        return request;
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return request;
    }
    let new_path = if rest.is_empty() {
        "/mcp".to_string()
    } else {
        format!("/mcp{rest}")
    };
    let rewritten = match request.uri().query() {
        Some(query) => format!("{new_path}?{query}"),
        None => new_path,
    };
    if let Ok(uri) = rewritten.parse() {
        *request.uri_mut() = uri;
        request.extensions_mut().insert(BotMcpMount);
    }
    request
}

#[cfg(test)]
mod signing_key_file_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::production_signing_key;

    #[test]
    fn signing_key_file_is_created_at_mode_0600_and_a_wider_mode_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bot-signing.key");
        let created = production_signing_key(&path).expect("create");
        assert_eq!(created.len(), 32);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "created mode is {mode:o}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = production_signing_key(&path).expect_err("wider key");
        assert_eq!(err, "bot signing key must not be group- or world-readable");
        let left = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(left, 0o644, "a wider key is refused and left unchanged");
    }
}
