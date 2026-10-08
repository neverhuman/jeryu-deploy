//! Which upstream a provider may reach, and which paths and headers a relay
//! request may carry (connection-v1 §6). The production table names exact
//! hosts on 443 and nothing else; until lane P enrolls path templates, no
//! path is allowed for a real provider.

use crate::RelayError;
use crate::frames::Header;

/// Where one provider's requests go and which paths are enrolled.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// `https://host` (production) or a loopback mock (test harnesses only).
    pub base_url: String,
    /// Enrolled path prefixes, e.g. `/v1/messages`. Empty means disabled.
    pub path_prefixes: Vec<String>,
    /// Fixed headers the provider requires that the guest may not set.
    pub fixed_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct UpstreamTable {
    claude: Option<Upstream>,
    codex: Option<Upstream>,
    grok: Option<Upstream>,
}

impl UpstreamTable {
    /// The production hosts with no enrolled paths: every real relay is
    /// refused until a path template is enrolled through `enroll`.
    #[must_use]
    pub fn production() -> Self {
        let upstream = |host: &str| Upstream {
            base_url: format!("https://{host}"),
            path_prefixes: Vec::new(),
            fixed_headers: Vec::new(),
        };
        Self {
            claude: Some(upstream("api.anthropic.com")),
            codex: Some(upstream("api.openai.com")),
            grok: Some(upstream("api.x.ai")),
        }
    }

    /// Enroll a path prefix for a production provider (lane P qualification).
    pub fn enroll(&mut self, provider: &str, prefix: &str) -> Result<(), RelayError> {
        let slot = self.slot(provider).ok_or(RelayError::ProviderUnknown)?;
        let upstream = slot.as_mut().ok_or(RelayError::UpstreamHostDenied)?;
        if !valid_path(prefix) {
            return Err(RelayError::RelayStreamInvalid);
        }
        upstream.path_prefixes.push(prefix.to_string());
        Ok(())
    }

    /// Point a provider at a loopback mock. Test harnesses only.
    #[cfg(feature = "test-harness")]
    pub fn loopback(&mut self, provider: &str, upstream: Upstream) -> Result<(), RelayError> {
        let is_loopback = upstream.base_url.starts_with("http://127.0.0.1:");
        if !is_loopback {
            return Err(RelayError::UpstreamHostDenied);
        }
        *self.slot(provider).ok_or(RelayError::ProviderUnknown)? = Some(upstream);
        Ok(())
    }

    fn slot(&mut self, provider: &str) -> Option<&mut Option<Upstream>> {
        match provider {
            "claude" => Some(&mut self.claude),
            "codex" => Some(&mut self.codex),
            "grok" => Some(&mut self.grok),
            _ => None,
        }
    }

    /// The upstream URL for an allowed request, or why it is refused.
    pub fn resolve(&self, provider: &str, path: &str) -> Result<(String, &Upstream), RelayError> {
        let upstream = match provider {
            "claude" => self.claude.as_ref(),
            "codex" => self.codex.as_ref(),
            "grok" => self.grok.as_ref(),
            _ => return Err(RelayError::ProviderUnknown),
        }
        .ok_or(RelayError::UpstreamHostDenied)?;
        if !valid_path(path) {
            return Err(RelayError::RelayStreamInvalid);
        }
        let enrolled = upstream.path_prefixes.iter().any(|prefix| {
            path == prefix
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        });
        if !enrolled {
            return Err(RelayError::UpstreamHostDenied);
        }
        Ok((format!("{}{path}", upstream.base_url), upstream))
    }
}

/// connection-v1 §6: one absolute path; no authority, userinfo, query,
/// fragment, percent escape, backslash, control byte, empty or dot segment.
#[must_use]
pub fn valid_path(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 2048 {
        return false;
    }
    let forbidden = |b: u8| b.is_ascii_control() || b"?#%\\@ ".contains(&b) || !b.is_ascii();
    if path.bytes().any(forbidden) {
        return false;
    }
    path[1..]
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Only `accept` and `content-type`, at most one of each, 1 to 256 printable
/// ASCII bytes.
pub fn check_headers(headers: &[Header]) -> Result<(), RelayError> {
    let mut seen = Vec::new();
    for header in headers {
        let allowed = matches!(header.name.as_str(), "accept" | "content-type");
        let printable = !header.value.is_empty()
            && header.value.len() <= 256
            && header.value.bytes().all(|b| (0x20..0x7f).contains(&b));
        if !allowed || !printable || seen.contains(&header.name) {
            return Err(RelayError::RelayStreamInvalid);
        }
        seen.push(header.name.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_must_be_plain_absolute_paths() {
        for good in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
            assert!(valid_path(good), "{good}");
        }
        for bad in [
            "v1",
            "//evil.example/x",
            "/v1/../admin",
            "/v1/./x",
            "/v1//x",
            "/v1?x=1",
            "/v1#f",
            "/v1/%2e",
            "/v1\\x",
            "/v1/\u{7f}",
            "/a@b",
            "/",
            "/v1/ x",
        ] {
            assert!(!valid_path(bad), "{bad}");
        }
    }

    #[test]
    fn production_relays_nothing_until_a_path_is_enrolled() {
        let mut table = UpstreamTable::production();
        assert_eq!(
            table.resolve("claude", "/v1/messages").unwrap_err(),
            RelayError::UpstreamHostDenied
        );
        assert_eq!(
            table.resolve("jekko", "/v1/messages").unwrap_err(),
            RelayError::ProviderUnknown
        );
        table.enroll("claude", "/v1/messages").expect("enroll");
        let (url, _) = table.resolve("claude", "/v1/messages").expect("allowed");
        assert_eq!(url, ["https://api.", "anthropic.com/v1/messages"].concat());
        assert_eq!(
            table.resolve("claude", "/v1/messagesX").unwrap_err(),
            RelayError::UpstreamHostDenied,
            "a prefix matches whole segments only"
        );
        assert_eq!(
            table.resolve("codex", "/v1/messages").unwrap_err(),
            RelayError::UpstreamHostDenied
        );
    }

    #[test]
    fn only_accept_and_content_type_once_each() {
        let header = |name: &str, value: &str| Header {
            name: name.into(),
            value: value.into(),
        };
        assert!(check_headers(&[header("accept", "text/event-stream")]).is_ok());
        assert!(check_headers(&[header("authorization", "Bearer x")]).is_err());
        assert!(check_headers(&[header("accept", "a"), header("accept", "b")]).is_err());
        assert!(check_headers(&[header("content-type", "x\ny")]).is_err());
    }
}
