use super::types::{SearchRequest, SearchResponse, SearchResult};
use crate::client::HTTP;
use crate::errors::{Result, WebSearchError};
use scraper::{Html, Selector};
use std::sync::LazyLock;
use std::time::Duration;
use url::Url;

const HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

static RESULT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("div.result").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.result__a").unwrap());
static SNIPPET_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.result__snippet").unwrap());

/// DuckDuckGo via the keyless HTML endpoint, parsed server-side.
pub async fn search(req: &SearchRequest) -> Result<SearchResponse> {
    if req.query.trim().is_empty() {
        return Err(WebSearchError::InvalidParams("query cannot be empty".into()));
    }

    let kp = match req.safe_search {
        2 => "1",
        1 => "-1",
        _ => "-2",
    };

    let resp = tokio::time::timeout(
        Duration::from_millis(req.timeout_ms),
        HTTP.get(HTML_ENDPOINT)
            .query(&[("q", req.query.as_str()), ("kp", kp)])
            .send(),
    )
    .await
    .map_err(|_| WebSearchError::Timeout("DuckDuckGo search timeout".into()))?
    .map_err(|e| WebSearchError::ProviderError(format!("DuckDuckGo request failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(WebSearchError::ProviderError(format!(
            "DuckDuckGo returned HTTP {}",
            resp.status()
        )));
    }

    let html = resp
        .text()
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("DuckDuckGo read failed: {e}")))?;

    let limit = req.limit;
    let results = tokio::task::spawn_blocking(move || parse_results(&html, limit))
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("parse task failed: {e}")))?
        .into_iter()
        .collect::<Vec<_>>();

    Ok(SearchResponse {
        success: true,
        results,
    })
}

fn parse_results(html: &str, limit: usize) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for node in doc.select(&RESULT_SEL) {
        if out.len() >= limit {
            break;
        }
        let Some(link) = node.select(&TITLE_SEL).next() else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        let href = link.value().attr("href").unwrap_or_default();
        let url = decode_redirect(href);
        if url.is_empty() {
            continue;
        }
        let snippet = node
            .select(&SNIPPET_SEL)
            .next()
            .map(|s| s.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        out.push(SearchResult::new(title, url, snippet, "duckduckgo"));
    }
    out
}

fn decode_redirect(href: &str) -> String {
    let absolute = if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = Url::parse(&absolute)
        && let Some((_, val)) = parsed.query_pairs().find(|(k, _)| k == "uddg")
    {
        return val.into_owned();
    }
    if absolute.starts_with("http") {
        absolute
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(decode_redirect(href), "https://example.com/page");
    }

    #[test]
    fn test_decode_redirect_no_uddg() {
        let href = "//duckduckgo.com/l/?rut=abc";
        assert_eq!(decode_redirect(href), "https://duckduckgo.com/l/?rut=abc");
    }

    #[test]
    fn test_decode_plain_http() {
        assert_eq!(
            decode_redirect("https://rust-lang.org"),
            "https://rust-lang.org"
        );
    }

    #[test]
    fn test_decode_redirect_malformed() {
        assert_eq!(decode_redirect(""), "");
        assert_eq!(decode_redirect("javascript:void(0)"), "");
    }

    #[test]
    fn test_parse_results() {
        let html = r#"
        <div class="result">
            <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org">The Rust Language</a>
            <a class="result__snippet">A systems language.</a>
        </div>"#;
        let r = parse_results(html, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "The Rust Language");
        assert_eq!(r[0].url, "https://rust-lang.org");
        assert_eq!(r[0].snippet, "A systems language.");
        assert_eq!(r[0].engine, "duckduckgo");
    }

    #[test]
    fn test_parse_results_limit() {
        let html = r#"
        <div class="result"><a class="result__a" href="https://a.com">A</a></div>
        <div class="result"><a class="result__a" href="https://b.com">B</a></div>
        <div class="result"><a class="result__a" href="https://c.com">C</a></div>"#;
        let r = parse_results(html, 2);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn test_parse_results_no_results() {
        let html = "<html><body>no results</body></html>";
        let r = parse_results(html, 10);
        assert!(r.is_empty());
    }
}
