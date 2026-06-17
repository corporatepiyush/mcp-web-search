use crate::errors::{Result, WebSearchError};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use url::{Host, Url};

/// Validate a user-supplied URL before fetching it, guarding against SSRF.
/// Uses async DNS via tokio — does not block the async worker thread.
pub async fn validate_url(raw: &str, allow_private: bool) -> Result<Url> {
    let url = parse_and_check_scheme_host(raw)?;
    if allow_private {
        return Ok(url);
    }
    check_host_async(&url).await?;
    Ok(url)
}

/// Synchronous validation for use in tests or sync contexts.
/// May block the calling thread on DNS — prefer `validate_url` in async code.
pub fn validate_url_blocking(raw: &str, allow_private: bool) -> Result<Url> {
    let url = parse_and_check_scheme_host(raw)?;
    if allow_private {
        return Ok(url);
    }
    check_host_blocking(&url)?;
    Ok(url)
}

fn parse_and_check_scheme_host(raw: &str) -> Result<Url> {
    let url = Url::parse(raw)
        .map_err(|e| WebSearchError::UrlNotAllowed(format!("invalid URL '{raw}': {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(WebSearchError::UrlNotAllowed(format!(
                "scheme '{other}' is not allowed (only http/https)"
            )));
        }
    }

    url.host()
        .ok_or_else(|| WebSearchError::UrlNotAllowed(format!("URL '{raw}' has no host")))?;

    Ok(url)
}

async fn check_host_async(url: &Url) -> Result<()> {
    match url.host() {
        Some(Host::Ipv4(ip)) => guard_ipv4(ip),
        Some(Host::Ipv6(ip)) => guard_ipv6(ip),
        Some(Host::Domain(domain)) => {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((domain, 0u16))
                .await
                .map_err(|e| {
                    WebSearchError::UrlNotAllowed(format!("cannot resolve host '{domain}': {e}"))
                })?
                .collect();
            check_addrs(addrs.into_iter(), domain)
        }
        None => Ok(()),
    }
}

fn check_host_blocking(url: &Url) -> Result<()> {
    match url.host() {
        Some(Host::Ipv4(ip)) => guard_ipv4(ip),
        Some(Host::Ipv6(ip)) => guard_ipv6(ip),
        Some(Host::Domain(domain)) => {
            let addrs = (domain, 0u16)
                .to_socket_addrs()
                .map_err(|e| {
                    WebSearchError::UrlNotAllowed(format!("cannot resolve host '{domain}': {e}"))
                })?;
            check_addrs(addrs, domain)
        }
        None => Ok(()),
    }
}

fn check_addrs(
    addrs: impl Iterator<Item = std::net::SocketAddr>,
    domain: &str,
) -> Result<()> {
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        match addr.ip() {
            IpAddr::V4(ip) => guard_ipv4(ip)?,
            IpAddr::V6(ip) => guard_ipv6(ip)?,
        }
    }
    if !saw_any {
        return Err(WebSearchError::UrlNotAllowed(format!(
            "host '{domain}' resolved to no addresses"
        )));
    }
    Ok(())
}

#[inline]
fn guard_ipv4(ip: Ipv4Addr) -> Result<()> {
    let octets = ip.octets();
    let blocked = ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || octets[0] == 0
        // Carrier-grade NAT 100.64.0.0/10
        || (octets[0] == 100 && (octets[1] & 0xC0) == 0x40)
        // Benchmarking 198.18.0.0/15
        || (octets[0] == 198 && (octets[1] & 0xFE) == 0x12)
        // TEST-NET 198.51.100.0/24, 203.0.113.0/24
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113);
    if blocked {
        return Err(WebSearchError::UrlNotAllowed(format!(
            "address {ip} is in a blocked (private/internal) range; pass --allow-private-hosts to override"
        )));
    }
    Ok(())
}

#[inline]
fn guard_ipv6(ip: Ipv6Addr) -> Result<()> {
    let segments = ip.segments();
    // Unique Local Address (ULA) fc00::/7
    let is_ula = (segments[0] & 0xFE00) == 0xFC00;
    // Link-local fe80::/10
    let is_link_local = (segments[0] & 0xFFC0) == 0xFE80;
    let blocked = ip.is_loopback() || ip.is_unspecified() || is_ula || is_link_local;
    if blocked {
        return Err(WebSearchError::UrlNotAllowed(format!(
            "address {ip} is in a blocked (private/internal) range; pass --allow-private-hosts to override"
        )));
    }
    // Reject IPv4-mapped addresses whose embedded v4 is internal.
    if let Some(v4) = ip.to_ipv4_mapped() {
        guard_ipv4(v4)?;
    }
    Ok(())
}

/// Validate an already-resolved IP address against the SSRF guard.
/// Returns `Ok(())` if the address is public (or `allow_private` is set),
/// and `WebSearchError::UrlNotAllowed` otherwise.
pub fn validate_ip(ip: IpAddr, allow_private: bool) -> Result<()> {
    if allow_private {
        return Ok(());
    }
    match ip {
        IpAddr::V4(v4) => guard_ipv4(v4),
        IpAddr::V6(v6) => guard_ipv6(v6),
    }
}

/// Check whether a URL resolves to a blocked (internal) address **without**
/// returning an error. Returns `true` if the host is safe to connect to.
/// This is a lighter-weight check used for logging/warnings when the actual
/// connection must be allowed (e.g. for self-hosted SearXNG).
#[must_use]
pub fn is_safe_host(raw: &str) -> bool {
    validate_url_blocking(raw, false).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_scheme() {
        assert!(validate_url_blocking("file:///etc/passwd", false).is_err());
        assert!(validate_url_blocking("ftp://example.com", false).is_err());
        assert!(validate_url_blocking("data:text/plain,hello", false).is_err());
        assert!(validate_url_blocking("javascript:alert(1)", false).is_err());
    }

    #[test]
    fn rejects_loopback_and_metadata() {
        assert!(validate_url_blocking("http://127.0.0.1/", false).is_err());
        assert!(validate_url_blocking("http://127.0.0.2/", false).is_err());
        assert!(validate_url_blocking("http://169.254.169.254/latest/meta-data/", false).is_err());
        assert!(validate_url_blocking("http://10.0.0.5/", false).is_err());
        assert!(validate_url_blocking("http://172.16.0.1/", false).is_err());
        assert!(validate_url_blocking("http://192.168.1.1/", false).is_err());
        assert!(validate_url_blocking("http://[::1]/", false).is_err());
        assert!(validate_url_blocking("http://0.0.0.0/", false).is_err());
    }

    #[test]
    fn rejects_cgnat_and_benchmark() {
        assert!(validate_url_blocking("http://100.64.0.1/", false).is_err());
        assert!(validate_url_blocking("http://100.127.255.255/", false).is_err());
        assert!(validate_url_blocking("http://198.18.0.1/", false).is_err());
        assert!(validate_url_blocking("http://198.19.255.255/", false).is_err());
    }

    #[test]
    fn rejects_test_nets() {
        assert!(validate_url_blocking("http://198.51.100.1/", false).is_err());
        assert!(validate_url_blocking("http://203.0.113.1/", false).is_err());
    }

    #[test]
    fn rejects_zero_octet() {
        assert!(validate_url_blocking("http://0.42.42.42/", false).is_err());
    }

    #[test]
    fn allows_private_when_opted_in() {
        assert!(validate_url_blocking("http://127.0.0.1:8080/search", true).is_ok());
        assert!(validate_url_blocking("http://192.168.1.1/", true).is_ok());
        assert!(validate_url_blocking("http://10.0.0.1/", true).is_ok());
    }

    #[test]
    fn accepts_public_ip() {
        assert!(validate_url_blocking("https://1.1.1.1/", false).is_ok());
        assert!(validate_url_blocking("https://8.8.8.8/", false).is_ok());
        assert!(validate_url_blocking("https://93.184.216.34/", false).is_ok());
    }

    #[test]
    fn rejects_missing_host() {
        assert!(validate_url_blocking("http:///path", false).is_err());
    }

    #[test]
    fn accepts_public_domain() {
        assert!(validate_url_blocking("https://example.com/", false).is_ok());
        assert!(validate_url_blocking("https://rust-lang.org/", false).is_ok());
    }

    #[test]
    fn rejects_ula_ipv6() {
        assert!(validate_url_blocking("http://[fc00::1]/", false).is_err());
        assert!(validate_url_blocking("http://[fd00::1]/", false).is_err());
    }

    #[test]
    fn rejects_link_local_ipv6() {
        assert!(validate_url_blocking("http://[fe80::1]/", false).is_err());
    }

    #[test]
    fn rejects_ipv4_mapped_private() {
        assert!(validate_url_blocking("http://[::ffff:127.0.0.1]/", false).is_err());
        assert!(validate_url_blocking("http://[::ffff:192.168.1.1]/", false).is_err());
    }

    #[test]
    fn accepts_ipv4_mapped_public() {
        assert!(validate_url_blocking("http://[::ffff:1.2.3.4]/", false).is_ok());
    }

    #[test]
    fn test_is_safe_host() {
        assert!(is_safe_host("https://example.com"));
        assert!(!is_safe_host("http://127.0.0.1/"));
        assert!(!is_safe_host("http://169.254.169.254/"));
    }

    #[test]
    fn rejects_domain_resolving_to_private() {
        if let Ok(url) = validate_url_blocking("http://127.0.0.1.nip.io/", false) {
            panic!("Expected nip.io loopback to be blocked, got ok: {url}");
        }
    }
}
