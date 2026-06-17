use super::scrape::collect_links;
use super::{get_opt_bool, get_opt_usize, get_opt_str, get_str_arg, text_content};
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::Result;
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
    let mut out: Vec<String> = Vec::new();

    let consider = |url: String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        if out.len() >= limit {
            return;
        }
        if !host_allowed(&url, &base_host, include_subdomains) {
            return;
        }
        if let Some(ref f) = filter
            && !url.to_lowercase().contains(f.as_str())
        {
            return;
        }
        if seen.insert(url.clone()) {
            out.push(url);
        }
    };

    // Parse sitemap.xml if present and not ignored
    if !ignore_sitemap
        && let Ok(origin) = start_url.join("/sitemap.xml")
            && let Ok(page) = fetch_page(origin.as_str(), config).await {
                for loc in parse_sitemap_fast(&page.body) {
                    consider(loc, &mut out, &mut seen);
                }
            }

    // Extract links from the page itself if not sitemap-only
    if !sitemap_only
        && let Ok(page) = fetch_page(start_url.as_str(), config).await {
            for link in collect_links(&page.body, &page.final_url) {
                consider(link, &mut out, &mut seen);
            }
        }

    Ok(text_content(
        if out.is_empty() {
            "No URLs discovered.".to_string()
        } else {
            out.join("\n")
        }
        .as_str(),
    ))
}

fn host_allowed(url: &str, base_host: &str, include_subdomains: bool) -> bool {
    match Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        Some(ref h) if h == base_host => true,
        Some(ref h) if include_subdomains => {
            h.ends_with(&format!(".{base_host}")) || h == base_host
        }
        _ => false,
    }
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
        assert!(host_allowed("https://a.com/x", "a.com", false));
        assert!(!host_allowed("https://b.com/x", "a.com", false));
    }

    #[test]
    fn test_host_allowed_subdomain() {
        assert!(host_allowed("https://sub.a.com/x", "a.com", true));
        assert!(!host_allowed("https://sub.a.com/x", "a.com", false));
    }

    #[test]
    fn test_host_allowed_self_with_subdomain() {
        assert!(host_allowed("https://a.com/x", "a.com", true));
    }

    #[test]
    fn test_host_allowed_different_tld() {
        assert!(!host_allowed("https://a.org/x", "a.com", true));
    }

    #[test]
    fn test_host_allowed_invalid_url() {
        assert!(!host_allowed("not a url", "a.com", false));
    }

    #[test]
    fn test_web_map_requires_url() {
        let config = Config::default();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_map(None, &config));
        assert!(result.is_err());
    }
}
