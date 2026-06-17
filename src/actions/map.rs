use super::scrape::collect_links;
use super::{get_opt_bool, get_opt_usize, get_opt_str, get_str_arg, text_content};
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::Result;
use futures::StreamExt;
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use serde_json::Value;
use std::collections::HashSet;
use url::Url;

/// `web_map` — discover URLs reachable from a starting URL, via the site's
/// sitemap.xml and/or in-page HTML links.
pub async fn web_map(args: Option<&Value>, config: &Config) -> Result<Value> {
    let start = get_str_arg(args, "url")?;
    let filter = get_opt_str(args, "search").map(|s| s.to_lowercase());
    let ignore_sitemap = get_opt_bool(args, "ignoreSitemap").unwrap_or(false);
    let sitemap_only = get_opt_bool(args, "sitemapOnly").unwrap_or(false);
    let include_subdomains = get_opt_bool(args, "includeSubdomains").unwrap_or(false);
    let limit = get_opt_usize(args, "limit")
        .unwrap_or(config.server.max_map_urls)
        .min(config.server.max_map_urls);

    let start_url = crate::validation::validate_url(&start, config.allow_private_hosts).await?;
    let base_host = start_url.host_str().unwrap_or_default().to_string();

    let mut seen: HashSet<String> = HashSet::new();
    // Each candidate carries whether it still needs a DNS check: exact-host URLs
    // share the already-validated start host, so they never need re-resolution.
    let mut candidates: Vec<(String, bool)> = Vec::new();

    // Cheap, synchronous filtering only (host scope, substring filter, dedup,
    // limit). DNS resolution is deferred to an async pass below so we never
    // block the runtime on a synchronous lookup per candidate.
    let consider = |url: String, candidates: &mut Vec<(String, bool)>, seen: &mut HashSet<String>| {
        if candidates.len() >= limit {
            return;
        }
        let host = match Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
        {
            Some(h) => h,
            None => return,
        };
        if !host_allowed_host(&host, &base_host, include_subdomains) {
            return;
        }
        if let Some(ref f) = filter
            && !url.to_lowercase().contains(f.as_str())
        {
            return;
        }
        // Only cross-host (subdomain) candidates require a DNS check; an exact
        // host match resolves to the same already-validated start host.
        let needs_dns = host != base_host;
        if seen.insert(url.clone()) {
            candidates.push((url, needs_dns));
        }
    };

    // Parse sitemap.xml if present and not ignored
    if !ignore_sitemap
        && let Ok(origin) = start_url.join("/sitemap.xml")
            && let Ok(page) = fetch_page(origin.as_str(), config).await {
                for loc in parse_sitemap_fast(&page.body) {
                    consider(loc, &mut candidates, &mut seen);
                }
            }

    // Extract links from the page itself if not sitemap-only
    if !sitemap_only
        && let Ok(page) = fetch_page(start_url.as_str(), config).await {
            for link in collect_links(&page.body, &page.final_url) {
                consider(link, &mut candidates, &mut seen);
            }
        }

    // Defense-in-depth: validate cross-host candidates (scheme + async DNS
    // against the SSRF guard) concurrently so internal/unreachable subdomains
    // never appear in the output, without blocking the async worker on DNS.
    // Same-host candidates skip DNS entirely (the start host is already
    // validated), so the common case — listing a single site without
    // includeSubdomains — performs zero extra resolutions and cannot be turned
    // into a DNS-amplification vector via a large hostile sitemap.
    let allow_private = config.allow_private_hosts;
    let concurrency = (*crate::config::CPU_COUNT * 2).max(8);
    let out: Vec<String> = futures::stream::iter(candidates)
        .map(|(u, needs_dns)| async move {
            if !needs_dns {
                return Some(u);
            }
            match crate::validation::validate_url(&u, allow_private).await {
                Ok(_) => Some(u),
                Err(_) => None,
            }
        })
        .buffered(concurrency)
        .filter_map(|x| async move { x })
        .collect()
        .await;

    Ok(text_content(
        if out.is_empty() {
            "No URLs discovered.".to_string()
        } else {
            out.join("\n")
        }
        .as_str(),
    ))
}

/// Whether a host is in scope for the map: the exact start host, or (when
/// enabled) a subdomain of it.
fn host_allowed_host(host: &str, base_host: &str, include_subdomains: bool) -> bool {
    host == base_host || (include_subdomains && host.ends_with(&format!(".{base_host}")))
}

/// Parse `<loc>` elements from a sitemap.xml using a proper streaming XML parser.
/// This is immune to the naive string-search vulnerabilities (e.g. `loc>` in
/// CDATA, comments, or attribute values).
fn parse_sitemap_fast(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut in_loc = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"loc" {
                    in_loc = true;
                }
            }
            Ok(Event::Text(ref e)) if in_loc => {
                if let Ok(text) = e.unescape() {
                    let s = text.trim();
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"loc" {
                    in_loc = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sitemap_simple() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url><loc>https://a.com/1</loc></url>
            <url><loc>https://a.com/2</loc></url>
        </urlset>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://a.com/1", "https://a.com/2"]);
    }

    #[test]
    fn test_parse_sitemap_sitemapindex() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <sitemapindex xmlns="http://www.sitemaps.org/schemas/siteindex/0.9">
            <sitemap><loc>https://a.com/sitemap1.xml</loc></sitemap>
            <sitemap><loc>https://a.com/sitemap2.xml</loc></sitemap>
        </sitemapindex>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(
            locs,
            vec![
                "https://a.com/sitemap1.xml",
                "https://a.com/sitemap2.xml"
            ]
        );
    }

    #[test]
    fn test_parse_sitemap_with_namespace() {
        let xml = r#"<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
            <url><loc>https://x.com/page</loc></url>
        </urlset>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://x.com/page"]);
    }

    #[test]
    fn test_parse_sitemap_empty() {
        assert!(parse_sitemap_fast("").is_empty());
    }

    #[test]
    fn test_parse_sitemap_no_locs() {
        let xml = "<root><foo>bar</foo></root>";
        assert!(parse_sitemap_fast(xml).is_empty());
    }

    #[test]
    fn test_parse_sitemap_ignores_loc_in_comments() {
        // The naive string-search approach would find this "loc" in a comment.
        let xml = r#"<urlset><!-- <loc>https://evil.com/</loc> --><url><loc>https://good.com/</loc></url></urlset>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://good.com/"]);
    }

    #[test]
    fn test_parse_sitemap_ignores_loc_in_attributes() {
        // loc appearing in an attribute value should not be parsed.
        let xml = r#"<urlset><url><loc data-x="loc">https://real.com/</loc></url></urlset>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://real.com/"]);
    }

    #[test]
    fn test_parse_sitemap_malformed() {
        let xml = r#"<urlset><url><loc>https://good.com"#; // Missing closing tags
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://good.com"]);
    }

    #[test]
    fn test_parse_sitemap_entity() {
        let xml = r#"<urlset><url><loc>https://a.com/path?q=1&amp;r=2</loc></url></urlset>"#;
        let locs = parse_sitemap_fast(xml);
        assert_eq!(locs, vec!["https://a.com/path?q=1&r=2"]);
    }

    #[test]
    fn test_host_allowed_exact() {
        assert!(host_allowed_host("a.com", "a.com", false));
        assert!(!host_allowed_host("b.com", "a.com", false));
    }

    // The same-host short-circuit (#Important fix): exact-host candidates skip
    // DNS (needs_dns == false); subdomains still require a resolution.
    #[test]
    fn test_host_allowed_host_and_dns_need() {
        // exact host: in scope, and would not need a DNS re-check
        assert!(host_allowed_host("a.com", "a.com", false));
        assert!(host_allowed_host("a.com", "a.com", true));
        // subdomain: only in scope with includeSubdomains, and needs DNS
        assert!(!host_allowed_host("sub.a.com", "a.com", false));
        assert!(host_allowed_host("sub.a.com", "a.com", true));
        // unrelated host never in scope
        assert!(!host_allowed_host("evil.com", "a.com", true));
        // a host that merely ends with the base string but isn't a subdomain
        assert!(!host_allowed_host("nota.com", "a.com", true));
    }

    #[test]
    fn test_host_allowed_subdomain() {
        assert!(host_allowed_host("sub.a.com", "a.com", true));
        assert!(!host_allowed_host("sub.a.com", "a.com", false));
    }

    #[test]
    fn test_host_allowed_self_with_subdomain() {
        assert!(host_allowed_host("a.com", "a.com", true));
    }

    #[test]
    fn test_host_allowed_different_tld() {
        assert!(!host_allowed_host("a.org", "a.com", true));
    }

    #[test]
    fn test_web_map_requires_url() {
        let config = Config::default();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_map(None, &config));
        assert!(result.is_err());
    }

    // EXPLOIT REGRESSION (#6): a private/internal start URL is rejected up front
    // (and the candidate-validation pass that follows is async, not a blocking
    // DNS call per URL on the runtime).
    #[tokio::test]
    async fn test_web_map_rejects_private_start() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://169.254.169.254/"});
        let result = web_map(Some(&args), &config).await;
        assert!(matches!(
            result.unwrap_err(),
            crate::errors::WebSearchError::UrlNotAllowed(_)
        ));
    }
}
