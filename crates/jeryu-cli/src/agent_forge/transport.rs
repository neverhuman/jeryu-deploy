use super::request::Request;
use anyhow::{Result, ensure};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Duration,
};

pub(super) fn origin(value: &str) -> Result<String> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "expected HTTPS origin"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub(super) fn read_token(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)?;
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.mode() & 0o7777 == 0o600
            && info.uid() == rustix::process::getuid().as_raw(),
        "invalid token custody"
    );
    let mut token = String::new();
    (&mut file).take(4097).read_to_string(&mut token)?;
    ensure!(token.len() <= 4096, "token too large");
    let token = token.trim();
    ensure!(
        !token.is_empty() && token.bytes().all(|c| c.is_ascii_graphic()),
        "invalid token"
    );
    Ok(token.to_owned())
}

pub(super) fn send(origin: &str, token: &str, spec: Request) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut request = client
        .request(spec.method, format!("{origin}{}", spec.route))
        .bearer_auth(token);
    if let Some(body) = spec.body {
        request = request.json(&body);
    }
    let response = request.send()?;
    ensure!(response.status().is_success(), "forge refused request");
    let mut bytes = Vec::new();
    response.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "response too large");
    Ok(String::from_utf8(bytes)?)
}
