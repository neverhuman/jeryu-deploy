//! The CI-A counters table from mock-provider-v1, end to end over HTTP.

use std::time::Duration;

use jeryu_mock_provider::{EventKind, MockConfig, MockProvider};
use serde_json::{Value, json};

struct Client {
    http: reqwest::Client,
    provider: MockProvider,
}

impl Client {
    async fn new(config: MockConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            provider: MockProvider::start(config).await.expect("start mock"),
        }
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self
            .http
            .post(self.provider.url(path))
            .json(&body)
            .send()
            .await
            .expect("post");
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn token(&self, form: &[(&str, &str)]) -> (u16, Value) {
        let response = self
            .http
            .post(self.provider.url("/oauth/token"))
            .form(form)
            .send()
            .await
            .expect("token");
        (
            response.status().as_u16(),
            response.json().await.unwrap_or(Value::Null),
        )
    }

    async fn chat(&self, bearer: Option<&str>) -> u16 {
        let mut request = self
            .http
            .post(self.provider.url("/v1/messages"))
            .json(&json!({}));
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        request.send().await.expect("chat").status().as_u16()
    }

    async fn get(&self, path: &str) -> Value {
        self.http
            .get(self.provider.url(path))
            .send()
            .await
            .expect("get")
            .json()
            .await
            .expect("json")
    }

    /// One device login: start, approve, then poll the token endpoint.
    async fn login(&self) -> Value {
        let (status, device) = self
            .post(
                "/oauth/device",
                json!({"provider": "claude", "owner_kind": "user", "owner_id": "user/alice",
                       "mode": "gateway_helper", "provider_account": "alice@example.com"}),
            )
            .await;
        assert_eq!(status, 200);
        let device_code = device["device_code"]
            .as_str()
            .expect("device code")
            .to_string();
        let grant = "urn:ietf:params:oauth:grant-type:device_code";
        let (status, pending) = self
            .token(&[("grant_type", grant), ("device_code", &device_code)])
            .await;
        assert_eq!(
            (status, pending["error"].as_str()),
            (400, Some("authorization_pending"))
        );
        let (status, _) = self
            .post(
                "/oauth/device/approve",
                json!({"user_code": device["user_code"]}),
            )
            .await;
        assert_eq!(status, 200);
        let (status, tokens) = self
            .token(&[("grant_type", grant), ("device_code", &device_code)])
            .await;
        assert_eq!(status, 200);
        tokens
    }
}

#[tokio::test]
async fn one_login_serves_three_sessions_and_disconnect_fences_them() {
    let client = Client::new(MockConfig::default()).await;
    let tokens = client.login().await;
    let connection_id = tokens["connection_id"].as_str().expect("id").to_string();
    let generation = tokens["connection_generation"]
        .as_u64()
        .expect("generation");
    assert_eq!(generation, 1);

    let leases = futures_join3(
        client.post(
            "/sessions",
            json!({"connection_id": connection_id, "connection_generation": 1}),
        ),
        client.post(
            "/sessions",
            json!({"connection_id": connection_id, "connection_generation": 1}),
        ),
        client.post(
            "/sessions",
            json!({"connection_id": connection_id, "connection_generation": 1}),
        ),
    )
    .await;
    let mut access = Vec::new();
    for (status, lease) in leases {
        assert_eq!(status, 200, "{lease}");
        access.push(lease["access_token"].as_str().expect("access").to_string());
    }
    for token in &access {
        assert_eq!(
            client.chat(Some(token)).await,
            200,
            "the injected token authorizes chat"
        );
    }
    let refresh = tokens["refresh_token"].as_str().expect("refresh");
    assert_eq!(
        client.chat(Some(refresh)).await,
        401,
        "a refresh token is not a bearer"
    );
    assert_eq!(
        client.chat(None).await,
        401,
        "the mock never adds a credential"
    );

    // Refresh: persisted, generation bumped, and only then a lease.
    let (status, refreshed) = client
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("connection_id", &connection_id),
        ])
        .await;
    assert_eq!(status, 200);
    assert_eq!(refreshed["connection_generation"], 2);
    let (status, stale) = client
        .post(
            "/sessions",
            json!({"connection_id": connection_id, "connection_generation": 1}),
        )
        .await;
    assert_eq!(
        (status, stale["error"].as_str()),
        (409, Some("connection_generation_stale"))
    );
    assert_eq!(
        client.chat(Some(&access[0])).await,
        401,
        "generation 1 access is fenced"
    );
    let (status, replay) = client
        .token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("connection_id", &connection_id),
        ])
        .await;
    assert_eq!(
        (status, replay["error"].as_str()),
        (409, Some("connection_generation_stale"))
    );

    let (status, _) = client
        .post(
            &format!("/connections/{connection_id}/disconnect"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    let fresh = refreshed["access_token"].as_str().expect("access");
    assert_eq!(
        client.chat(Some(fresh)).await,
        401,
        "disconnect fences live access"
    );
    let (status, gone) = client
        .post(
            "/sessions",
            json!({"connection_id": connection_id, "connection_generation": 3}),
        )
        .await;
    assert_eq!(
        (status, gone["error"].as_str()),
        (409, Some("provider_not_connected"))
    );

    let counters = client.get("/counters").await;
    assert_eq!(counters["connections_issued"], 1);
    assert!(counters["concurrent_sessions_peak"].as_u64() >= Some(3));
    assert_eq!(counters["refreshes_persisted"], 1);
    assert_eq!(counters["refresh_before_lease_ok"], 1);
    assert_eq!(counters["stale_generation_refusals"], 1);
    assert_eq!(counters["chat_authorized"], 3);
    assert_eq!(counters["chat_rejected"], 4);
    assert_eq!(counters["outbound_dial_attempts"], 0);
    let body = counters.to_string();
    assert!(
        !body.contains(refresh) && !body.contains(&access[0]),
        "no token in counters"
    );

    // In the event log, the refresh's persist precedes its lease.
    let events: Vec<EventKind> = serde_json::from_value::<Vec<Value>>(client.get("/events").await)
        .expect("events")
        .iter()
        .map(|event| serde_json::from_value(event["kind"].clone()).expect("kind"))
        .collect();
    let bumped = events
        .iter()
        .position(|kind| *kind == EventKind::GenerationBumped)
        .expect("bump");
    assert_eq!(events[bumped - 1], EventKind::RefreshPersisted);
    assert_eq!(events[bumped + 1], EventKind::AccessLeaseIssued);
}

#[tokio::test]
async fn a_failed_persist_mints_nothing_and_keeps_the_old_refresh_token() {
    let state_dir = tempfile::tempdir().expect("state dir");
    let config = MockConfig {
        fail_persist_once: true,
        state_file: Some(state_dir.path().join("refresh.log")),
        ..MockConfig::default()
    };
    let client = Client::new(config).await;
    let tokens = client.login().await;
    let connection_id = tokens["connection_id"].as_str().expect("id");
    let refresh = tokens["refresh_token"].as_str().expect("refresh");
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("connection_id", connection_id),
    ];
    let (status, failed) = client.token(&form).await;
    assert_eq!(
        (status, failed["error"].as_str()),
        (503, Some("refresh_persist_failed"))
    );
    let leases_after_failure = client.get("/counters").await["access_leases_issued"].clone();
    let (status, retried) = client.token(&form).await;
    assert_eq!(status, 200, "the old refresh token is still the stored one");
    assert_eq!(retried["connection_generation"], 2);
    assert_eq!(leases_after_failure, 1, "the failed refresh minted nothing");
    let persisted = std::fs::read_to_string(state_dir.path().join("refresh.log")).expect("log");
    assert_eq!(
        persisted.lines().count(),
        2,
        "login and one successful rotation were fsynced"
    );
}

#[tokio::test]
async fn unknown_providers_and_expired_access_are_refused() {
    let client = Client::new(MockConfig {
        access_ttl: Duration::from_millis(50),
        ..MockConfig::default()
    })
    .await;
    let (status, jekko) = client
        .post("/oauth/device", json!({"provider": "jekko"}))
        .await;
    assert_eq!(
        (status, jekko["error"].as_str()),
        (400, Some("provider_unknown"))
    );
    let tokens = client.login().await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        client.chat(tokens["access_token"].as_str()).await,
        401,
        "expired access"
    );
    let (status, expired) = client
        .post(
            "/sessions",
            json!({"connection_id": tokens["connection_id"], "connection_generation": 1}),
        )
        .await;
    assert_eq!(
        (status, expired["error"].as_str()),
        (409, Some("access_expired"))
    );
}

async fn futures_join3<A, B, C>(a: A, b: B, c: C) -> [(u16, Value); 3]
where
    A: std::future::Future<Output = (u16, Value)>,
    B: std::future::Future<Output = (u16, Value)>,
    C: std::future::Future<Output = (u16, Value)>,
{
    let (a, b, c) = tokio::join!(a, b, c);
    [a, b, c]
}
