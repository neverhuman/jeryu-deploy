//! Strict query validation for the `/api/v1` list routes.
//!
//! A filter that can never match is a mistake in the request, not an empty
//! collection: `?status=bogus` and the misspelled `?statuss=open` both used to
//! answer `200` with no rows, which reads as "there is nothing to do". Every
//! route that filters a list by a closed set reads its query through
//! [`StrictQuery`], which refuses a key the route does not read and a value
//! outside the set, naming in the error what it does accept.
//!
//! A route declares both with [`StrictFields`]: `KEYS` is every key it reads
//! (including the ones a flattened struct such as `PageParams` contributes),
//! and `check_values` validates the closed sets with [`one_of`]. The refusal
//! is the published `invalid_query` code at `422`; a route whose query error
//! already has its own code keeps it through [`StrictFields::CODE`].

use axum::extract::{FromRequestParts, Query};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response as AxumResponse};
use serde::de::DeserializeOwned;

use super::workcells_support::{TypedError, typed_error};

pub(crate) const CODE: &str = "invalid_query";
const DOCS: &str = "docs/errors.md";

/// What a route accepts in its query string.
pub(crate) trait StrictFields: Sized {
    /// Every key the route reads. Any other key is refused: a client that
    /// misspells a filter must hear about it rather than read an unfiltered
    /// list as a filtered one.
    const KEYS: &'static [&'static str];

    /// The error code the refusal answers with.
    const CODE: &'static str = CODE;

    /// What the refusal tells a caller to do, when the route's parameters need
    /// more than the keys and values the message already names.
    const REPAIR_HINT: &'static str = "send a key this route reads, with a value the message names";

    /// The closed sets this route's values must come from. The default accepts
    /// every value the type could deserialize.
    fn check_values(&self) -> Result<(), String> {
        Ok(())
    }
}

/// `value` is one of `allowed`, or why it is not.
pub(crate) fn one_of(name: &str, value: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&value) {
        return Ok(());
    }
    Err(format!(
        "{name}: {value:?} is not one of {}",
        allowed.join(", ")
    ))
}

/// [`one_of`] for an optional filter: absent or empty filters nothing.
pub(crate) fn filter_one_of(
    name: &str,
    value: Option<&String>,
    allowed: &[&str],
) -> Result<(), String> {
    match value.map(|value| value.trim()).filter(|v| !v.is_empty()) {
        None => Ok(()),
        Some(value) => one_of(name, value, allowed),
    }
}

/// A query string read strictly: every key is one the route reads, and every
/// closed-set value is in its set.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StrictQuery<T>(pub T);

/// A refused query; answers `invalid_query` (or the route's own code).
#[derive(Debug)]
pub(crate) struct QueryRefused {
    reason: String,
    keys: &'static [&'static str],
    code: &'static str,
    repair_hint: &'static str,
}

impl IntoResponse for QueryRefused {
    fn into_response(self) -> AxumResponse {
        let reads = format!("this route reads only {}", self.keys.join(", "));
        typed_error(TypedError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: self.code,
            purpose: "filter a list with query parameters",
            reason: &self.reason,
            common_fixes: &[&reads, "drop the filter to list everything"],
            docs_url: DOCS,
            repair_hint: self.repair_hint,
            message: &self.reason,
        })
    }
}

/// The key half of a `key=value` pair, percent-decoded.
fn key_of(pair: &str) -> String {
    let raw = pair.split('=').next().unwrap_or(pair);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every key a query string names, in the order it names them.
fn keys(query: &str) -> Vec<String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(key_of)
        .collect()
}

#[axum::async_trait]
impl<T, S> FromRequestParts<S> for StrictQuery<T>
where
    T: DeserializeOwned + StrictFields,
    S: Send + Sync,
{
    type Rejection = QueryRefused;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let refuse = |reason: String| QueryRefused {
            reason,
            keys: T::KEYS,
            code: T::CODE,
            repair_hint: T::REPAIR_HINT,
        };
        for key in keys(parts.uri.query().unwrap_or_default()) {
            if !T::KEYS.contains(&key.as_str()) {
                return Err(refuse(format!(
                    "{key:?} is not a parameter of this route; it reads {}",
                    T::KEYS.join(", ")
                )));
            }
        }
        let Query(value) = Query::<T>::try_from_uri(&parts.uri)
            .map_err(|rejection| refuse(rejection.body_text()))?;
        value.check_values().map_err(refuse)?;
        Ok(Self(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// `uri`'s query read strictly, or the error envelope it is refused with.
    async fn refusal<T: DeserializeOwned + StrictFields>(uri: &str) -> Option<Value> {
        let request = axum::http::Request::builder()
            .uri(uri)
            .body(())
            .expect("request");
        let (mut parts, ()) = request.into_parts();
        match StrictQuery::<T>::from_request_parts(&mut parts, &()).await {
            Ok(_) => None,
            Err(refused) => {
                let response = refused.into_response();
                assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{uri}");
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body");
                Some(serde_json::from_slice(&bytes).expect("envelope"))
            }
        }
    }

    /// `uri` is refused, and the refusal says `named`.
    async fn refused<T: DeserializeOwned + StrictFields>(uri: &str, code: &str, named: &[&str]) {
        let body = refusal::<T>(uri).await.unwrap_or_else(|| {
            panic!("{uri} was accepted");
        });
        assert_eq!(body["code"], code, "{uri}");
        let said = format!("{} {}", body["reason"], body["common_fixes"]);
        for want in named {
            assert!(said.contains(want), "{uri} does not say {want:?}: {said}");
        }
    }

    async fn accepted<T: DeserializeOwned + StrictFields>(uri: &str) {
        assert!(refusal::<T>(uri).await.is_none(), "{uri} was refused");
    }

    /// A filter that could never match is refused, naming the values the route
    /// accepts; the same route still accepts every value in the set.
    #[tokio::test]
    async fn a_value_outside_the_closed_set_is_refused() {
        use crate::web::merge_queue::QueueListQuery;
        use crate::web::pipeline::attention::AttentionQuery;
        use crate::web::shift::TodosQuery;

        refused::<TodosQuery>(
            "/?status=bogus",
            CODE,
            &[
                "bogus",
                "open, claimed, done, blocked, handoff, parked, closed",
            ],
        )
        .await;
        refused::<TodosQuery>("/?mode=bogus", CODE, &["now, night"]).await;
        accepted::<TodosQuery>("/?status=closed&mode=night&limit=5").await;

        refused::<QueueListQuery>(
            "/?state=bogus",
            CODE,
            &["building, landed, failed, dequeued, all"],
        )
        .await;
        for state in ["building", "landed", "failed", "dequeued", "all", ""] {
            accepted::<QueueListQuery>(&format!("/?state={state}")).await;
        }

        refused::<AttentionQuery>("/?severity=bogus", CODE, &["critical, action, watch"]).await;
        refused::<AttentionQuery>("/?kind=bogus", CODE, &["todo_blocked"]).await;
        accepted::<AttentionQuery>("/?severity=critical&kind=todo_blocked").await;
    }

    /// A key the route does not read is a typo, not a filter that does nothing.
    #[tokio::test]
    async fn an_unknown_key_is_refused_naming_the_keys_the_route_reads() {
        use crate::web::pipeline::EventsQuery;
        use crate::web::shift::TodosQuery;

        refused::<TodosQuery>("/?statuss=open", CODE, &["statuss", "status", "worked_by"]).await;
        refused::<TodosQuery>("/?page=1&pages=2", CODE, &["pages"]).await;
        // A route with its own published query code keeps it.
        refused::<EventsQuery>("/?familyy=acme", "events_invalid_query", &["family"]).await;
        refused::<EventsQuery>("/?limit=nine", "events_invalid_query", &["limit"]).await;
        accepted::<EventsQuery>("/?family=acme&limit=9&needs_human=true").await;
    }

    #[test]
    fn a_key_is_read_without_its_value() {
        assert_eq!(keys("status=open&mode=now"), ["status", "mode"]);
        assert_eq!(keys(""), Vec::<String>::new());
        // A value carrying an `=` or an encoded key still reads as one key.
        assert_eq!(keys("repo=a=b"), ["repo"]);
        assert_eq!(keys("worked%5Fby=dana"), ["worked_by"]);
        assert_eq!(keys("per+page=1"), ["per page"]);
    }

    #[test]
    fn a_closed_set_names_what_it_accepts() {
        assert_eq!(one_of("status", "open", &["open", "done"]), Ok(()));
        assert_eq!(
            one_of("status", "bogus", &["open", "done"]),
            Err("status: \"bogus\" is not one of open, done".to_string())
        );
        assert_eq!(filter_one_of("status", None, &["open"]), Ok(()));
        assert_eq!(
            filter_one_of("status", Some(&"  ".to_string()), &["open"]),
            Ok(())
        );
    }
}
