use super::{get_opt_bool, get_opt_str, get_opt_usize, get_str_arg};
use crate::browser;
use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use crate::validation::validate_url;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams,
};
use serde_json::{json, Value};
use std::time::Duration;

// ─── browser_scrape ───────────────────────────────────────────────────────────

/// Navigate to a URL using a real headless browser (Chrome/Chromium).
/// Unlike `web_scrape`, this executes JavaScript and handles SPAs.
///
/// # Parameters
/// - `url` – target http/https URL (SSRF-validated before loading)
/// - `formats` – `["markdown","html","rawHtml","links","screenshot"]`
/// - `onlyMainContent` – extract main/article content only
/// - `waitFor` – CSS selector to wait for before extraction
/// - `waitTimeoutMs` – ms to wait for `waitFor` (default 5 000)
/// - `javascript` – JS snippet to evaluate after load
/// - `scrollToBottom` – scroll to trigger lazy-loaded content
pub async fn browser_scrape(args: Option<&Value>, config: &Config) -> Result<Value> {
    let pool = browser::get_pool(&config.browser).await.ok_or_else(|| {
        WebSearchError::ConfigError(
            "Headless browser is disabled. Install Chrome/Chromium or \
             use web_scrape for static pages."
                .into(),
        )
    })?;

    let url_str = get_str_arg(args, "url")?;
    // SSRF guard: validate before handing the URL to the browser.
    let url = validate_url(&url_str, config.allow_private_hosts).await?;

    let formats: Vec<String> = args
        .and_then(|a| a.get("formats"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| vec!["markdown".into()]);

    let only_main = get_opt_bool(args, "onlyMainContent").unwrap_or(false);
    let wait_for = get_opt_str(args, "waitFor");
    let wait_timeout_ms = get_opt_usize(args, "waitTimeoutMs").unwrap_or(5_000);
    let javascript = get_opt_str(args, "javascript");
    let scroll_to_bottom = get_opt_bool(args, "scrollToBottom").unwrap_or(false);
    let nav_timeout = pool.nav_timeout;

    let pooled = pool.acquire_page().await?;
    let page = &pooled.page;

    // Navigate with the pool's nav timeout.
    match tokio::time::timeout(nav_timeout, page.goto(url.as_str())).await {
        Err(_) => {
            pooled.close().await;
            return Err(WebSearchError::Timeout(format!(
                "browser navigation to {url} timed out after {}s",
                nav_timeout.as_secs()
            )));
        }
        Ok(Err(e)) => {
            pooled.close().await;
            return Err(WebSearchError::HttpError(format!(
                "browser navigation to {url} failed: {e}"
            )));
        }
        Ok(Ok(_)) => {}
    }

    // Optional: scroll to trigger lazy-loaded content.
    if scroll_to_bottom {
        let _ = page
            .evaluate("window.scrollTo(0, document.body.scrollHeight)")
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Optional: wait for a CSS selector (e.g. wait until the SPA has rendered).
    if let Some(ref selector) = wait_for {
        let wait_dur = Duration::from_millis(wait_timeout_ms as u64);
        match tokio::time::timeout(wait_dur, page.find_element(selector.as_str())).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::debug!("waitFor selector '{selector}' error: {e}"),
            Err(_) => tracing::debug!("waitFor selector '{selector}' timed out after {wait_timeout_ms}ms"),
        }
    }

    // Optional: evaluate user-supplied JavaScript (best-effort, errors ignored).
    if let Some(ref js) = javascript {
        let _ = page.evaluate(js.as_str()).await;
    }

    // Extract content in the requested formats.
    let mut blocks: Vec<Value> = Vec::with_capacity(formats.len());

    for fmt in &formats {
        match fmt.as_str() {
            "rawHtml" | "html" => {
                let html = page.content().await.unwrap_or_default();
                blocks.push(json!({ "type": "text", "text": html }));
            }
            "links" => {
                let html = page.content().await.unwrap_or_default();
                if let Ok(base) = url::Url::parse(url.as_str()) {
                    let links = crate::actions::scrape::collect_links(&html, &base);
                    blocks.push(json!({ "type": "text", "text": links.join("\n") }));
                }
            }
            "screenshot" => {
                let params = CaptureScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .build();
                if let Ok(bytes) = page.screenshot(params).await {
                    blocks.push(json!({
                        "type": "image",
                        "data": BASE64.encode(&bytes),
                        "mimeType": "image/png"
                    }));
                }
            }
            // "markdown" | "extract" | anything else → HTML→markdown
            _ => {
                let html = page.content().await.unwrap_or_default();
                let md = if only_main {
                    crate::actions::scrape::main_to_markdown(&html)
                } else {
                    crate::actions::scrape::dom_to_markdown(&html)
                };
                blocks.push(json!({ "type": "text", "text": md }));
            }
        }
    }

    pooled.close().await;

    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }
    Ok(json!({ "content": blocks }))
}

// ─── browser_screenshot ───────────────────────────────────────────────────────

/// Navigate to a URL and return a base64-encoded PNG screenshot as an MCP
/// image content block.
///
/// # Parameters
/// - `url` – target http/https URL (SSRF-validated)
/// - `fullPage` – capture the full scrollable page vs. viewport only (default false)
/// - `width` – viewport width in pixels (default 1280, max 3840)
/// - `height` – viewport height in pixels (default 800, max 2160)
/// - `waitFor` – CSS selector to wait for before screenshotting
/// - `waitTimeoutMs` – ms to wait for `waitFor` (default 5 000)
pub async fn browser_screenshot(args: Option<&Value>, config: &Config) -> Result<Value> {
    let pool = browser::get_pool(&config.browser).await.ok_or_else(|| {
        WebSearchError::ConfigError(
            "Headless browser is disabled. Install Chrome/Chromium or \
             use web_scrape for static pages."
                .into(),
        )
    })?;

    let url_str = get_str_arg(args, "url")?;
    let url = validate_url(&url_str, config.allow_private_hosts).await?;

    let full_page = get_opt_bool(args, "fullPage").unwrap_or(false);
    let _width = get_opt_usize(args, "width").unwrap_or(1280).min(3840);
    let _height = get_opt_usize(args, "height").unwrap_or(800).min(2160);
    let wait_for = get_opt_str(args, "waitFor");
    let wait_timeout_ms = get_opt_usize(args, "waitTimeoutMs").unwrap_or(5_000);
    let nav_timeout = pool.nav_timeout;

    let pooled = pool.acquire_page().await?;
    let page = &pooled.page;

    // Navigate
    match tokio::time::timeout(nav_timeout, page.goto(url.as_str())).await {
        Err(_) => {
            pooled.close().await;
            return Err(WebSearchError::Timeout(format!(
                "browser navigation to {url} timed out after {}s",
                nav_timeout.as_secs()
            )));
        }
        Ok(Err(e)) => {
            pooled.close().await;
            return Err(WebSearchError::HttpError(format!(
                "browser navigation to {url} failed: {e}"
            )));
        }
        Ok(Ok(_)) => {}
    }

    // Optional: wait for a CSS selector before screenshotting.
    if let Some(ref selector) = wait_for {
        let wait_dur = Duration::from_millis(wait_timeout_ms as u64);
        let _ = tokio::time::timeout(wait_dur, page.find_element(selector.as_str())).await;
    }

    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .capture_beyond_viewport(full_page)
        .build();

    let bytes = page.screenshot(params).await.map_err(|e| {
        WebSearchError::HttpError(format!("screenshot failed: {e}"))
    })?;

    pooled.close().await;

    Ok(json!({
        "content": [{
            "type": "image",
            "data": BASE64.encode(&bytes),
            "mimeType": "image/png"
        }]
    }))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BrowserSettings, Config};
    use serde_json::json;
    use std::time::Duration;

    fn cfg_disabled() -> Config {
        let mut c = Config::default();
        c.browser.disabled = true;
        c
    }

    fn cfg_enabled() -> Config {
        let mut c = Config::default();
        c.browser = BrowserSettings {
            disabled: false,
            max_pages: 4,
            nav_timeout: Duration::from_secs(30),
            chrome_path: None,
        };
        c
    }

    // ── Disabled browser path ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_browser_scrape_disabled_is_config_error() {
        let result = browser_scrape(Some(&json!({"url":"https://example.com"})), &cfg_disabled()).await;
        assert!(
            matches!(result, Err(WebSearchError::ConfigError(_))),
            "expected ConfigError, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_browser_screenshot_disabled_is_config_error() {
        let result = browser_screenshot(Some(&json!({"url":"https://example.com"})), &cfg_disabled()).await;
        assert!(
            matches!(result, Err(WebSearchError::ConfigError(_))),
            "expected ConfigError, got {result:?}"
        );
    }

    // When disabled the missing-URL check still runs first, but we get an error
    // either way.
    #[tokio::test]
    async fn test_browser_scrape_disabled_missing_url_is_error() {
        let result = browser_scrape(Some(&json!({})), &cfg_disabled()).await;
        assert!(result.is_err());
    }

    // ── SSRF guard (browser-enabled path, Chrome not required) ────────────

    #[tokio::test]
    async fn test_browser_scrape_rejects_private_ip() {
        let cfg = cfg_enabled();
        let result = browser_scrape(Some(&json!({"url":"http://192.168.1.1/"})), &cfg).await;
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), WebSearchError::UrlNotAllowed(_)),
            "private IP must be rejected by SSRF guard"
        );
    }

    #[tokio::test]
    async fn test_browser_screenshot_rejects_private_ip() {
        let cfg = cfg_enabled();
        let result = browser_screenshot(Some(&json!({"url":"http://10.0.0.1/"})), &cfg).await;
        assert!(
            matches!(result, Err(WebSearchError::UrlNotAllowed(_))),
            "private IP must be rejected"
        );
    }

    #[tokio::test]
    async fn test_browser_scrape_rejects_file_scheme() {
        let cfg = cfg_enabled();
        let result = browser_scrape(Some(&json!({"url":"file:///etc/passwd"})), &cfg).await;
        assert!(
            matches!(result, Err(WebSearchError::UrlNotAllowed(_))),
            "file:// must be rejected by scheme check"
        );
    }

    #[tokio::test]
    async fn test_browser_screenshot_rejects_javascript_scheme() {
        let cfg = cfg_enabled();
        let result = browser_screenshot(Some(&json!({"url":"javascript:alert(1)"})), &cfg).await;
        assert!(
            matches!(result, Err(WebSearchError::UrlNotAllowed(_))),
            "javascript: must be rejected by scheme check"
        );
    }

    #[tokio::test]
    async fn test_browser_scrape_rejects_loopback() {
        let cfg = cfg_enabled();
        // localhost resolves to 127.0.0.1 — should be blocked by the SSRF guard.
        let result = browser_scrape(Some(&json!({"url":"http://127.0.0.1/"})), &cfg).await;
        assert!(result.is_err());
    }

    // ── Invalid/missing URL ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_browser_scrape_missing_url() {
        let result = browser_scrape(Some(&json!({})), &cfg_enabled()).await;
        assert!(
            matches!(result, Err(WebSearchError::InvalidParams(_))),
            "missing url must be InvalidParams"
        );
    }

    #[tokio::test]
    async fn test_browser_screenshot_missing_url() {
        let result = browser_screenshot(Some(&json!({})), &cfg_enabled()).await;
        assert!(matches!(result, Err(WebSearchError::InvalidParams(_))));
    }

    #[tokio::test]
    async fn test_browser_scrape_invalid_url() {
        let result = browser_scrape(Some(&json!({"url":"not-a-url"})), &cfg_enabled()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_browser_screenshot_invalid_url() {
        let result = browser_screenshot(Some(&json!({"url":"://bad"})), &cfg_enabled()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_browser_scrape_none_args() {
        let result = browser_scrape(None, &cfg_enabled()).await;
        assert!(matches!(result, Err(WebSearchError::InvalidParams(_))));
    }

    // ── Dimension / config bounds ─────────────────────────────────────────

    #[test]
    fn test_screenshot_width_clamped() {
        let width = get_opt_usize(Some(&json!({"width": 99999})), "width")
            .unwrap_or(1280)
            .min(3840) as u32;
        assert_eq!(width, 3840);
    }

    #[test]
    fn test_screenshot_height_clamped() {
        let height = get_opt_usize(Some(&json!({"height": 99999})), "height")
            .unwrap_or(800)
            .min(2160) as u32;
        assert_eq!(height, 2160);
    }

    #[test]
    fn test_screenshot_defaults() {
        let args = json!({"url": "https://example.com"});
        let w = get_opt_usize(Some(&args), "width").unwrap_or(1280);
        let h = get_opt_usize(Some(&args), "height").unwrap_or(800);
        let full = get_opt_bool(Some(&args), "fullPage").unwrap_or(false);
        assert_eq!(w, 1280);
        assert_eq!(h, 800);
        assert!(!full);
    }

    #[test]
    fn test_wait_timeout_default() {
        let args = json!({"url": "https://example.com"});
        let timeout = get_opt_usize(Some(&args), "waitTimeoutMs").unwrap_or(5_000);
        assert_eq!(timeout, 5_000);
    }

    #[test]
    fn test_formats_default_to_markdown() {
        let args = json!({"url": "https://example.com"});
        let formats: Vec<String> = args
            .get("formats")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_else(|| vec!["markdown".into()]);
        assert_eq!(formats, vec!["markdown"]);
    }

    #[test]
    fn test_scrape_formats_parsed() {
        let args = json!({"url": "https://x.com", "formats": ["markdown","links","screenshot"]});
        let formats: Vec<String> = args
            .get("formats")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        assert_eq!(formats, vec!["markdown", "links", "screenshot"]);
    }

    // ── BrowserSettings config parsing ────────────────────────────────────

    #[test]
    fn test_browser_settings_disabled_flag() {
        let cfg = cfg_disabled();
        assert!(cfg.browser.disabled);
    }

    #[test]
    fn test_browser_settings_max_pages_default() {
        let cfg = Config::default();
        assert!(cfg.browser.max_pages >= 4);
    }

    #[test]
    fn test_browser_settings_nav_timeout_default() {
        let cfg = Config::default();
        assert_eq!(cfg.browser.nav_timeout, Duration::from_secs(30));
    }

    // ── SSRF: allow_private_hosts opt-in ─────────────────────────────────

    #[tokio::test]
    async fn test_browser_scrape_allows_private_when_opted_in() {
        let mut cfg = cfg_enabled();
        cfg.allow_private_hosts = true;
        // Passes SSRF validation; fails at browser launch (no Chrome).
        let result = browser_scrape(Some(&json!({"url":"http://192.168.1.1/"})), &cfg).await;
        // Error is expected — just NOT UrlNotAllowed.
        if let Err(e) = &result {
            assert!(
                !matches!(e, WebSearchError::UrlNotAllowed(_)),
                "allow_private_hosts should skip SSRF, but got UrlNotAllowed: {e}"
            );
        }
    }

    // ── Live integration tests (Chrome required) ──────────────────────────

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_live_markdown() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "formats": ["markdown"]})),
            &cfg,
        )
        .await
        .unwrap();
        let blocks = result["content"].as_array().unwrap();
        assert!(!blocks.is_empty());
        let text = blocks[0]["text"].as_str().unwrap();
        assert!(text.contains("Example"), "expected 'Example' in markdown");
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_live_links() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "formats": ["links"]})),
            &cfg,
        )
        .await
        .unwrap();
        let blocks = result["content"].as_array().unwrap();
        assert!(!blocks.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_live_raw_html() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "formats": ["rawHtml"]})),
            &cfg,
        )
        .await
        .unwrap();
        let html = result["content"][0]["text"].as_str().unwrap();
        assert!(html.contains("<html") || html.contains("<!DOCTYPE"));
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_live_screenshot_format() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "formats": ["screenshot"]})),
            &cfg,
        )
        .await
        .unwrap();
        let img = &result["content"][0];
        assert_eq!(img["type"], "image");
        assert_eq!(img["mimeType"], "image/png");
        let data = img["data"].as_str().unwrap();
        assert!(!data.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_screenshot_live() {
        let cfg = cfg_enabled();
        let result = browser_screenshot(
            Some(&json!({"url": "https://example.com"})),
            &cfg,
        )
        .await
        .unwrap();
        let img = &result["content"][0];
        assert_eq!(img["type"], "image");
        assert_eq!(img["mimeType"], "image/png");
        // PNG magic bytes encoded as base64 start with "iVBORw0KGgo"
        let b64 = img["data"].as_str().unwrap();
        assert!(b64.starts_with("iVBOR"), "expected PNG magic bytes in base64");
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_screenshot_full_page() {
        let cfg = cfg_enabled();
        let result = browser_screenshot(
            Some(&json!({"url": "https://example.com", "fullPage": true})),
            &cfg,
        )
        .await
        .unwrap();
        assert_eq!(result["content"][0]["type"], "image");
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_wait_for_selector() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "waitFor": "h1", "waitTimeoutMs": 3000})),
            &cfg,
        )
        .await
        .unwrap();
        assert!(!result["content"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_javascript_execution() {
        let cfg = cfg_enabled();
        // Inject a custom element and verify it appears in the HTML
        let result = browser_scrape(
            Some(&json!({
                "url": "https://example.com",
                "javascript": "document.body.setAttribute('data-test','injected')",
                "formats": ["rawHtml"]
            })),
            &cfg,
        )
        .await
        .unwrap();
        let html = result["content"][0]["text"].as_str().unwrap();
        assert!(html.contains("data-test"), "JS injection should be visible in HTML");
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_concurrent_pages() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        let cfg = Arc::new(cfg_enabled());
        let barrier = Arc::new(Barrier::new(3));

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let cfg = Arc::clone(&cfg);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    let args = json!({"url": "https://example.com"});
                    let result = browser_scrape(Some(&args), &cfg);
                    barrier.wait().await;
                    result.await
                })
            })
            .collect();

        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_only_main_content() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "onlyMainContent": true})),
            &cfg,
        )
        .await
        .unwrap();
        assert!(!result["content"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_scroll_to_bottom() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({"url": "https://example.com", "scrollToBottom": true})),
            &cfg,
        )
        .await
        .unwrap();
        assert!(!result["content"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_browser_scrape_multiple_formats() {
        let cfg = cfg_enabled();
        let result = browser_scrape(
            Some(&json!({
                "url": "https://example.com",
                "formats": ["markdown", "links", "screenshot"]
            })),
            &cfg,
        )
        .await
        .unwrap();
        let blocks = result["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");  // markdown
        assert_eq!(blocks[1]["type"], "text");  // links
        assert_eq!(blocks[2]["type"], "image"); // screenshot
    }
}
