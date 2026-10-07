//! Parsing and allowlisting a git remote before it reaches `git`.
//!
//! A remote is a network destination and a command-line argument at once, so
//! "it starts with https://" says too little: `git` reads a leading dash as an
//! option, a host that merely begins with an allowed one is a different host,
//! and a path walking up through `..` names a different repository. Every
//! remote this process reads from or writes to is parsed here and then matched,
//! whole, against the host and first path segment the site allows.
//!
//! The allowlist is site configuration, so this module carries no default for
//! it: nothing configured refuses every remote.

use std::collections::BTreeSet;

/// Host plus first path segment pairs a remote may name, e.g.
/// `github.com/examplecorp`.
#[derive(Clone, Debug, Default)]
pub struct GitRemoteAllowlist {
    entries: BTreeSet<String>,
}

impl GitRemoteAllowlist {
    /// Parse `host/segment` entries separated by whitespace or commas. Hosts and
    /// segments are compared lowercased, as DNS and the allowlist are both
    /// case-insensitive here.
    pub fn parse(raw: &str) -> Self {
        let entries = raw
            .split(|ch: char| ch.is_whitespace() || ch == ',')
            .filter(|entry| !entry.is_empty())
            .map(|entry| entry.trim_end_matches('/').to_ascii_lowercase())
            .collect();
        Self { entries }
    }

    /// Read the allowlist from `var`. `None` when the variable is unset or holds
    /// no entry, which every caller treats as "no remote may be reached".
    pub fn from_env(var: &str) -> Option<Self> {
        let raw = std::env::var(var).ok()?;
        let parsed = Self::parse(&raw);
        if parsed.entries.is_empty() {
            None
        } else {
            Some(parsed)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `Ok(())` when `url` may be handed to a `git` command that reaches a
    /// remote; otherwise one sentence naming the rule it broke, fit to record in
    /// an outcome a person reads.
    pub fn check(&self, url: &str) -> Result<(), String> {
        let refuse = |why: &str| Err(format!("refusing git remote {url}: {why}"));

        if url.is_empty() {
            return refuse("it is empty");
        }
        if url.starts_with('-') {
            return refuse("it starts with a dash, which git reads as an option, not a remote");
        }
        if url.chars().any(|ch| ch.is_control()) {
            return refuse("it holds a control character (newline, carriage return, tab or NUL)");
        }
        let Some(rest) = url.strip_prefix("https://") else {
            return refuse(
                "only https remotes are reached, so neither ssh nor http nor a local scheme is one",
            );
        };
        let Some((host, path)) = rest.split_once('/') else {
            return refuse("it names no repository path under its host");
        };
        if host.contains('@') {
            return refuse(
                "it carries userinfo before the host, which hides which host is reached",
            );
        }
        if host.contains(':') {
            return refuse("it names an explicit port, and only the default https port is reached");
        }
        if !is_plain_host(host) {
            return refuse("its host is not a plain hostname");
        }
        if path.contains('%') {
            return refuse("its path is percent-encoded, which can spell a dot segment git walks");
        }
        let mut segments = path.split('/');
        let first = segments.next().unwrap_or_default();
        for segment in std::iter::once(first).chain(segments) {
            if segment.is_empty() {
                return refuse("its path holds an empty segment");
            }
            if segment == "." || segment == ".." {
                return refuse(
                    "its path holds a dot segment, so it does not name the repository it reads as",
                );
            }
            if !is_path_segment(segment) {
                return refuse(&format!(
                    "its path segment {segment} is not a repository path component"
                ));
            }
        }
        if self.entries.is_empty() {
            return refuse("no remote allowlist is configured, so no remote may be reached");
        }
        let entry = format!(
            "{}/{}",
            host.to_ascii_lowercase(),
            first.to_ascii_lowercase()
        );
        if !self.entries.contains(&entry) {
            return refuse(&format!("{entry} is not an allowlisted host and owner"));
        }
        Ok(())
    }
}

fn is_plain_host(host: &str) -> bool {
    let bytes = host.as_bytes();
    let edges_are_alphanumeric = matches!(bytes.first(), Some(b) if b.is_ascii_alphanumeric())
        && matches!(bytes.last(), Some(b) if b.is_ascii_alphanumeric());
    edges_are_alphanumeric
        && host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
}

fn is_path_segment(segment: &str) -> bool {
    matches!(segment.as_bytes().first(), Some(b) if b.is_ascii_alphanumeric())
        && segment
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every host, owner and path below is invented.
    fn allowlist() -> GitRemoteAllowlist {
        GitRemoteAllowlist::parse("github.com/examplecorp, forge.example.net/examplecorp-ops")
    }

    fn why(url: &str) -> String {
        allowlist()
            .check(url)
            .expect_err("remote should be refused")
    }

    #[test]
    fn allowlisted_host_and_owner_pass() {
        assert!(
            allowlist()
                .check("https://github.com/examplecorp/app.git")
                .is_ok()
        );
        assert!(
            allowlist()
                .check("https://forge.example.net/examplecorp-ops/app.git")
                .is_ok()
        );
        // Host and owner are matched case-insensitively.
        assert!(
            allowlist()
                .check("https://GitHub.com/ExampleCorp/App.git")
                .is_ok()
        );
    }

    #[test]
    fn empty_remote_is_refused() {
        assert!(why("").contains("empty"));
    }

    #[test]
    fn leading_dash_is_refused() {
        assert!(why("--upload-pack=touch /tmp/pwned").contains("dash"));
        assert!(
            why("-c protocol.ext.allow=always https://github.com/examplecorp/app.git")
                .contains("dash")
        );
    }

    #[test]
    fn control_characters_are_refused() {
        for url in [
            "https://github.com/examplecorp/app.git\nhttps://evil.example.net/x/y.git",
            "https://github.com/examplecorp/app.git\r",
            "https://github.com/examplecorp/app.git\t",
            "https://github.com/examplecorp/app.git\0",
        ] {
            assert!(why(url).contains("control character"), "{url:?}");
        }
    }

    #[test]
    fn non_https_schemes_are_refused() {
        for url in [
            "ssh://git@github.com/examplecorp/app.git",
            "git@github.com:examplecorp/app.git",
            "http://github.com/examplecorp/app.git",
            "file:///srv/git/examplecorp/app.git",
            "ext::sh -c touch% /tmp/pwned",
            // Case is not a way past the scheme check either.
            "HTTPS://github.com/examplecorp/app.git",
        ] {
            assert!(why(url).contains("only https"), "{url:?}");
        }
    }

    #[test]
    fn userinfo_is_refused() {
        assert!(why("https://github.com@evil.example.net/x/y.git").contains("userinfo"));
        assert!(
            why("https://x-access-token:secret@github.com/examplecorp/app.git")
                .contains("userinfo")
        );
    }

    #[test]
    fn explicit_port_is_refused() {
        assert!(why("https://github.com:8443/examplecorp/app.git").contains("explicit port"));
    }

    #[test]
    fn a_host_is_matched_whole() {
        assert!(
            why("https://github.com.evil.example.net/examplecorp/app.git")
                .contains("not an allowlisted")
        );
        assert!(why("https://evil-github.com/examplecorp/app.git").contains("not an allowlisted"));
    }

    #[test]
    fn an_owner_is_matched_whole() {
        assert!(
            why("https://github.com/examplecorp-staging/app.git").contains("not an allowlisted")
        );
        assert!(why("https://github.com/other/app.git").contains("not an allowlisted"));
    }

    #[test]
    fn dot_segments_are_refused() {
        assert!(why("https://github.com/examplecorp/../evil/app.git").contains("dot segment"));
        assert!(why("https://github.com/examplecorp/./app.git").contains("dot segment"));
    }

    #[test]
    fn percent_encoded_path_is_refused() {
        assert!(
            why("https://github.com/examplecorp/%2e%2e/evil/app.git").contains("percent-encoded")
        );
        assert!(why("https://github.com/examplecorp/app.git%00").contains("percent-encoded"));
    }

    #[test]
    fn a_path_is_required() {
        assert!(why("https://github.com").contains("no repository path"));
        assert!(why("https://github.com/examplecorp/").contains("empty segment"));
    }

    #[test]
    fn an_unconfigured_allowlist_refuses_everything() {
        let empty = GitRemoteAllowlist::parse("  ,  ");
        assert!(empty.is_empty());
        let why = empty
            .check("https://github.com/examplecorp/app.git")
            .expect_err("an unconfigured allowlist refuses");
        assert!(why.contains("no remote allowlist is configured"), "{why}");
    }
}
