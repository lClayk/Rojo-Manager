//! HTTPS to GitHub and Roblox through Windows' own TLS (schannel), with one shared agent.

use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

#[derive(Debug)]
pub enum HttpsError {
    /// The server answered with a non-2xx status.
    Status(u16),
    /// DNS, connection, TLS, timeout, or an unreadable body.
    Transport(String),
}

impl std::fmt::Display for HttpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpsError::Status(code) => write!(f, "HTTP {code}"),
            HttpsError::Transport(message) => f.write_str(message),
        }
    }
}

impl From<ureq::Error> for HttpsError {
    fn from(error: ureq::Error) -> Self {
        HttpsError::Transport(error.to_string())
    }
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent("RojoManager")
            .http_status_as_error(false)
            // Trust the Windows certificate store, like 0.5, rather than ureq's bundled roots.
            .tls_config(
                TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build(),
            )
            .build()
            .into()
    })
}

fn get(url: &str, accept: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, HttpsError> {
    let response = agent().get(url).header("Accept", accept).config().timeout_global(Some(timeout)).build().call()?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(HttpsError::Status(status));
    }
    Ok(response)
}

/// GET a JSON document (at most 1 MB).
pub fn get_json(url: &str, accept: &str, timeout: Duration) -> Result<Value, HttpsError> {
    let mut response = get(url, accept, timeout)?;
    let body = response.body_mut().with_config().limit(1 << 20).read_to_vec()?;
    serde_json::from_slice(&body).map_err(|error| HttpsError::Transport(format!("Invalid JSON: {error}")))
}

/// The URL a GET ends at after redirects, e.g. GitHub's /releases/latest page.
pub fn final_url(url: &str, timeout: Duration) -> Result<String, HttpsError> {
    use ureq::ResponseExt;
    let response = get(url, "text/html", timeout)?;
    Ok(response.get_uri().to_string())
}

/// Stream a download to `destination`.
pub fn download(url: &str, destination: &Path, timeout: Duration) -> Result<(), HttpsError> {
    let response = get(url, "application/octet-stream", timeout)?;
    let mut reader = response.into_body().into_reader();
    let mut file = std::fs::File::create(destination).map_err(|e| HttpsError::Transport(e.to_string()))?;
    io::copy(&mut reader.by_ref(), &mut file).map_err(|e| HttpsError::Transport(e.to_string()))?;
    Ok(())
}
