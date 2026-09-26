//! Authenticated HTTP JSON transport for live `jeryu-api` commands.

use std::{io::Read, time::Duration};

use reqwest::{
    Method, Url,
    blocking::{Client, Response},
    redirect::Policy,
};
use serde_json::Value;

use crate::client::{ApiFailure, ClientError, ClientResult};

pub(crate) struct ApiClient {
    base: Url,
    client: Client,
    token: Option<String>,
}

impl ApiClient {
    pub(crate) fn new(api_url: &str) -> ClientResult<Self> {
        let token = match std::env::var_os("JERYU_TOKEN_FILE") {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|_| ClientError::Invalid("cannot read JERYU_TOKEN_FILE".into()))?
                    .trim()
                    .to_owned(),
            ),
            None => std::env::var("JERYU_TOKEN").ok(),
        };
        Self::with_token(api_url, token)
    }

    fn with_token(api_url: &str, token: Option<String>) -> ClientResult<Self> {
        let base =
            Url::parse(api_url).map_err(|_| ClientError::Invalid("invalid API URL".into()))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::Invalid(
                "API URL must be HTTP(S), without credentials, query or fragment".into(),
            ));
        }
        if token
            .as_ref()
            .is_some_and(|t| t.is_empty() || t.chars().any(char::is_control))
        {
            return Err(ClientError::Invalid("invalid API token".into()));
        }
        // A redirect would replay the bearer token at an address the operator
        // never named, so the transport follows none of them.
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(Policy::none())
            .build()
            .map_err(|_| ClientError::NotWired("cannot initialize HTTP client".into()))?;
        Ok(Self {
            base,
            client,
            token,
        })
    }

    pub(crate) fn get(&self, path: &str) -> ClientResult<Value> {
        self.request(Method::GET, path, None)
    }

    /// A caller can bound the entire paginated read, including response bytes.
    ///
    /// Returns the parsed body and the bytes it cost, so a caller reading page
    /// after page can subtract each page from one shared budget.
    pub(crate) fn get_bounded(
        &self,
        path: &str,
        timeout: Duration,
        max_bytes: usize,
    ) -> ClientResult<(Value, usize)> {
        if timeout.is_zero() || max_bytes == 0 {
            return Err(ClientError::Invalid("API read budget exhausted".into()));
        }
        let response = self.send(Method::GET, path, None, Some(timeout))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ClientError::NotWired("API response read failed".into()))?;
        if bytes.len() > max_bytes {
            return Err(ClientError::Invalid(
                "API response exceeds read byte budget".into(),
            ));
        }
        let text = String::from_utf8_lossy(&bytes);
        if !(200..300).contains(&status) {
            return Err(Self::failure(status, &text));
        }
        let value = serde_json::from_slice(&bytes)
            .map_err(|_| ClientError::Invalid("API returned invalid JSON".into()))?;
        Ok((value, bytes.len()))
    }

    pub(crate) fn post(&self, path: &str, body: Value) -> ClientResult<Value> {
        self.request(Method::POST, path, Some(body))
    }

    pub(crate) fn put(&self, path: &str, body: Value) -> ClientResult<Value> {
        self.request(Method::PUT, path, Some(body))
    }

    fn request(&self, method: Method, path: &str, body: Option<Value>) -> ClientResult<Value> {
        let response = self.send(method, path, body, None)?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .map_err(|_| ClientError::NotWired("API response read failed".into()))?;
        if !(200..300).contains(&status) {
            return Err(Self::failure(status, &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|_| {
            ClientError::Invalid(format!(
                "parse JSON response: HTTP {status} body is not JSON: {}",
                text.trim()
            ))
        })
    }

    /// The API's non-2xx answer, carrying the body verbatim.
    ///
    /// An error body that is not JSON is still the API's answer: keep it as a
    /// JSON string so `--json` hands the caller every byte of it.
    fn failure(status: u16, text: &str) -> ClientError {
        let body =
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.trim().to_string()));
        let message = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(text.trim())
            .to_string();
        ClientError::Api(Box::new(ApiFailure {
            status,
            message,
            body,
        }))
    }

    fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Option<Duration>,
    ) -> ClientResult<Response> {
        let url = format!("{}{}", self.base.as_str().trim_end_matches('/'), path);
        let mut request = self
            .client
            .request(method, &url)
            .header("Accept", "application/json");
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().map_err(|error| {
            ClientError::NotWired(format!("API request failed: {}", error.without_url()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[test]
    fn sends_bearer_auth_and_decodes_chunked_json() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/prefix", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /prefix/repos http/1.1"));
            assert!(request.contains("authorization: bearer fixture-token\r\n"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n").unwrap();
        });
        assert_eq!(
            ApiClient::with_token(&url, Some("fixture-token".into()))
                .unwrap()
                .get("/repos")
                .unwrap(),
            serde_json::json!([])
        );
        server.join().unwrap();
    }

    #[test]
    fn refuses_credentials_in_url_and_unreachable_server() {
        assert!(ApiClient::with_token("http://user:password@localhost", None).is_err());
        assert!(ApiClient::with_token("https://example.org", None).is_ok());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(
            ApiClient::with_token(&format!("http://{address}"), None)
                .unwrap()
                .get("/repos")
                .is_err()
        );
    }

    #[test]
    fn rejects_an_empty_or_control_character_token() {
        assert!(ApiClient::with_token("http://127.0.0.1:8787", Some(String::new())).is_err());
        assert!(
            ApiClient::with_token("http://127.0.0.1:8787", Some("bad\r\nheader".into())).is_err()
        );
    }

    #[test]
    fn a_redirect_is_never_followed_and_maps_to_the_api_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let error = ApiClient::with_token(&url, Some("fixture-token".into()))
            .unwrap()
            .get("/repos")
            .unwrap_err();
        assert!(matches!(&error, ClientError::Api(failure) if failure.status == 302));
        server.join().unwrap();
    }

    #[test]
    fn bounded_read_rejects_oversized_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "missing bounded read");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16 * 1024);
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull").unwrap();
        });
        let api = ApiClient::with_token(&url, None).unwrap();
        assert!(
            matches!(api.get_bounded("/checks", Duration::from_secs(2), 2), Err(ClientError::Invalid(message)) if message.contains("byte budget"))
        );
        assert!(api.get_bounded("/checks", Duration::ZERO, 16).is_err_and(
            |error| matches!(&error, ClientError::Invalid(m) if m.contains("budget exhausted"))
        ));
        server.join().unwrap();
    }
}
