//! HTTPS GETs restricted to GitHub: every URL, including each redirect, must
//! be HTTPS to an allowed host, redirects are followed by hand up to a
//! limit, and bodies are capped in size and time.

use std::io::Read;
use std::time::{Duration, Instant};

use ureq::http::Uri;

/// GitHub's API, the release download URLs, and the hosts those downloads
/// redirect to (`release-assets` now, `objects` before it).
const GITHUB_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// Longest URL accepted, redirects included; GitHub's signed asset URLs are
/// under 2 KiB.
const MAX_URL: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("refused to contact {0}")]
    Disallowed(String),
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("not found")]
    NotFound,
    #[error("GitHub's rate limit was reached")]
    RateLimited,
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("the response is larger than {0} bytes")]
    TooLarge(u64),
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Io(String),
}

/// Where requests may go and how long they may take.
#[derive(Debug, Clone)]
pub struct Policy {
    hosts: Vec<String>,
    /// Plain HTTP, for the loopback test server only.
    allow_http: bool,
    /// The only port allowed; `None` allows only the default HTTPS port.
    port: Option<u16>,
    max_redirects: u32,
    pub connect_timeout: Duration,
    /// Longest wait for any single read.
    pub read_timeout: Duration,
    proxy_from_env: bool,
}

impl Policy {
    pub fn github() -> Self {
        Self {
            hosts: GITHUB_HOSTS.iter().map(|h| (*h).to_owned()).collect(),
            allow_http: false,
            port: None,
            max_redirects: 5,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            proxy_from_env: true,
        }
    }

    /// Plain HTTP to 127.0.0.1 at `port`, with short timeouts and no proxy.
    #[cfg(test)]
    pub fn loopback(port: u16) -> Self {
        Self {
            hosts: vec!["127.0.0.1".to_owned()],
            allow_http: true,
            port: Some(port),
            max_redirects: 5,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_millis(500),
            proxy_from_env: false,
        }
    }

    /// `url` parsed, if the policy allows contacting it.
    pub fn check(&self, url: &str) -> Result<Uri, HttpError> {
        let disallowed = || HttpError::Disallowed(host_of(url));
        if url.len() > MAX_URL {
            return Err(disallowed());
        }
        let uri: Uri = url.parse().map_err(|_| disallowed())?;
        let scheme_ok = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => self.allow_http,
            _ => false,
        };
        let Some(authority) = uri.authority() else {
            return Err(disallowed());
        };
        let host = authority.host();
        let port_ok = match (self.port, uri.port_u16()) {
            (Some(allowed), port) => port == Some(allowed),
            (None, None) => true,
            (None, Some(port)) => port == 443 && uri.scheme_str() == Some("https"),
        };
        if !scheme_ok
            || !port_ok
            || authority.as_str().contains('@')
            || !self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
        {
            return Err(disallowed());
        }
        Ok(uri)
    }
}

/// The host of `url` for messages, without the rest (signed URLs carry
/// tokens).
fn host_of(url: &str) -> String {
    match url.parse::<Uri>() {
        Ok(uri) => uri.host().unwrap_or("an invalid address").to_owned(),
        Err(_) => "an invalid address".to_owned(),
    }
}

/// Where a redirect from `from` to `location` leads: an absolute URL, or a
/// path on the same origin. Anything else is refused.
fn resolve_redirect(from: &Uri, location: &str) -> Option<String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Some(location.to_owned());
    }
    if location.starts_with('/') && !location.starts_with("//") {
        let scheme = from.scheme_str()?;
        let authority = from.authority()?;
        return Some(format!("{scheme}://{authority}{location}"));
    }
    None
}

/// What one GET may receive.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub accept: &'static str,
    pub max_bytes: u64,
    /// For the whole request, redirects and body included.
    pub total: Duration,
}

pub struct Client {
    agent: ureq::Agent,
    policy: Policy,
}

impl Client {
    pub fn new(policy: Policy) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(!policy.allow_http)
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(if policy.proxy_from_env {
                ureq::Proxy::try_from_env()
            } else {
                None
            })
            .timeout_connect(Some(policy.connect_timeout))
            .timeout_per_read(Some(policy.read_timeout))
            .user_agent(user_agent())
            .tls_config(tls_config())
            .build();
        Self {
            agent: config.new_agent(),
            policy,
        }
    }

    /// The body of `url`, whole.
    pub fn get_bytes(&self, url: &str, limits: Limits) -> Result<Vec<u8>, HttpError> {
        let mut body = Vec::new();
        self.get(url, limits, &mut |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(body)
    }

    /// Streams the body of `url` into `sink` and returns its length. `sink`
    /// is never given more than `limits.max_bytes` in total.
    pub fn get(
        &self,
        url: &str,
        limits: Limits,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<u64, HttpError> {
        let deadline = Instant::now() + limits.total;
        let mut uri = self.policy.check(url)?;
        for _ in 0..=self.policy.max_redirects {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HttpError::Timeout);
            }
            let response = self
                .agent
                .get(uri.to_string())
                .header("Accept", limits.accept)
                .config()
                .timeout_global(Some(remaining))
                .build()
                .call()
                .map_err(map_error)?;
            let status = response.status().as_u16();
            let header = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            };
            match status {
                200 => return read_body(response.into_body(), limits.max_bytes, sink),
                301 | 302 | 303 | 307 | 308 => {
                    let next = header("location")
                        .and_then(|location| resolve_redirect(&uri, &location))
                        .ok_or_else(|| HttpError::Disallowed(host_of(&uri.to_string())))?;
                    uri = self.policy.check(&next)?;
                }
                404 => return Err(HttpError::NotFound),
                429 => return Err(HttpError::RateLimited),
                403 if header("x-ratelimit-remaining").as_deref() == Some("0") => {
                    return Err(HttpError::RateLimited);
                }
                _ => return Err(HttpError::Status(status)),
            }
        }
        Err(HttpError::TooManyRedirects)
    }
}

/// SChannel with the Windows certificate store.
#[cfg(target_os = "windows")]
fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

/// rustls with Mozilla's roots, built in.
#[cfg(not(target_os = "windows"))]
fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::WebPki)
        .build()
}

fn read_body(
    body: ureq::Body,
    max_bytes: u64,
    sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> Result<u64, HttpError> {
    if body
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(HttpError::TooLarge(max_bytes));
    }
    let mut reader = body.into_reader();
    let mut buffer = vec![0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(map_io(e)),
        };
        total += n as u64;
        if total > max_bytes {
            return Err(HttpError::TooLarge(max_bytes));
        }
        sink(&buffer[..n]).map_err(|e| HttpError::Io(e.to_string()))?;
    }
}

fn map_error(e: ureq::Error) -> HttpError {
    match e {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::Io(e) => map_io(e),
        e => HttpError::Network(e.to_string()),
    }
}

/// ureq reports its own errors inside the body reader's I/O errors.
fn map_io(e: std::io::Error) -> HttpError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        return HttpError::Timeout;
    }
    match e.downcast::<ureq::Error>() {
        Ok(inner) => map_error(inner),
        Err(e) => HttpError::Network(e.to_string()),
    }
}

/// Names the app and its version, nothing else.
pub fn user_agent() -> String {
    format!("pcvolumecontrol/{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_policy_allows_only_github_over_https() {
        let policy = Policy::github();
        for ok in [
            "https://api.github.com/repos/PcVolumeControl/PcVolumeControlServer/releases/latest",
            "https://github.com/PcVolumeControl/PcVolumeControlServer/releases/download/v1.2.3/SHA256SUMS",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/abc?sp=r&sig=x",
            "https://objects.githubusercontent.com/github-production-release-asset-2e65be/1",
            "https://github.com:443/x",
        ] {
            assert!(policy.check(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://api.github.com/repos/x",
            "https://raw.githubusercontent.com/x",
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://github.com:8443/x",
            "https://user@github.com/x",
            "https://user:pw@github.com/x",
            "ftp://github.com/x",
            "file:///etc/passwd",
            "github.com/x",
            "/relative",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "",
        ] {
            assert!(
                matches!(policy.check(bad), Err(HttpError::Disallowed(_))),
                "{bad}"
            );
        }
        let long = format!("https://github.com/{}", "a".repeat(MAX_URL));
        assert!(policy.check(&long).is_err());
    }

    #[test]
    fn loopback_policy_is_narrow() {
        let policy = Policy::loopback(4321);
        assert!(policy.check("http://127.0.0.1:4321/x").is_ok());
        assert!(policy.check("http://127.0.0.1:4322/x").is_err());
        assert!(policy.check("http://localhost:4321/x").is_err());
        assert!(policy.check("http://127.0.0.1/x").is_err());
    }

    #[test]
    fn redirects_resolve_to_absolute_urls_or_same_origin_paths() {
        let from: Uri = "https://github.com/a/b".parse().unwrap();
        assert_eq!(
            resolve_redirect(&from, "https://release-assets.githubusercontent.com/x").as_deref(),
            Some("https://release-assets.githubusercontent.com/x")
        );
        assert_eq!(
            resolve_redirect(&from, "/c/d?e=f").as_deref(),
            Some("https://github.com/c/d?e=f")
        );
        assert_eq!(resolve_redirect(&from, "//evil.example/x"), None);
        assert_eq!(resolve_redirect(&from, "c/d"), None);
        assert_eq!(resolve_redirect(&from, "javascript:x"), None);
    }

    #[test]
    fn user_agent_names_only_the_app() {
        assert_eq!(
            user_agent(),
            format!("pcvolumecontrol/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn host_of_hides_the_rest_of_the_url() {
        assert_eq!(
            host_of("https://release-assets.githubusercontent.com/x?sig=secret"),
            "release-assets.githubusercontent.com"
        );
    }
}
