//! Registry URL validation (M3) and origin comparison (M2, L3, L4).
//!
//! The registry URL is a trust anchor: it decides where tokens are sent and
//! whose signed definitions are accepted. Only an origin is acceptable: https,
//! no userinfo, no path, no query. Plain http is allowed solely for loopback
//! hosts and only in debug builds or with `SCENERY_ALLOW_INSECURE_REGISTRY=1`.

use url::Url;

pub const ALLOW_INSECURE_ENV: &str = "SCENERY_ALLOW_INSECURE_REGISTRY";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistryUrlError {
    #[error("registry URL is not a valid URL: {0}")]
    Parse(String),
    #[error("registry URL must use https (got {0}://)")]
    NotHttps(String),
    #[error(
        "http:// registry URLs are only allowed for loopback hosts, in debug builds or with {ALLOW_INSECURE_ENV}=1"
    )]
    InsecureNotAllowed,
    #[error("registry URL must not contain credentials")]
    HasUserinfo,
    #[error("registry URL must be an origin only (no path, query or fragment)")]
    HasPathOrQuery,
    #[error("registry URL has no host")]
    NoHost,
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn insecure_allowed() -> bool {
    cfg!(debug_assertions) || std::env::var(ALLOW_INSECURE_ENV).as_deref() == Ok("1")
}

/// Validate a registry URL. Returns the parsed URL.
pub fn validate(raw: &str) -> Result<Url, RegistryUrlError> {
    let url = Url::parse(raw.trim()).map_err(|e| RegistryUrlError::Parse(e.to_string()))?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or(RegistryUrlError::NoHost)?
        .to_string();
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RegistryUrlError::HasUserinfo);
    }
    if (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(RegistryUrlError::HasPathOrQuery);
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" => {
            if is_loopback_host(&host) && insecure_allowed() {
                Ok(url)
            } else {
                Err(RegistryUrlError::InsecureNotAllowed)
            }
        }
        other => Err(RegistryUrlError::NotHttps(other.to_string())),
    }
}

/// Canonical origin string (`scheme://host[:port]`, no trailing slash).
pub fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Validate and return the canonical origin string.
pub fn validate_origin(raw: &str) -> Result<String, RegistryUrlError> {
    Ok(origin_of(&validate(raw)?))
}

/// True when both URLs parse and share scheme, host and port exactly.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (Url::parse(a), Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin() == b.origin() && a.origin().is_tuple(),
        _ => false,
    }
}

#[cfg(test)]
#[path = "registry_url_tests.rs"]
mod tests;
