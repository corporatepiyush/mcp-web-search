use super::{get_str_arg, get_str_array, text_content};
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use scraper::{Html, Selector};
use serde_json::Value;
use std::sync::LazyLock;
use url::Url;

static A_HREF: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static MAIN_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());

/// `web_scrape` — fetch a single page over HTTP and return content.
/// HTML parsing and markdown conversion run on the blocking thread pool
/// to prevent CPU-heavy DOM work from stalling the async workers.
pub async fn web_scrape(args: Option<&Value>, config: &Config) -> Result<Value> {
    let url = get_str_arg(args, "url")?;
    let formats = get_str_array(args, "formats").unwrap_or_else(|| vec!["markdown".to_string()]);
    let only_main = super::get_opt_bool(args, "onlyMainContent").unwrap_or(false);

    // Validate formats before doing any IO
    for fmt in &formats {
        match fmt.as_str() {
            "markdown" | "extract" | "html" | "rawHtml" | "links" => {}
            "screenshot" | "screenshot@fullPage" => {
                return Err(WebSearchError::InvalidParams(
                    "screenshot formats require a browser and are not supported".into(),
                ));
            }
            other => {
                return Err(WebSearchError::InvalidParams(format!(
                    "unknown format '{other}'"
                )));
            }
        }
    }

    let page = fetch_page(&url, config).await?;

    let body = page.body;
    let final_url = page.final_url;

    // All CPU-heavy work (HTML parse, DOM query, markdown convert) runs
    // on the blocking pool so async workers stay free for IO.
    let text = tokio::task::spawn_blocking(move || -> Result<String> {
        let html = if only_main {
            extract_main(&body)
        } else {
            body.clone()
        };

        let mut sections: Vec<String> = Vec::with_capacity(formats.len());
        for fmt in &formats {
            match fmt.as_str() {
                "markdown" | "extract" => sections.push(to_markdown(&html)?),
                "html" => sections.push(html.clone()),
                "rawHtml" => sections.push(body.clone()),
                "links" => sections.push(collect_links(&body, &final_url).join("\n")),
                _ => unreachable!(), // validated above
            }
        }
        Ok(sections.join("\n\n"))
    })
    .await
    .map_err(|e| WebSearchError::ProviderError(format!("parse task failed: {e}")))??;

    Ok(text_content(if text.is_empty() {
        "No content found"
    } else {
        &text
    }))
}

// These functions are `pub` because extract.rs calls them via
// `scrape::to_markdown` / `scrape::extract_main` inside spawn_blocking.
// Tests also import them directly.
pub fn to_markdown(html: &str) -> Result<String> {
    htmd::convert(html)
        .map_err(|e| WebSearchError::ProviderError(format!("HTML→markdown failed: {e}")))
}

/// Fetch a URL (SSRF-guarded), isolate its main content, and convert to markdown.
/// CPU-heavy HTML parsing and markdown conversion run on the blocking pool.
pub async fn extract_main_markdown(url: &str, config: &Config) -> Result<String> {
    let page = fetch_page(url, config).await?;
    let body = page.body;
    tokio::task::spawn_blocking(move || to_markdown(&extract_main(&body)))
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("parse task failed: {e}")))?
}

pub fn extract_main(html: &str) -> String {
    let doc = Html::parse_document(html);
    if let Some(main) = doc.select(&MAIN_SEL).next() {
        return main.html();
    }
    html.to_string()
}

pub fn collect_links(html: &str, base: &Url) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for el in doc.select(&A_HREF) {
        if let Some(href) = el.value().attr("href")
            && let Ok(abs) = base.join(href)
        {
            let s = abs.to_string();
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_links_resolves_relative() {
        let base = Url::parse("https://example.com/dir/").unwrap();
        let html = r#"<a href="/abs">a</a><a href="rel">b</a><a href="https://x.com">c</a>"#;
        let links = collect_links(html, &base);
        assert!(links.contains(&"https://example.com/abs".to_string()));
        assert!(links.contains(&"https://example.com/dir/rel".to_string()));
        assert!(links.contains(&"https://x.com/".to_string()));
    }

    #[test]
    fn test_collect_links_dedup() {
        let base = Url::parse("https://example.com/").unwrap();
        let html = r#"<a href="/a">x</a><a href="/a">y</a>"#;
        let links = collect_links(html, &base);
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn test_collect_links_empty() {
        let base = Url::parse("https://example.com/").unwrap();
        let html = "<p>no links</p>";
        let links = collect_links(html, &base);
        assert!(links.is_empty());
    }

    #[test]
    fn test_collect_links_no_duplicates_across_formats() {
        let base = Url::parse("https://example.com/").unwrap();
        let html = r#"<a href="/page">link</a><a href="https://example.com/page">same</a>"#;
        let links = collect_links(html, &base);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0], "https://example.com/page");
    }

    #[test]
    fn test_to_markdown() {
        let md = to_markdown("<h1>Title</h1><p>Body text</p>").unwrap();
        assert!(md.contains("Title"));
        assert!(md.contains("Body text"));
    }

    #[test]
    fn test_to_markdown_nested() {
        let md = to_markdown("<div><h1>A</h1><ul><li>one</li><li>two</li></ul></div>").unwrap();
        assert!(md.contains("A"));
        assert!(md.contains("one"));
        assert!(md.contains("two"));
    }

    #[test]
    fn test_to_markdown_empty() {
        let md = to_markdown("").unwrap();
        assert!(md.is_empty());
    }

    #[test]
    fn test_extract_main_prefers_main_tag() {
        let html = "<body><nav>menu</nav><main><p>real</p></main></body>";
        let extracted = extract_main(html);
        assert!(extracted.contains("real"));
        assert!(!extracted.contains("menu"));
    }

    #[test]
    fn test_extract_main_with_article() {
        let html = "<body><header>top</header><article>content</article></body>";
        let extracted = extract_main(html);
        assert!(extracted.contains("content"));
    }

    #[test]
    fn test_extract_main_role_main() {
        let html = r#"<div role="main">primary</div><div>other</div>"#;
        let extracted = extract_main(html);
        assert!(extracted.contains("primary"));
    }

    #[test]
    fn test_extract_main_fallback() {
        let html = "<html><body>everything</body></html>";
        let extracted = extract_main(html);
        assert!(extracted.contains("everything"));
    }

    #[test]
    fn test_extract_main_empty() {
        assert_eq!(extract_main(""), "");
    }

    #[test]
    fn test_web_scrape_requires_url() {
        let config = Config::default();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(web_scrape(None, &config));
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::InvalidParams(_)
        ));
    }

    #[tokio::test]
    async fn test_web_scrape_rejects_private() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://127.0.0.1:8080/"});
        let result = web_scrape(Some(&args), &config).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            WebSearchError::UrlNotAllowed(_)
        ));
    }

    #[test]
    fn test_collect_links_handles_fragment() {
        let base = Url::parse("https://example.com/page").unwrap();
        let html = concat!("<a href=\"", "#section\">link</a>");
        let links = collect_links(html, &base);
        assert!(links.contains(&"https://example.com/page#section".to_string()));
    }
}
