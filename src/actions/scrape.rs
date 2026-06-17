use super::{get_str_arg, get_str_array, text_content};
use crate::client::fetch_page;
use crate::config::Config;
use crate::errors::{Result, WebSearchError};
use scraper::{ElementRef, Html, Node, Selector};
use serde_json::Value;
use std::sync::LazyLock;
use url::Url;

static A_HREF: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static MAIN_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());

// ─── dom_to_markdown — single-parse HTML→markdown ────────────────────────

/// Number of bytes in a valid UTF-8 sequence given the leading byte.
/// Returns 1 for ASCII, continuation bytes, or invalid lead bytes ≥0xF8.
#[inline]
const fn utf8_seq_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// Scalar helper: collapse whitespace in `bytes[i..end]`, updating `out`
/// and `prev_was_space`. Returns the final byte position (may exceed `end`
/// when a multi-byte UTF-8 sequence straddles the boundary).
#[inline]
fn scalar_ws(bytes: &[u8], s: &str, mut i: usize, end: usize, out: &mut String, prev_was_space: &mut bool) -> usize {
    while i < end {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            if !*prev_was_space {
                out.push(' ');
                *prev_was_space = true;
            }
            i += 1;
        } else if b < 0x80 {
            out.push(b as char);
            *prev_was_space = false;
            i += 1;
        } else {
            let seq_len = utf8_seq_len(b);
            let seq_end = (i + seq_len).min(bytes.len());
            out.push_str(&s[i..seq_end]);
            *prev_was_space = false;
            i = seq_end;
        }
    }
    i
}

/// Collapse runs of ASCII whitespace to a single space.
///
/// ## SSE2 path (x86_64)
/// - 16-byte chunked loop; `pmovmskb` non-ASCII check → scalar fallback.
/// - Whitespace detection via **range check** (article §"SIMD Hash Table"):
///   one `pcmpeqb` for space, one `paddb` + `pcmpgtb` for the range `[0x09,0x0D]`,
///   avoiding five individual byte comparisons.
/// - Bitmask walk with `trailing_zeros` for run extraction.
///
/// ## NEON path (aarch64)
/// - 16-byte chunked loop; `vmaxvq_u8` for both non-ASCII and whitespace checks.
/// - Same range-check technique with NEON `vsubq_u8` + `vcleq_u8`.
/// - Absent a direct `movemask`, the "any whitespace" fast path bulk-copies;
///   mixed chunks fall back to scalar for that window.
///
/// ## Generic path
/// - Simple byte-scan loop (lets LLVM auto-vectorize when possible).
#[cfg(target_arch = "x86_64")]
fn collapse_whitespace(s: &str) -> String {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut prev_was_space = false;

    if len < 16 {
        scalar_ws(bytes, s, 0, len, &mut out, &mut prev_was_space);
        return out;
    }

    use std::arch::x86_64::*;

    let mut i = 0;
    unsafe {
        let ones = _mm_set1_epi8(-1i8);
        // Range check: subtract 9, then unsigned ≤ 4.
        // Sub9 in [0x00, 0x04] ↔ lane in [0x09, 0x0D].
        // Use unsigned-compare trick: XOR with 0x80, then signed > 0x84.
        let sign = _mm_set1_epi8(0x80u8 as i8);

        while i + 16 <= len {
            let chunk = _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i);
            let non_ascii = _mm_movemask_epi8(chunk) as u32;

            if non_ascii != 0 {
                i = scalar_ws(bytes, s, i, i + 16, &mut out, &mut prev_was_space);
                continue;
            }

            // ── All-ASCII: detect whitespace (article: range check over 5 PCMPEQB) ──
            let eq_space = _mm_cmpeq_epi8(chunk, _mm_set1_epi8(b' ' as i8));
            let sub9 = _mm_add_epi8(chunk, _mm_set1_epi8((-9i8) as i8));
            let biased = _mm_xor_si128(sub9, sign);
            // Unsigned: sub9 > 4 ?  ↔  signed: biased > 0x84 ?
            let gt4 = _mm_cmpgt_epi8(biased, _mm_set1_epi8(0x84u8 as i8));
            let in_range = _mm_xor_si128(gt4, ones);

            let ws = _mm_or_si128(eq_space, in_range);
            let ws_mask = _mm_movemask_epi8(ws) as u16;

            if ws_mask == 0 {
                out.push_str(&s[i..i + 16]);
                prev_was_space = false;
                i += 16;
                continue;
            }

            // Walk the bitmask (branchless via trailing_zeros).
            let run_start = i;
            let mut bits = ws_mask;
            loop {
                let trailing_ws = bits.trailing_zeros() as usize;
                if trailing_ws > 0 {
                    let copy_end = i + trailing_ws;
                    out.push_str(&s[i..copy_end]);
                    i = copy_end;
                    prev_was_space = false;
                }
                if !prev_was_space {
                    out.push(' ');
                    prev_was_space = true;
                }
                i += 1;
                bits >>= trailing_ws + 1;
                if bits == 0 {
                    break;
                }
            }
            let remaining = (run_start + 16).saturating_sub(i);
            if remaining > 0 {
                out.push_str(&s[i..run_start + 16]);
                prev_was_space = false;
                i = run_start + 16;
            }
        }
    }

    scalar_ws(bytes, s, i, len, &mut out, &mut prev_was_space);
    out
}

/// NEON SIMD path for aarch64 (Apple Silicon, etc.).
///
/// Uses the same strategy as SSE2 but without a direct `movemask`:
/// `vmaxvq_u8` checks whether any lane hit whitespace — if none did,
/// we bulk-copy the whole 16-byte chunk. Mixed (whitespace + text)
/// chunks fall back to scalar for that window.
#[cfg(target_arch = "aarch64")]
fn collapse_whitespace(s: &str) -> String {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut prev_was_space = false;

    if len < 16 {
        scalar_ws(bytes, s, 0, len, &mut out, &mut prev_was_space);
        return out;
    }

    use std::arch::aarch64::*;

    let mut i = 0;
    unsafe {
        while i + 16 <= len {
            let chunk = vld1q_u8(bytes.as_ptr().add(i));

            // Non-ASCII check via max byte value
            if vmaxvq_u8(chunk) >= 0x80 {
                i = scalar_ws(bytes, s, i, i + 16, &mut out, &mut prev_was_space);
                continue;
            }

            // All ASCII — detect whitespace
            let eq_space = vceqq_u8(chunk, vdupq_n_u8(b' '));
            let sub9 = vsubq_u8(chunk, vdupq_n_u8(9));
            let le4 = vcleq_u8(sub9, vdupq_n_u8(4));
            let ws = vorrq_u8(eq_space, le4);

            if vmaxvq_u8(ws) == 0 {
                // No whitespace — bulk copy
                out.push_str(&s[i..i + 16]);
                prev_was_space = false;
                i += 16;
                continue;
            }

            // Has whitespace — scalar fallback for this chunk
            i = scalar_ws(bytes, s, i, i + 16, &mut out, &mut prev_was_space);
        }
    }

    scalar_ws(bytes, s, i, len, &mut out, &mut prev_was_space);
    out
}

/// Scalar fallback for targets without explicit SIMD.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn collapse_whitespace(s: &str) -> String {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut prev_was_space = false;
    let mut i = 0;
    while i < len {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            if !prev_was_space {
                out.push(' ');
                prev_was_space = true;
            }
            i += 1;
        } else if b < 0x80 {
            out.push(b as char);
            prev_was_space = false;
            i += 1;
        } else {
            let seq_len = utf8_seq_len(b);
            let end = (i + seq_len).min(len);
            out.push_str(&s[i..end]);
            prev_was_space = false;
            i = end;
        }
    }
    out
}

/// Collapse runs of 3+ consecutive newlines down to 2, then trim leading
/// and trailing whitespace.
fn cleanup_md(md: &mut String) {
    let bytes = md.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return;
    }
    let mut out = String::with_capacity(len);
    let mut consecutive_nl = 0u32;
    let mut i = 0;
    while i < len {
        if bytes[i] == b'\n' {
            consecutive_nl += 1;
            if consecutive_nl <= 2 {
                out.push('\n');
            }
            i += 1;
        } else {
            consecutive_nl = 0;
            if bytes[i] < 0x80 {
                out.push(bytes[i] as char);
                i += 1;
            } else {
                let seq_len = utf8_seq_len(bytes[i]);
                let end = (i + seq_len).min(len);
                out.push_str(&md[i..end]);
                i = end;
            }
        }
    }
    let trimmed = out.trim().to_string();
    md.clear();
    md.push_str(&trimmed);
}

/// Walk child nodes of an element, dispatching elements to `walk_element`
/// and appending text content directly.
fn walk_children(el: ElementRef, md: &mut String, list_depth: &mut u32) {
    for child in el.children() {
        if let Some(child_el) = ElementRef::wrap(child) {
            walk_element(child_el, md, list_depth);
        } else if let Node::Text(text) = child.value() {
            let s = collapse_whitespace(&text.text);
            if !s.is_empty() && s != " " {
                md.push_str(&s);
            }
        }
    }
}

/// Recursively walk a single element node and produce markdown.
#[allow(clippy::too_many_lines)]
fn walk_element(el: ElementRef, md: &mut String, list_depth: &mut u32) {
    match el.value().name() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = el.value().name().as_bytes()[1] - b'0';
            for _ in 0..level {
                md.push('#');
            }
            md.push(' ');
            walk_children(el, md, list_depth);
            if !md.ends_with('\n') {
                md.push('\n');
            }
            md.push('\n');
        }
        "p" => {
            walk_children(el, md, list_depth);
            if !md.ends_with('\n') {
                md.push('\n');
            }
            md.push('\n');
        }
        "br" => md.push('\n'),
        "hr" => md.push_str("---\n\n"),
        "blockquote" => {
            let mut inner = String::new();
            walk_children(el, &mut inner, list_depth);
            for line in inner.lines() {
                md.push_str("> ");
                md.push_str(line);
                md.push('\n');
            }
            md.push('\n');
        }
        "pre" => {
            md.push_str("```\n");
            for desc in el.descendants() {
                if let Node::Text(t) = desc.value() {
                    md.push_str(&t.text);
                }
            }
            if !md.ends_with('\n') {
                md.push('\n');
            }
            md.push_str("```\n\n");
        }
        "code" => {
            md.push('`');
            for child in el.children() {
                if let Node::Text(t) = child.value() {
                    md.push_str(t.text.trim());
                }
            }
            md.push('`');
        }
        "ul" => {
            *list_depth += 1;
            for child in el.children() {
                if let Some(child_el) = ElementRef::wrap(child) {
                    walk_element(child_el, md, list_depth);
                }
            }
            *list_depth -= 1;
            if !md.ends_with('\n') {
                md.push('\n');
            }
            md.push('\n');
        }
        "ol" => {
            *list_depth += 1;
            let mut idx = 1u32;
            for child in el.children() {
                if let Some(li_el) = ElementRef::wrap(child) {
                    let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                    md.push_str(&indent);
                    let _ = std::fmt::Write::write_fmt(md, format_args!("{}. ", idx));
                    walk_children(li_el, md, list_depth);
                    md.push('\n');
                    idx += 1;
                }
            }
            *list_depth -= 1;
            md.push('\n');
        }
        "li" => {
            for _ in 0..list_depth.saturating_sub(1) {
                md.push_str("  ");
            }
            md.push_str("- ");
            walk_children(el, md, list_depth);
            md.push('\n');
        }
        "a" => {
            let href = el.value().attr("href").unwrap_or("");
            md.push('[');
            walk_children(el, md, list_depth);
            md.push(']');
            md.push('(');
            md.push_str(href);
            md.push(')');
        }
        "img" => {
            let src = el.value().attr("src").unwrap_or("");
            let alt = el.value().attr("alt").unwrap_or("");
            md.push('!');
            md.push('[');
            md.push_str(alt);
            md.push(']');
            md.push('(');
            md.push_str(src);
            md.push(')');
        }
        "strong" | "b" => {
            md.push_str("**");
            walk_children(el, md, list_depth);
            md.push_str("**");
        }
        "em" | "i" => {
            md.push('*');
            walk_children(el, md, list_depth);
            md.push('*');
        }
        "del" | "s" => {
            md.push_str("~~");
            walk_children(el, md, list_depth);
            md.push_str("~~");
        }
        // Skip elements whose children are not user-facing content
        "script" | "style" | "head" | "nav" | "footer" | "aside"
        | "form" | "svg" | "math"
        | "input" | "button" | "select" | "textarea" => {}
        // Inline quotation — wrap in double quotes
        "q" => {
            md.push('"');
            walk_children(el, md, list_depth);
            md.push('"');
        }
        // Keyboard input — format as inline code
        "kbd" => {
            md.push('`');
            for child in el.children() {
                if let Node::Text(t) = child.value() {
                    md.push_str(t.text.trim());
                }
            }
            md.push('`');
        }
        // Subscript / superscript — use ^text^ / _{text} (LaTeX-style)
        "sub" => {
            md.push('_');
            md.push('{');
            walk_children(el, md, list_depth);
            md.push('}');
        }
        "sup" => {
            md.push('^');
            walk_children(el, md, list_depth);
            md.push('^');
        }
        // Embedded content — show source URL
        "iframe" => {
            let src = el.value().attr("src").unwrap_or("");
            if !src.is_empty() {
                md.push_str(&format!("[iframe: {src}]"));
            }
        }
        // Tables: extract text with minimal spacing
        "td" | "th" => {
            walk_children(el, md, list_depth);
            md.push(' ');
        }
        "tr" => {
            walk_children(el, md, list_depth);
            md.push('\n');
        }
        "table" => {
            walk_children(el, md, list_depth);
            md.push('\n');
        }
        // Fragments that should show text but don't need markdown formatting
        "span" | "div" | "section" | "article" | "main"
        | "header" | "figure" | "figcaption" | "details" | "summary"
        | "u" | "ins" | "mark" | "small" | "cite" | "dfn"
        | "abbr" | "bdi" | "bdo" | "noscript" | "template"
        | "dl" | "dt" | "dd" | "wbr" => {
            walk_children(el, md, list_depth);
        }
        _ => {
            walk_children(el, md, list_depth);
        }
    }
}

/// Parse HTML and produce markdown by walking scraper's ego-tree (arena-allocated DOM).
/// Avoids the second HTML parse that htmd requires, but produces simpler formatting.
pub fn dom_to_markdown(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut md = String::with_capacity(html.len().max(64));
    let mut list_depth = 0u32;
    for child in doc.tree.root().children() {
        if let Some(el) = ElementRef::wrap(child) {
            walk_element(el, &mut md, &mut list_depth);
        }
    }
    cleanup_md(&mut md);
    md
}

/// Parse HTML, isolate main/article content, convert to markdown in one pass.
/// Falls back to the full document if no main element is found.
pub fn main_to_markdown(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut md = String::with_capacity(html.len().max(64));
    let mut list_depth = 0u32;
    if let Some(main) = doc.select(&MAIN_SEL).next() {
        walk_element(main, &mut md, &mut list_depth);
    } else {
        for child in doc.tree.root().children() {
            if let Some(el) = ElementRef::wrap(child) {
                walk_element(el, &mut md, &mut list_depth);
            }
        }
    }
    cleanup_md(&mut md);
    md
}

/// Strip all HTML tags, returning only text runs joined by spaces.
/// Skips content inside `<script>`, `<style>`, and other non-content elements.
pub fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut out = String::with_capacity(html.len() / 2);
    for child in doc.tree.root().children() {
        if let Some(el) = ElementRef::wrap(child) {
            text_walk_element(el, &mut out);
        }
    }
    let trimmed = out.trim().to_string();
    out.clear();
    out.push_str(&trimmed);
    out
}

fn text_skip_element(name: &str) -> bool {
    matches!(name, "script" | "style" | "nav" | "footer" | "aside" | "head")
}

fn text_walk_element(el: ElementRef, out: &mut String) {
    if text_skip_element(el.value().name()) {
        return;
    }
    for child in el.children() {
        if let Node::Text(t) = child.value() {
            let s = collapse_whitespace(t.text.trim());
            if !s.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&s);
            }
        } else if let Some(child_el) = ElementRef::wrap(child) {
            text_walk_element(child_el, out);
        }
    }
}

// ─── Existing tool: web_scrape ──────────────────────────────────────────

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
        let mut sections: Vec<String> = Vec::with_capacity(formats.len());
        for fmt in &formats {
            match fmt.as_str() {
                "markdown" | "extract" => {
                    let md = if only_main {
                        main_to_markdown(&body)
                    } else {
                        dom_to_markdown(&body)
                    };
                    sections.push(md);
                }
                "html" => {
                    if only_main {
                        sections.push(extract_main(&body));
                    } else {
                        sections.push(body.clone());
                    }
                }
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

// ─── New lightweight tools ───────────────────────────────────────────────

/// `web_fetch` — fetch a single URL and return content as markdown.
/// Simpler than web_scrape: no format options, always returns markdown.
/// Uses the efficient dom_to_markdown path (single HTML parse, no htmd).
pub async fn web_fetch(args: Option<&Value>, config: &Config) -> Result<Value> {
    let url = get_str_arg(args, "url")?;
    let page = fetch_page(&url, config).await?;
    let body = page.body;
    let text = tokio::task::spawn_blocking(move || dom_to_markdown(&body))
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("parse task failed: {e}")))?;
    Ok(text_content(if text.is_empty() {
        "No content found"
    } else {
        &text
    }))
}

/// `web_fetch_text` — fetch a single URL and return as plain text.
/// All HTML tags are stripped; runs of whitespace are collapsed to single spaces.
/// Lighter than full markdown conversion — ideal for feeds, previews, or
/// downstream NLP processing that wants raw text without formatting.
pub async fn web_fetch_text(args: Option<&Value>, config: &Config) -> Result<Value> {
    let url = get_str_arg(args, "url")?;
    let page = fetch_page(&url, config).await?;
    let body = page.body;
    let text = tokio::task::spawn_blocking(move || html_to_text(&body))
        .await
        .map_err(|e| WebSearchError::ProviderError(format!("parse task failed: {e}")))?;
    Ok(text_content(if text.is_empty() {
        "No content found"
    } else {
        &text
    }))
}

// ─── Library helpers (used by extract.rs and others) ─────────────────────

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
    tokio::task::spawn_blocking(move || Ok(main_to_markdown(&body)))
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
            if !seen.contains(&s) {
                seen.insert(s.clone());
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

    // ─── dom_to_markdown: extended element tests ───────────────────────

    #[test]
    fn test_dom_to_markdown_headings_all_levels() {
        let md = dom_to_markdown("<h1>a</h1><h2>b</h2><h3>c</h3><h4>d</h4><h5>e</h5><h6>f</h6>");
        assert!(md.contains("# a"), "got: {md:?}");
        assert!(md.contains("## b"), "got: {md:?}");
        assert!(md.contains("### c"), "got: {md:?}");
        assert!(md.contains("#### d"), "got: {md:?}");
        assert!(md.contains("##### e"), "got: {md:?}");
        assert!(md.contains("###### f"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_paragraphs() {
        let md = dom_to_markdown("<p>First</p><p>Second</p>");
        assert!(md.contains("First"), "got: {md:?}");
        assert!(md.contains("Second"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_paragraph_with_inline() {
        let md = dom_to_markdown("<p>Hello <strong>world</strong>!</p>");
        assert!(md.contains("Hello **world**!"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_br() {
        let md = dom_to_markdown("<p>Line1<br>Line2</p>");
        assert!(md.contains("Line1"), "got: {md:?}");
        assert!(md.contains("Line2"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_hr() {
        let md = dom_to_markdown("<hr>");
        assert!(md.contains("---"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_bold_variants() {
        let md = dom_to_markdown("<p><b>bold</b> and <strong>strong</strong></p>");
        assert!(md.contains("**bold**"), "got: {md:?}");
        assert!(md.contains("**strong**"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_italic_variants() {
        let md = dom_to_markdown("<p><i>italic</i> and <em>emphasis</em></p>");
        assert!(md.contains("*italic*"), "got: {md:?}");
        assert!(md.contains("*emphasis*"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_strikethrough_variants() {
        let md = dom_to_markdown("<p><del>gone</del> and <s>struck</s></p>");
        assert!(md.contains("~~gone~~"), "got: {md:?}");
        assert!(md.contains("~~struck~~"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_links() {
        let md = dom_to_markdown(r#"<a href="https://x.com">X</a>"#);
        assert!(md.contains("[X](https://x.com)"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_link_no_href() {
        let md = dom_to_markdown("<a>no link</a>");
        assert!(md.contains("[no link]()"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_images() {
        let md = dom_to_markdown(r#"<img src="pic.png" alt="Photo">"#);
        assert!(md.contains("![Photo](pic.png)"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_image_no_alt() {
        let md = dom_to_markdown(r#"<img src="pic.png">"#);
        assert!(md.contains("![]"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_inline_code() {
        let md = dom_to_markdown("<p>Use <code>map()</code></p>");
        assert!(md.contains("`map()`"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_pre_code() {
        let md = dom_to_markdown("<pre><code>let x = 1;</code></pre>");
        assert!(md.contains("```"), "got: {md:?}");
        assert!(md.contains("let x = 1;"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_pre_no_code() {
        let md = dom_to_markdown("<pre>raw text</pre>");
        assert!(md.contains("```"), "got: {md:?}");
        assert!(md.contains("raw text"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_q() {
        let md = dom_to_markdown("<p><q>cite</q> said</p>");
        assert!(md.contains("\"cite\""), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_kbd() {
        let md = dom_to_markdown("<p>Press <kbd>Ctrl+C</kbd></p>");
        assert!(md.contains("`Ctrl+C`"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_sub_sup() {
        let md = dom_to_markdown("<p>H<sub>2</sub>O and E=mc<sup>2</sup></p>");
        assert!(md.contains("_{2}"), "got: {md:?}");
        assert!(md.contains("^2^"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_blockquote_single() {
        let md = dom_to_markdown("<blockquote><p>Quote</p></blockquote>");
        assert!(md.contains(">"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_blockquote_multi_paragraph() {
        let html = "<blockquote><p>First</p><p>Second</p></blockquote>";
        let md = dom_to_markdown(html);
        assert!(md.lines().any(|l| l == "> First"), "got: {md:?}");
        assert!(md.lines().any(|l| l == "> Second"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_unordered_list() {
        let md = dom_to_markdown("<ul><li>A</li><li>B</li></ul>");
        assert!(md.contains("- A"), "got: {md:?}");
        assert!(md.contains("- B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_ordered_list() {
        let md = dom_to_markdown("<ol><li>First</li><li>Second</li></ol>");
        assert!(md.contains("1. First"), "got: {md:?}");
        assert!(md.contains("2. Second"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_nested_ul_in_ul() {
        let md = dom_to_markdown("<ul><li>A<ul><li>B</li></ul></li></ul>");
        assert!(md.contains("- A"), "got: {md:?}");
        assert!(md.contains("  - B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_nested_ol_in_ol() {
        let md = dom_to_markdown("<ol><li>One<ol><li>Two</li></ol></li></ol>");
        assert!(md.contains("1. One"), "got: {md:?}");
        assert!(md.contains(" 1. Two"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_mixed_nested_lists() {
        let md = dom_to_markdown("<ul><li>A<ol><li>1</li></ol></li></ul>");
        assert!(md.contains("- A"), "got: {md:?}");
        assert!(md.contains("1. 1"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_li_with_paragraph() {
        let md = dom_to_markdown("<ul><li><p>Item</p></li></ul>");
        assert!(md.contains("- Item"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_table_simple() {
        let md = dom_to_markdown("<table><tr><td>A</td><td>B</td></tr></table>");
        assert!(md.contains("A"), "got: {md:?}");
        assert!(md.contains("B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_table_with_headers() {
        let md = dom_to_markdown("<table><tr><th>Name</th><th>Value</th></tr></table>");
        assert!(md.contains("Name"), "got: {md:?}");
        assert!(md.contains("Value"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_skips_head() {
        let md = dom_to_markdown("<html><head><title>secret</title></head><body>visible</body></html>");
        assert!(!md.contains("secret"), "got: {md:?}");
        assert!(md.contains("visible"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_skips_script_style() {
        let md = dom_to_markdown("<p>Text</p><script>alert(1)</script><style>.c{}</style>");
        assert!(!md.contains("alert(1)"), "got: {md:?}");
        assert!(!md.contains(".c{}"), "got: {md:?}");
        assert!(md.contains("Text"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_skips_nav_footer_aside_form() {
        let md = dom_to_markdown("<nav>n</nav><footer>f</footer><aside>a</aside><form>frm</form>");
        assert!(!md.contains('n'), "got: {md:?}");
        assert!(!md.contains('f'), "got: {md:?}");
        assert!(!md.contains('a'), "got: {md:?}");
        assert!(!md.contains("frm"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_skips_svg_math() {
        let md = dom_to_markdown("<p>a</p><svg><text>svg</text></svg><math><mi>x</mi></math>");
        assert!(!md.contains("svg"), "got: {md:?}");
        assert!(!md.contains("x"), "got: {md:?}");
        assert!(md.contains('a'), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_skips_input_button_select() {
        let md = dom_to_markdown("<input value='x'><button>Click</button><select><option>a</option></select>");
        assert!(!md.contains("Click"), "got: {md:?}");
        assert!(!md.contains('a'), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_iframe() {
        let md = dom_to_markdown(r#"<iframe src="https://example.com"></iframe>"#);
        assert!(md.contains("[iframe:"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_iframe_no_src() {
        let md = dom_to_markdown("<iframe></iframe>");
        assert!(!md.contains("iframe"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_empty() {
        assert_eq!(dom_to_markdown(""), "");
    }

    #[test]
    fn test_dom_to_markdown_no_body_content() {
        let md = dom_to_markdown("<html><head></head><body></body></html>");
        assert!(md.is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_only_text() {
        let md = dom_to_markdown("plain text");
        assert!(md.contains("plain text"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_whitespace_only() {
        let md = dom_to_markdown("   \n\n  ");
        assert!(md.is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_nested_formatting() {
        let md = dom_to_markdown(r#"<p><strong><em>bold italic</em></strong></p>"#);
        assert!(md.contains("***bold italic***"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_link_with_bold() {
        let md = dom_to_markdown(r#"<a href="https://x.com"><strong>bold link</strong></a>"#);
        assert!(md.contains("[**bold link**](https://x.com)"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_link_with_image() {
        let md = dom_to_markdown(r#"<a href="https://x.com"><img src="pic.png" alt="img"></a>"#);
        assert!(md.contains("[![img](pic.png)](https://x.com)"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_heading_with_inline() {
        let md = dom_to_markdown("<h1>Hello <em>World</em></h1>");
        assert!(md.contains("# Hello *World*"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_blockquote_with_list() {
        let md = dom_to_markdown("<blockquote><ul><li>A</li><li>B</li></ul></blockquote>");
        assert!(md.contains("> - A"), "got: {md:?}");
        assert!(md.contains("> - B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_article_section_main() {
        let md = dom_to_markdown("<article><section><p>content</p></section></article>");
        assert!(md.contains("content"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_figure() {
        let md = dom_to_markdown("<figure><img src='x.png' alt='X'><figcaption>Caption</figcaption></figure>");
        assert!(md.contains("![X](x.png)"), "got: {md:?}");
        assert!(md.contains("Caption"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_details_summary() {
        let md = dom_to_markdown("<details><summary>Title</summary>Hidden</details>");
        assert!(md.contains("Title"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_underline_and_mark() {
        let md = dom_to_markdown("<p><u>under</u> and <mark>highlight</mark></p>");
        assert!(md.contains("under"), "got: {md:?}");
        assert!(md.contains("highlight"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_noscript() {
        let md = dom_to_markdown("<p>before</p><noscript>JS disabled</noscript><p>after</p>");
        assert!(md.contains("before"), "got: {md:?}");
        assert!(md.contains("JS disabled"), "got: {md:?}");
        assert!(md.contains("after"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_definition_list() {
        let md = dom_to_markdown("<dl><dt>Term</dt><dd>Definition</dd></dl>");
        assert!(md.contains("Term"), "got: {md:?}");
        assert!(md.contains("Definition"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_deeply_nested() {
        let html = "<div><div><div><div><p>deep</p></div></div></div></div>";
        let md = dom_to_markdown(html);
        assert!(md.contains("deep"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_large_text_block() {
        let html = format!("<p>{}</p>", "A".repeat(10000));
        let md = dom_to_markdown(&html);
        assert!(md.contains("AAAAA"), "got len: {}", md.len());
    }

    #[test]
    fn test_dom_to_markdown_multiple_h1() {
        let md = dom_to_markdown("<h1>A</h1><h1>B</h1>");
        assert!(md.contains("# A"), "got: {md:?}");
        assert!(md.contains("# B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_empty_elements() {
        let md = dom_to_markdown("<p></p><div></div><span></span>");
        assert!(md.is_empty() || md.trim().is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_span_only() {
        let md = dom_to_markdown("<span>spanned</span>");
        assert!(md.contains("spanned"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_comment_handled() {
        let md = dom_to_markdown("<p>before<!-- comment -->after</p>");
        assert!(md.contains("before"), "got: {md:?}");
        assert!(md.contains("after"), "got: {md:?}");
    }

    // ─── main_to_markdown tests ───────────────────────────────────────

    #[test]
    fn test_main_to_markdown_main_tag() {
        let html = "<body><nav>menu</nav><main><p>content</p></main></body>";
        let md = main_to_markdown(html);
        assert!(md.contains("content"), "got: {md:?}");
        assert!(!md.contains("menu"), "got: {md:?}");
    }

    #[test]
    fn test_main_to_markdown_article() {
        let html = "<body><header>top</header><article>article text</article></body>";
        let md = main_to_markdown(html);
        assert!(md.contains("article text"), "got: {md:?}");
        assert!(!md.contains("top"), "got: {md:?}");
    }

    #[test]
    fn test_main_to_markdown_role_main() {
        let html = r#"<div role="main">primary</div><div>other</div>"#;
        let md = main_to_markdown(html);
        assert!(md.contains("primary"), "got: {md:?}");
    }

    #[test]
    fn test_main_to_markdown_fallback() {
        let html = "<html><body>everything</body></html>";
        let md = main_to_markdown(html);
        assert!(md.contains("everything"), "got: {md:?}");
    }

    #[test]
    fn test_main_to_markdown_empty() {
        assert_eq!(main_to_markdown(""), "");
    }

    #[test]
    fn test_main_to_markdown_no_main_empty_body() {
        let md = main_to_markdown("<html><body></body></html>");
        assert!(md.is_empty(), "got: {md:?}");
    }

    // ─── html_to_text tests ───────────────────────────────────────────

    #[test]
    fn test_html_to_text_basic() {
        let text = html_to_text("<h1>Title</h1><p>Body text</p>");
        assert!(text.contains("Title"), "got: {text:?}");
        assert!(text.contains("Body text"), "got: {text:?}");
    }

    #[test]
    fn test_html_to_text_strips_tags() {
        let text = html_to_text("<div><strong>bold</strong> and <em>italic</em></div>");
        assert_eq!(text, "bold and italic");
    }

    #[test]
    fn test_html_to_text_skips_script() {
        let text = html_to_text("<p>Hi</p><script>alert(1)</script>");
        assert_eq!(text, "Hi");
    }

    #[test]
    fn test_html_to_text_skips_style() {
        let text = html_to_text("<p>A</p><style>.c{color:red}</style><p>B</p>");
        assert_eq!(text, "A B");
    }

    #[test]
    fn test_html_to_text_skips_nav_footer_aside() {
        let text = html_to_text("<p>main</p><nav>nav</nav><footer>footer</footer><aside>aside</aside>");
        assert_eq!(text, "main");
    }

    #[test]
    fn test_html_to_text_empty() {
        assert_eq!(html_to_text(""), "");
    }

    #[test]
    fn test_html_to_text_no_text() {
        let text = html_to_text("<div><script>x</script></div>");
        assert!(text.is_empty(), "got: {text:?}");
    }

    #[test]
    fn test_html_to_text_multiple_paragraphs() {
        let text = html_to_text("<p>First</p><p>Second</p>");
        assert_eq!(text, "First Second");
    }

    #[test]
    fn test_html_to_text_table() {
        let text = html_to_text("<table><tr><td>A</td><td>B</td></tr></table>");
        assert_eq!(text, "A B");
    }

    #[test]
    fn test_html_to_text_whitespace_collapsed() {
        let text = html_to_text("<p>  Hello    World  </p>");
        assert_eq!(text, "Hello World");
    }

    // ─── web_fetch validation tests ───────────────────────────────────

    #[tokio::test]
    async fn test_web_fetch_requires_url() {
        let config = Config::default();
        let result = web_fetch(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_web_fetch_rejects_private() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://127.0.0.1:8080/"});
        let result = web_fetch(Some(&args), &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::UrlNotAllowed(_)));
    }

    #[tokio::test]
    async fn test_web_fetch_text_requires_url() {
        let config = Config::default();
        let result = web_fetch_text(None, &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn test_web_fetch_text_rejects_private() {
        let config = Config::default();
        let args = serde_json::json!({"url": "http://127.0.0.1:8080/"});
        let result = web_fetch_text(Some(&args), &config).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), WebSearchError::UrlNotAllowed(_)));
    }

    // ─── cleanup_md tests ─────────────────────────────────────────────

    #[test]
    fn test_cleanup_removes_excessive_newlines() {
        let mut md = "a\n\n\n\nb\n\n\nc".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "a\n\nb\n\nc");
    }

    #[test]
    fn test_cleanup_trims_whitespace() {
        let mut md = "  hello world  \n\n".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "hello world");
    }

    #[test]
    fn test_cleanup_handles_empty() {
        let mut md = String::new();
        cleanup_md(&mut md);
        assert!(md.is_empty());
    }

    #[test]
    fn test_cleanup_single_newline_preserved() {
        let mut md = "line1\nline2".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "line1\nline2");
    }

    // ─── Edge case: character handling ──────────────────────────────────

    #[test]
    fn test_dom_to_markdown_unicode_emoji() {
        let md = dom_to_markdown("<p>Hello 🌍🌎🌏 World</p>");
        assert!(md.contains("🌍"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_cjk_text() {
        let md = dom_to_markdown("<p>中文测试</p>");
        assert!(md.contains("中文测试"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_cjk_in_heading() {
        let md = dom_to_markdown("<h1>标题</h1>");
        assert!(md.contains("标题"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_html_entities() {
        let md = dom_to_markdown("<p>AT&amp;T &lt;bold&gt;</p>");
        // html5ever decodes &amp; -> &, &lt; -> <
        assert!(md.contains("AT&T"), "got: {md:?}");
        assert!(md.contains("<bold>"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_nbsp_entity() {
        let md = dom_to_markdown("<p>hello&nbsp;world</p>");
        // &nbsp; -> \u{00A0}, which is NOT ascii whitespace
        assert!(md.contains("hell"), "got: {md:?}");
        assert!(md.contains("world"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_unicode_in_inline() {
        let md = dom_to_markdown("<p><strong>Bold emoji 🔥</strong> and <em>italic émoji</em></p>");
        assert!(md.contains("**Bold emoji 🔥**"), "got: {md:?}");
        assert!(md.contains("*italic émoji*"), "got: {md:?}");
    }

    // ─── Edge case: malformed HTML ──────────────────────────────────────

    #[test]
    fn test_dom_to_markdown_unclosed_tags() {
        let md = dom_to_markdown("<p>para<p>second");
        assert!(md.contains("para"), "got: {md:?}");
        assert!(md.contains("second"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_mismatched_tags() {
        let md = dom_to_markdown("<p>text</div></p>");
        assert!(md.contains("text"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_self_closing_void() {
        let md = dom_to_markdown("<p>a<br/>b<br />c</p>");
        assert!(md.contains("a"), "got: {md:?}");
        assert!(md.contains("b"), "got: {md:?}");
        assert!(md.contains("c"), "got: {md:?}");
        assert!(!md.contains("br"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_multiple_consecutive_br() {
        let md = dom_to_markdown("<p>a<br><br>b</p>");
        assert!(md.contains("a"), "got: {md:?}");
        assert!(md.contains("b"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_br_in_heading() {
        let md = dom_to_markdown("<h1>Line1<br>Line2</h1>");
        assert!(md.contains("Line1"), "got: {md:?}");
        assert!(md.contains("Line2"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_br_in_blockquote() {
        let md = dom_to_markdown("<blockquote>Line1<br>Line2</blockquote>");
        assert!(md.contains("> Line1"), "got: {md:?}");
        assert!(md.contains("> Line2"), "got: {md:?}");
    }

    // ─── Edge case: complex nesting ─────────────────────────────────────

    #[test]
    fn test_dom_to_markdown_nested_blockquotes() {
        let md = dom_to_markdown("<blockquote><blockquote><p>deep</p></blockquote></blockquote>");
        assert!(md.contains("> > deep"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_very_deep_nesting() {
        let inner = "<p>bottom</p>".to_string();
        let html = (0..100).fold(inner, |acc, _| format!("<div>{}</div>", acc));
        let md = dom_to_markdown(&html);
        assert!(md.contains("bottom"), "got: {md:?}");
    }

    // ─── Edge case: list edge cases ─────────────────────────────────────

    #[test]
    fn test_dom_to_markdown_empty_list_items() {
        let md = dom_to_markdown("<ul><li></li><li>B</li></ul>");
        assert!(md.contains("- B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_list_with_only_inline() {
        let md = dom_to_markdown("<ul><li><strong>bold</strong> item</li></ul>");
        assert!(md.contains("- **bold** item"), "got: {md:?}");
    }

    // ─── Edge case: table edge cases ────────────────────────────────────

    #[test]
    fn test_dom_to_markdown_table_empty_cells() {
        let md = dom_to_markdown("<table><tr><td></td><td>B</td></tr></table>");
        assert!(md.contains("B"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_table_in_blockquote() {
        let md = dom_to_markdown("<blockquote><table><tr><td>A</td></tr></table></blockquote>");
        assert!(md.contains("> A"), "got: {md:?}");
    }

    // ─── Edge case: link/image edge cases ───────────────────────────────

    #[test]
    fn test_dom_to_markdown_mailto_link() {
        let md = dom_to_markdown(r#"<a href="mailto:user@example.com">Email</a>"#);
        assert!(md.contains("[Email](mailto:user@example.com)"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_img_no_src_no_alt() {
        let md = dom_to_markdown("<img>");
        assert!(md.contains("![]()"), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_img_src_only() {
        let md = dom_to_markdown(r#"<img src="pic.png">"#);
        assert!(md.contains("![](pic.png)"), "got: {md:?}");
    }

    // ─── Edge case: block elements in root text ─────────────────────────

    #[test]
    fn test_dom_to_markdown_consecutive_hr() {
        let md = dom_to_markdown("<hr><hr>");
        assert!(md.matches("---").count() >= 2, "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_consecutive_paragraphs_in_div() {
        let md = dom_to_markdown("<div><p>One</p><p>Two</p></div>");
        assert!(md.contains("One"), "got: {md:?}");
        assert!(md.contains("Two"), "got: {md:?}");
    }

    // ─── Edge case: html_to_text with special elements ───────────────────

    #[test]
    fn test_html_to_text_with_br() {
        let text = html_to_text("<p>a<br>b</p>");
        // <br> has no text children, so it contributes nothing
        assert_eq!(text, "a b");
    }

    #[test]
    fn test_html_to_text_with_links() {
        let text = html_to_text(r#"<p>Click <a href="x">here</a> now</p>"#);
        assert_eq!(text, "Click here now");
    }

    #[test]
    fn test_html_to_text_with_lists() {
        let text = html_to_text("<ul><li>One</li><li>Two</li></ul>");
        assert_eq!(text, "One Two");
    }

    #[test]
    fn test_html_to_text_entities() {
        let text = html_to_text("<p>AT&amp;T &lt;test&gt;</p>");
        assert!(text.contains("AT&T"), "got: {text:?}");
        assert!(text.contains("<test>"), "got: {text:?}");
    }

    #[test]
    fn test_html_to_text_unicode() {
        let text = html_to_text("<p>中文测试 🌍</p>");
        assert!(text.contains("中文测试"), "got: {text:?}");
        assert!(text.contains("🌍"), "got: {text:?}");
    }

    #[test]
    fn test_html_to_text_empty_elements_skipped() {
        let text = html_to_text("<p>A</p><p> </p><p>B</p>");
        assert_eq!(text, "A B");
    }

    #[test]
    fn test_html_to_text_all_skipped() {
        let text = html_to_text("<nav>n</nav><script>x</script>");
        assert!(text.is_empty(), "got: {text:?}");
    }

    // ─── Edge case: cleanup_md with special content ─────────────────────

    #[test]
    fn test_cleanup_all_newlines() {
        let mut md = "\n\n\n\n\n".to_string();
        cleanup_md(&mut md);
        assert!(md.is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_cleanup_unicode_retained() {
        let mut md = "中文\n\n测试".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "中文\n\n测试");
    }

    #[test]
    fn test_cleanup_no_newlines() {
        let mut md = "hello world".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "hello world");
    }

    #[test]
    fn test_cleanup_leading_newlines() {
        let mut md = "\n\nhello".to_string();
        cleanup_md(&mut md);
        assert_eq!(md, "hello");
    }

    // ─── Edge case: collapse_whitespace edge cases ──────────────────────

    #[test]
    fn test_collapse_whitespace_only_spaces() {
        assert_eq!(collapse_whitespace("     "), " ");
    }

    #[test]
    fn test_collapse_whitespace_empty() {
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn test_collapse_whitespace_no_spaces() {
        assert_eq!(collapse_whitespace("hello"), "hello");
    }

    #[test]
    fn test_collapse_whitespace_unicode_no_ascii_whitespace() {
        // \u{00A0} non-breaking space is NOT ascii whitespace
        assert_eq!(collapse_whitespace("\u{00A0}"), "\u{00A0}");
    }

    #[test]
    fn test_collapse_whitespace_mixed_unicode_and_spaces() {
        let s = collapse_whitespace("hello   world\u{00A0}!");
        assert_eq!(s, "hello world\u{00A0}!");
    }

    // ─── Edge case: empty/near-empty input ──────────────────────────────

    #[test]
    fn test_dom_to_markdown_only_whitespace_in_elements() {
        let md = dom_to_markdown("<p>  </p><div>  </div><span>  </span>");
        assert!(md.trim().is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_html_comment_only() {
        let md = dom_to_markdown("<!-- just a comment -->");
        assert!(md.is_empty(), "got: {md:?}");
    }

    #[test]
    fn test_dom_to_markdown_doctype() {
        let md = dom_to_markdown("<!DOCTYPE html><html><p>hello</p></html>");
        assert!(md.contains("hello"), "got: {md:?}");
    }

    // ─── collapse_whitespace tests ─────────────────────────────────────

    #[test]
    fn test_collapse_whitespace_multiple_spaces() {
        assert_eq!(collapse_whitespace("hello    world"), "hello world");
    }

    #[test]
    fn test_collapse_whitespace_newlines() {
        assert_eq!(collapse_whitespace("hello\n\nworld"), "hello world");
    }

    #[test]
    fn test_collapse_whitespace_mixed() {
        assert_eq!(collapse_whitespace("  hello \n world  "), " hello world ");
    }

    #[test]
    fn test_collapse_whitespace_no_change() {
        assert_eq!(collapse_whitespace("hello world"), "hello world");
    }

    #[test]
    fn test_collapse_whitespace_tabs() {
        assert_eq!(collapse_whitespace("hello\tworld"), "hello world");
    }

    // ─── text_skip_element tests ───────────────────────────────────────

    #[test]
    fn test_text_skip_known_elements() {
        for name in &["script", "style", "nav", "footer", "aside", "head"] {
            assert!(text_skip_element(name), "{name} should be skipped");
        }
    }

    #[test]
    fn test_text_skip_content_elements() {
        for name in &["p", "div", "h1", "article", "main"] {
            assert!(!text_skip_element(name), "{name} should not be skipped");
        }
    }
}
