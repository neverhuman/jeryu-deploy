//! Who a login actually is: normalized principal ids for independence checks.
//!
//! Required evidence and approvals only mean something when they come from
//! someone other than the change's author, and "someone else" is a question
//! about identity, not about spelling. A display-string comparison accepts
//! `Alton2` approving `alton2`'s own pull request, or a runner's second login
//! scoring the head its own author pushed. Every independence check in the web
//! surface compares normalized ids instead: trimmed, a leading `@` dropped,
//! ASCII-case-folded, and then mapped through the site's alias table.
//!
//! The alias table is site configuration (`JERYU_PRINCIPAL_ALIASES`), never a
//! default in code — jeryu is public and ships no identities of its own. Each
//! entry is `canonical=alias[,alias...]`; entries are separated by `;`,
//! newlines or whitespace. An alias of an alias is not followed: the canonical
//! name is whatever the entry puts on the left.

/// Site configuration naming the logins that are the same person or robot.
const PRINCIPAL_ALIASES_ENV: &str = "JERYU_PRINCIPAL_ALIASES";

/// The principal a login denotes, or `None` when the login names nobody. An
/// unattributable producer is never "someone else": a caller that cannot say
/// who produced a datum has to refuse it, not compare it.
pub(in crate::web) fn principal(login: &str) -> Option<String> {
    principal_with_aliases(site_aliases().as_deref(), login)
}

/// Whether two logins are the same principal. `false` when either names
/// nobody, so a missing producer never accidentally matches a missing author.
pub(in crate::web) fn same_principal(left: &str, right: &str) -> bool {
    same_principal_with_aliases(site_aliases().as_deref(), left, right)
}

fn site_aliases() -> Option<String> {
    std::env::var(PRINCIPAL_ALIASES_ENV).ok()
}

/// [`principal`] against an explicit alias table. Pure, so the identity rules
/// are tested without mutating shared process env.
pub(in crate::web) fn principal_with_aliases(aliases: Option<&str>, login: &str) -> Option<String> {
    let normalized = normalize(login);
    if normalized.is_empty() {
        return None;
    }
    Some(resolve(aliases, &normalized))
}

/// [`same_principal`] against an explicit alias table.
pub(in crate::web) fn same_principal_with_aliases(
    aliases: Option<&str>,
    left: &str,
    right: &str,
) -> bool {
    match (
        principal_with_aliases(aliases, left),
        principal_with_aliases(aliases, right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// The comparable spelling of one login, before aliases.
fn normalize(login: &str) -> String {
    login
        .trim()
        .trim_start_matches('@')
        .trim()
        .to_ascii_lowercase()
}

/// The canonical name the alias table gives `normalized`, or `normalized`.
fn resolve(aliases: Option<&str>, normalized: &str) -> String {
    for (canonical, alias) in alias_pairs(aliases) {
        if alias == normalized {
            return canonical;
        }
    }
    normalized.to_string()
}

/// Every `(canonical, alias)` pair the configuration declares, normalized.
fn alias_pairs(aliases: Option<&str>) -> impl Iterator<Item = (String, String)> + '_ {
    aliases
        .unwrap_or_default()
        .split([';', ' ', '\t', '\n', '\r'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| entry.split_once('='))
        .flat_map(|(canonical, listed)| {
            let canonical = normalize(canonical);
            listed
                .split(',')
                .map(normalize)
                .filter(|alias| !alias.is_empty())
                .map(move |alias| (canonical.clone(), alias))
                .collect::<Vec<_>>()
        })
        .filter(|(canonical, _)| !canonical.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALIASES: &str = "gatebot=gate-bot,gatebot-2; mina=mina.q";

    #[test]
    fn a_login_names_a_principal_only_when_it_names_something() {
        assert_eq!(
            principal_with_aliases(None, "  Mina  "),
            Some("mina".into())
        );
        assert_eq!(principal_with_aliases(None, "@Mina"), Some("mina".into()));
        assert_eq!(principal_with_aliases(None, "   "), None);
        assert_eq!(principal_with_aliases(None, "@"), None);
    }

    #[test]
    fn spelling_never_makes_one_principal_into_two() {
        assert!(same_principal_with_aliases(None, "Mina", "mina"));
        assert!(same_principal_with_aliases(None, "@mina ", "mina"));
        assert!(same_principal_with_aliases(Some(ALIASES), "mina.q", "Mina"));
        assert!(same_principal_with_aliases(
            Some(ALIASES),
            "GATE-BOT",
            "gatebot-2"
        ));
    }

    #[test]
    fn distinct_principals_stay_distinct_and_nobody_matches_nobody() {
        assert!(!same_principal_with_aliases(
            Some(ALIASES),
            "mina",
            "gatebot"
        ));
        assert!(!same_principal_with_aliases(None, "", ""));
        assert!(!same_principal_with_aliases(None, "mina", ""));
        // An alias is not transitive: the table says who is canonical.
        assert!(!same_principal_with_aliases(
            Some("mina=mina.q"),
            "mina.q",
            "mina.q.alt"
        ));
    }

    #[test]
    fn a_malformed_alias_table_is_ignored_entry_by_entry() {
        let aliases = Some("not-a-pair; =orphan; mina=mina.q,,");
        assert!(same_principal_with_aliases(aliases, "mina.q", "mina"));
        assert!(!same_principal_with_aliases(aliases, "orphan", "mina"));
        assert_eq!(
            principal_with_aliases(aliases, "not-a-pair"),
            Some("not-a-pair".into())
        );
    }
}
