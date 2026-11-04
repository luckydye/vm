//! Pulls from private container registries without credentials in the guest.
//!
//! The guest agent mirrors each registry listed in `~/.varmint/registries`,
//! one host per line, on its loopback. For every repository it pulls, it asks
//! the host for a bearer token. The host requests one with the credentials
//! `docker login` saved in `~/.docker/config.json`, scoped to pulling that
//! repository, so the guest only ever holds short-lived pull tokens.

use crate::{
    instance,
    protocol::{RegistryToken, RegistryTokenRequest},
};
use std::{
    env, fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

/// How long a token lasts when the registry does not say, per the token spec.
const DEFAULT_EXPIRES_IN: u64 = 60;

pub fn allowed() -> Result<Vec<String>, String> {
    let path = instance::home().join("registries");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let registries: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    for registry in &registries {
        if !registry.chars().all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c)) {
            return Err(format!("invalid registry {registry:?} in {}", path.display()));
        }
    }
    Ok(registries)
}

pub fn token(allowed: &[String], request: &RegistryTokenRequest) -> RegistryToken {
    match pull_token(allowed, &request.registry, &request.repository) {
        Ok((token, expires_in)) => RegistryToken {
            id: request.id,
            token: Some(token),
            expires_in: Some(expires_in),
            error: None,
        },
        Err(error) => RegistryToken {
            id: request.id,
            error: Some(error),
            ..Default::default()
        },
    }
}

fn pull_token(allowed: &[String], registry: &str, repository: &str) -> Result<(String, u64), String> {
    if !allowed.iter().any(|host| host == registry) {
        return Err(format!("{registry} is not in ~/.varmint/registries"));
    }
    let valid_repository = !repository.is_empty()
        && repository
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/".contains(c));
    if !valid_repository {
        return Err(format!("invalid repository name {repository:?}"));
    }
    let auth = credentials(registry)?;

    let headers = curl(&format!("url = {}\nhead\n", quote(&format!("https://{registry}/v2/"))))?;
    let challenge = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("www-authenticate").then(|| value.trim())
        })
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| format!("{registry} does not ask for a bearer token"))?;
    let parameter = |name: &str| {
        challenge.split(',').find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.trim_matches('"').to_owned())
        })
    };
    let realm = parameter("realm").ok_or_else(|| format!("{registry} names no token realm"))?;
    // The credentials never go out unencrypted.
    if !realm.starts_with("https://") {
        return Err(format!("{registry} names a token realm without https: {realm}"));
    }

    let mut config = format!(
        "url = {}\nget\nfail\ndata-urlencode = {}\nheader = {}\n",
        quote(&realm),
        quote(&format!("scope=repository:{repository}:pull")),
        quote(&format!("Authorization: Basic {auth}")),
    );
    if let Some(service) = parameter("service") {
        config += &format!("data-urlencode = {}\n", quote(&format!("service={service}")));
    }
    let body = curl(&config)?;
    let reply: serde_json::Value =
        serde_json::from_str(&body).map_err(|error| format!("malformed token from {realm}: {error}"))?;
    let token = reply["token"]
        .as_str()
        .or(reply["access_token"].as_str())
        .ok_or_else(|| format!("{realm} returned no token"))?;
    Ok((
        token.to_owned(),
        reply["expires_in"].as_u64().unwrap_or(DEFAULT_EXPIRES_IN),
    ))
}

/// The base64 `user:password` that `docker login` saved for `registry`.
fn credentials(registry: &str) -> Result<String, String> {
    let path = PathBuf::from(env::var_os("HOME").expect("HOME is not set")).join(".docker/config.json");
    let text = fs::read_to_string(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let config: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("invalid {}: {error}", path.display()))?;
    config["auths"][registry]["auth"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "no credentials for {registry} in {}; run docker login {registry}",
                path.display()
            )
        })
}

/// Runs curl with its options on stdin, where `ps` does not show them.
fn curl(config: &str) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args(["--silent", "--show-error", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to run curl: {error}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(config.as_bytes())
        .map_err(|error| error.to_string())?;
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

/// A curl config value.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
