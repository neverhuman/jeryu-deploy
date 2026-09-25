//! Identity of the authenticated caller.
//!
//! `GET /user` and the GraphQL `viewer` field both answer "who am I?", and
//! `gh auth status`, `gh api user` and every agent that introspects its own
//! identity believe the answer. Both therefore render the caller's own
//! account — never a fixed service principal — so two tokens never report the
//! same login.

use jeryu_core::AccountSummary;
use serde_json::{Value, json};

use super::GithubRouter;
use super::support::json_response;
use crate::routes::Response;

/// Login reported to the trusted in-process edge, which is past authorization
/// and carries no token to resolve an account from. It matches the default
/// acting principal the write routes record.
const SERVICE_LOGIN: &str = "jeryu";
const SERVICE_NAME: &str = "Jeryu Local Operator";

impl GithubRouter {
    /// `GET /user` for a caller the web edge authenticated.
    pub(crate) fn user_for_account(&self, account: &AccountSummary) -> Response {
        json_response(200, &rest_user(&account.login, &account.display_name))
    }

    /// `GET /user` on the trusted in-process edge: no token, so the caller is
    /// the service principal. Its display name comes from the account store
    /// when one exists there.
    pub(super) fn service_user(&self) -> Response {
        let (login, name) = self.service_identity();
        json_response(200, &rest_user(&login, &name))
    }

    pub(super) fn viewer_json(&self, account: Option<&AccountSummary>) -> Value {
        let (login, name) = match account {
            Some(account) => (account.login.clone(), account.display_name.clone()),
            None => self.service_identity(),
        };
        json!({
            "login": login,
            "name": name,
            "id": node_id(&login),
        })
    }

    fn service_identity(&self) -> (String, String) {
        match self.core().get_account(SERVICE_LOGIN) {
            Ok(account) => (account.login, account.display_name),
            Err(_) => (SERVICE_LOGIN.to_owned(), SERVICE_NAME.to_owned()),
        }
    }
}

fn rest_user(login: &str, name: &str) -> Value {
    json!({
        "login": login,
        "id": user_id(login),
        "node_id": node_id(login),
        "type": "User",
        "name": name,
        "url": "/user",
    })
}

fn node_id(login: &str) -> String {
    format!("U_{login}")
}

/// A stable numeric id per login. GitHub's ids are opaque positive integers
/// clients key their caches on, so the same login must always render the same
/// id across restarts: derive it from the login (FNV-1a, folded into GitHub's
/// positive 32-bit range) rather than from insertion order.
fn user_id(login: &str) -> u32 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in login.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    // Fold to 31 bits and lift off zero: ids are positive and fit an i32,
    // which is what GitHub clients parse them into.
    #[allow(clippy::cast_possible_truncation)]
    let folded = ((hash ^ (hash >> 32)) & 0x7fff_ffff) as u32;
    folded.max(1)
}

#[cfg(test)]
mod tests {
    use super::{node_id, user_id};

    #[test]
    fn user_id_is_stable_and_distinct_per_login() {
        assert_eq!(user_id("alice"), user_id("alice"));
        assert_ne!(user_id("alice"), user_id("bob"));
        assert!(user_id("alice") > 0);
        assert!(user_id("") > 0);
    }

    #[test]
    fn node_id_follows_the_login() {
        assert_eq!(node_id("alice"), "U_alice");
    }
}
