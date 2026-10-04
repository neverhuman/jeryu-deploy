//! The web app's own route patterns, as its router declares them, and the
//! check that an href the server emits lands on one of them.
//!
//! The forge hands people paths into the web app (an attention item's `href`
//! is the whole next step for most kinds), so a path that is not a route of
//! the app is a dead link, and one the app only answers with a redirect is a
//! detour that breaks as soon as the redirect goes. Both are mistakes a test
//! can see, and this list is what it sees them against. When a route is added,
//! renamed or retired in `apps/web/src/app/router.tsx`, change it here too.

/// Every route the web app answers directly, `:name` matching one segment and
/// a trailing `*` the rest. Paths the app answers only by redirecting
/// somewhere else are deliberately absent: see [`REDIRECTED`].
pub(crate) const WEB_ROUTES: &[&str] = &[
    "/",
    "/needs-you",
    "/activity",
    "/search",
    "/repos",
    "/repos/new",
    "/repos/family/:family",
    // One catch-all per provider; each repository page parses the rest of the
    // splat, which is `<name>[/sub-path]`. A repository is addressed by all
    // three of provider, owner and name, so the pattern spells out that a
    // shorter path is somebody else's page, not this one.
    "/repos/:provider/:owner/*",
    // Work is one page: the queue of every family, with the composer (`#add`)
    // and the workers strip (`#workers`) as places on it.
    "/work",
    "/work/:key",
    "/in-flight",
    "/releases",
    "/releases/family/:family",
    "/intelligence",
    "/intelligence/dependencies",
    "/quality-gate",
    "/quality-gate/rules/:rule",
    "/quality-gate/heads/:owner/:name/:sha",
    "/runners",
    "/shared-tools/findings",
    "/shared-tools/proposals",
    "/shared-tools/adoption",
    "/shared-tools/adoption/:tool",
    "/settings",
    "/wiki",
    "/wiki/*",
];

/// Paths the web app still answers for the sake of old links and bookmarks, by
/// sending the reader somewhere else. Nothing the server emits should need
/// one; each is paired with what to emit instead so a failure says so.
pub(crate) const REDIRECTED: &[(&str, &str)] = &[
    ("/login", "/"),
    ("/signup", "/"),
    ("/work/shift", "/work (one todo: /work/<id>)"),
    ("/work/shift/new", "/work#add"),
    ("/work/shift/workers", "/work#workers"),
    ("/pull-room", "/in-flight"),
    ("/unreleased", "/releases"),
    ("/fleet", "/runners"),
    ("/shared-code", "/shared-tools/findings"),
    ("/shared-tools", "/shared-tools/findings"),
    ("/tools", "/shared-tools/findings"),
    ("/tool-fleet", "/shared-tools/adoption"),
    ("/tool-fleet/:tool", "/shared-tools/adoption/:tool"),
    ("/notifications", "/activity"),
];

/// The path of an href: its query string and its in-page anchor are the page's
/// own business, not the router's.
fn path_of(href: &str) -> &str {
    let href = href.split('#').next().unwrap_or(href);
    href.split('?').next().unwrap_or(href)
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// Whether `path` is what `pattern` matches: segment for segment, `:name`
/// taking any one segment and a trailing `*` the rest (one segment at least,
/// as the app's own catch-all routes do).
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (segments(pattern), segments(path));
    for (index, part) in pattern.iter().enumerate() {
        if *part == "*" {
            return path.len() > index;
        }
        match path.get(index) {
            Some(segment) if part.starts_with(':') || part == segment => {}
            _ => return false,
        }
    }
    pattern.len() == path.len()
}

/// The route pattern `href` opens, or why it opens none.
pub(crate) fn route_of(href: &str) -> Result<&'static str, String> {
    let path = path_of(href);
    // The redirects are asked first because the app asks them first: it
    // declares `work/shift` ahead of `work/:key`, so that path is a redirect
    // and not a todo named "shift".
    if let Some((_, canonical)) = REDIRECTED
        .iter()
        .find(|(redirect, _)| matches(redirect, path))
    {
        return Err(format!(
            "{href} is a path the web app answers by sending the reader to {canonical}; \
             emit that instead"
        ));
    }
    match WEB_ROUTES.iter().find(|route| matches(route, path)) {
        Some(route) => Ok(route),
        None => Err(format!(
            "{href} is not a route of the web app; emit one of {WEB_ROUTES:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_matches_a_path_segment_for_segment() {
        assert!(matches("/work/:key", "/work/acme-1"));
        assert!(!matches("/work/:key", "/work"));
        assert!(!matches("/work/:key", "/work/acme-1/extra"));
        assert!(matches(
            "/repos/:provider/:owner/*",
            "/repos/jeryu/acme/widget/pulls/7"
        ));
        assert!(!matches("/repos/:provider/:owner/*", "/repos/acme/widget"));
        assert!(matches("/", "/"));
    }

    #[test]
    fn a_query_string_or_anchor_is_not_part_of_the_path() {
        assert_eq!(route_of("/work?family=acme"), Ok("/work"));
        assert_eq!(route_of("/work#workers"), Ok("/work"));
        assert_eq!(route_of("/releases?repo=acme/widget"), Ok("/releases"));
    }

    #[test]
    fn a_redirected_path_is_refused_and_names_what_to_emit() {
        let refusal = route_of("/work/shift?family=acme&todo=acme-1").expect_err("not a route");
        assert!(refusal.contains("/work/<id>"), "{refusal}");
        assert!(route_of("/unreleased").is_err());
        assert!(route_of("/repos/acme/widget").is_err());
    }
}
