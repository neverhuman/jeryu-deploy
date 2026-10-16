//! The one canonical URL policy: where a repository and a pull request are
//! read by a person, and where the same object is read by a program.
//!
//! A browser URL that a person or an agent can retype has to be short enough
//! to remember and unambiguous enough to be right. The forge's own
//! repositories are addressed by `owner/name` everywhere else — in a clone
//! URL, in `full_name`, in a todo — so their browser URL says the same thing:
//!
//!   * repository: `/{owner}/{name}`
//!   * pull request: `/{owner}/{name}/pull/{number}`
//!
//! A repository the forge only mirrors from another host keeps the
//! provider-qualified shape, `/repos/{provider}/{owner}/{name}[/…]`: two hosts
//! can carry the same `owner/name`, and guessing between them would open
//! somebody else's repository. The provider-qualified shape stays served for
//! native repositories too — it is what the links already in circulation say,
//! and the web app resolves both onto the same page.
//!
//! The API path is a separate field, never the browser one: `html_url` is for
//! opening, `api_url` is for fetching, and a client that confuses them gets
//! JSON where it wanted a page (or an app shell where it wanted JSON).

/// Provider name of the repositories this forge hosts itself.
pub const NATIVE_PROVIDER: &str = "jeryu";

/// Public origin of the web UI, e.g. `https://git.neverhuman.org`. The
/// production unit sets it; no deployment hardcodes a domain.
pub const PUBLIC_ORIGIN_ENV: &str = "JERYU_PRODUCTION_ORIGIN";

/// The public origin this deployment is reached at, if it has one.
pub fn public_origin() -> Option<String> {
    std::env::var(PUBLIC_ORIGIN_ENV).ok()
}

/// Absolute form of a rooted path, using the configured public origin. Without
/// one (local dev, tests) the path is returned unchanged, which is still a
/// URL a client on the same host can follow.
pub fn absolute(path: &str) -> String {
    absolute_with_origin(public_origin().as_deref(), path)
}

pub fn absolute_with_origin(origin: Option<&str>, path: &str) -> String {
    match origin.map(|origin| origin.trim().trim_end_matches('/')) {
        Some(origin) if !origin.is_empty() && path.starts_with('/') => format!("{origin}{path}"),
        _ => path.to_string(),
    }
}

/// Whether `provider` names the forge's own hosting.
pub fn is_native(provider: &str) -> bool {
    provider.eq_ignore_ascii_case(NATIVE_PROVIDER)
}

/// Browser path of a repository: the short shape for a repository this forge
/// hosts, the provider-qualified one for a mirror of another host.
pub fn repo_browser_path(provider: &str, owner: &str, name: &str) -> String {
    if is_native(provider) {
        format!("/{owner}/{name}")
    } else {
        format!("/repos/{provider}/{owner}/{name}")
    }
}

/// Browser path of a pull request. The native shape is the forge-shaped
/// `pull/{number}`; the provider-qualified one keeps the web app's own
/// `pulls/{number}` tab path, which is what existing links say.
pub fn pull_browser_path(provider: &str, owner: &str, name: &str, number: u64) -> String {
    if is_native(provider) {
        format!("/{owner}/{name}/pull/{number}")
    } else {
        format!("/repos/{provider}/{owner}/{name}/pulls/{number}")
    }
}

/// The in-app path of a repository: the provider-qualified route the web app's
/// own pages are served at. A link inside a payload the web app renders uses
/// this, so a click resolves on the page it is on; a link meant to leave the
/// forge uses [`repo_browser_path`], which resolves onto the same page.
pub fn repo_app_path(provider: &str, owner: &str, name: &str) -> String {
    format!("/repos/{provider}/{owner}/{name}")
}

/// [`repo_app_path`] for a repository this forge hosts.
pub fn native_repo_app_path(owner: &str, name: &str) -> String {
    repo_app_path(NATIVE_PROVIDER, owner, name)
}

/// The in-app path of a pull request; see [`repo_app_path`].
pub fn pull_app_path(provider: &str, owner: &str, name: &str, number: u64) -> String {
    format!("/repos/{provider}/{owner}/{name}/pulls/{number}")
}

/// [`pull_app_path`] for a pull request this forge hosts.
pub fn native_pull_app_path(owner: &str, name: &str, number: u64) -> String {
    pull_app_path(NATIVE_PROVIDER, owner, name, number)
}

/// Canonical browser URL of a repository, absolute where an origin is set.
pub fn repo_html_url(provider: &str, owner: &str, name: &str) -> String {
    absolute(&repo_browser_path(provider, owner, name))
}

/// Canonical browser URL of a pull request, absolute where an origin is set.
pub fn pull_html_url(provider: &str, owner: &str, name: &str, number: u64) -> String {
    absolute(&pull_browser_path(provider, owner, name, number))
}

/// The native API path of a repository. `/api/v1/repos/` accepts the
/// `owner/name` pair as well as the UUID, so the link a client is handed reads
/// like the repository it names.
pub fn repo_api_path(owner: &str, name: &str) -> String {
    format!("/api/v1/repos/{owner}/{name}")
}

/// The native API path of a pull request.
pub fn pull_api_path(owner: &str, name: &str, number: u64) -> String {
    format!("/api/v1/repos/{owner}/{name}/pulls/{number}")
}

/// Absolute form of [`repo_api_path`].
pub fn repo_api_url(owner: &str, name: &str) -> String {
    absolute(&repo_api_path(owner, name))
}

/// Absolute form of [`pull_api_path`].
pub fn pull_api_url(owner: &str, name: &str, number: u64) -> String {
    absolute(&pull_api_path(owner, name, number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_repository_browser_paths_omit_the_provider() {
        assert_eq!(repo_browser_path("jeryu", "acme", "widget"), "/acme/widget");
        assert_eq!(
            pull_browser_path("jeryu", "acme", "widget", 7),
            "/acme/widget/pull/7"
        );
    }

    #[test]
    fn a_mirrored_repository_keeps_its_provider() {
        assert_eq!(
            repo_browser_path("github", "acme", "widget"),
            "/repos/github/acme/widget"
        );
        assert_eq!(
            pull_browser_path("github", "acme", "widget", 7),
            "/repos/github/acme/widget/pulls/7"
        );
    }

    #[test]
    fn an_in_app_path_stays_provider_qualified_and_resolves_to_the_same_object() {
        // The web app serves its pages under the provider-qualified route; a
        // canonical link resolves onto it rather than replacing it.
        assert_eq!(
            native_pull_app_path("acme", "widget", 7),
            "/repos/jeryu/acme/widget/pulls/7"
        );
        assert_eq!(
            native_repo_app_path("acme", "widget"),
            "/repos/jeryu/acme/widget"
        );
    }

    #[test]
    fn api_paths_are_separate_from_browser_paths() {
        assert_eq!(repo_api_path("acme", "widget"), "/api/v1/repos/acme/widget");
        assert_eq!(
            pull_api_path("acme", "widget", 7),
            "/api/v1/repos/acme/widget/pulls/7"
        );
        assert_ne!(
            pull_api_path("acme", "widget", 7),
            pull_browser_path("jeryu", "acme", "widget", 7)
        );
    }

    #[test]
    fn an_origin_makes_a_link_absolute_without_doubling_its_slash() {
        assert_eq!(
            absolute_with_origin(Some("https://git.example.org/"), "/acme/widget/pull/7"),
            "https://git.example.org/acme/widget/pull/7"
        );
        assert_eq!(
            absolute_with_origin(Some("  "), "/acme/widget"),
            "/acme/widget"
        );
        assert_eq!(absolute_with_origin(None, "/acme/widget"), "/acme/widget");
    }
}
