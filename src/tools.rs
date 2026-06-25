use serde_json::{Value, json};
use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// Coarse capability groups used to selectively expose tools at startup.
/// Maps one-to-one to a `--enable-<slug>` flag; keep at or below ten variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolCategory {
    /// Web search (and search-then-scrape).
    Search,
    /// Page scraping & content extraction, including the headless browser.
    Scrape,
    /// Raw HTTP fetches: body, text, headers.
    Fetch,
    /// Site discovery: mapping, sitemaps, link checking.
    Crawl,
}

impl ToolCategory {
    pub const ALL: &'static [ToolCategory] = &[
        ToolCategory::Search,
        ToolCategory::Scrape,
        ToolCategory::Fetch,
        ToolCategory::Crawl,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            ToolCategory::Search => "search",
            ToolCategory::Scrape => "scrape",
            ToolCategory::Fetch => "fetch",
            ToolCategory::Crawl => "crawl",
        }
    }
}

impl fmt::Display for ToolCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

impl FromStr for ToolCategory {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "search" => Ok(ToolCategory::Search),
            "scrape" => Ok(ToolCategory::Scrape),
            "fetch" => Ok(ToolCategory::Fetch),
            "crawl" => Ok(ToolCategory::Crawl),
            _ => Err(format!("Unknown tool category: {s}")),
        }
    }
}

pub struct ToolMeta {
    pub name: &'static str,
    pub category: ToolCategory,
    pub write: bool,
    pub idempotent: bool,
    pub destructive: bool,
}

use ToolCategory::{Crawl, Fetch, Scrape, Search};

#[rustfmt::skip]
pub const ALL_TOOLS: &[ToolMeta] = &[
    ToolMeta { name: "web_search",        category: Search, write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_scrape",        category: Scrape, write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_map",           category: Crawl,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_extract",       category: Scrape, write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_fetch",         category: Fetch,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_fetch_text",    category: Fetch,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_fetch_headers", category: Fetch,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_search_scrape", category: Search, write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_sitemap",       category: Crawl,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_check_links",   category: Crawl,  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "browser_scrape",    category: Scrape, write: false, idempotent: true, destructive: false },
    ToolMeta { name: "browser_screenshot",category: Scrape, write: false, idempotent: true, destructive: false },
];

/// Every tool definition from `tools.json`, parsed once. The `tools/list`
/// payload is derived from this by filtering to a server's enabled categories.
static ALL_TOOL_DEFS: LazyLock<Vec<Value>> = LazyLock::new(|| {
    let tools_json = include_str!("../tools.json");
    serde_json::from_str(tools_json).expect("Failed to parse tools.json")
});

#[inline]
fn lookup(name: &str) -> Option<&'static ToolMeta> {
    ALL_TOOLS.iter().find(|t| t.name == name)
}

#[inline]
#[must_use]
pub fn tool_exists(name: &str) -> bool {
    lookup(name).is_some()
}

#[inline]
#[must_use]
pub fn is_write_tool(name: &str) -> bool {
    lookup(name).map(|t| t.write).unwrap_or(false)
}

/// The category a tool belongs to, or `None` if the tool is unknown.
#[inline]
#[must_use]
pub fn category_of(name: &str) -> Option<ToolCategory> {
    lookup(name).map(|t| t.category)
}

/// Whether a tool is callable given the set of enabled categories. A tool is
/// available only if it exists *and* its category is enabled.
#[inline]
#[must_use]
pub fn is_tool_available(name: &str, enabled: &[ToolCategory]) -> bool {
    category_of(name).is_some_and(|c| enabled.contains(&c))
}

/// Build the `{"tools":[...]}` `tools/list` payload, filtered to the enabled
/// categories. Tools whose category is not enabled are omitted entirely; with
/// an empty `enabled` set the payload is `{"tools":[]}`.
#[must_use]
pub fn build_tools_list(enabled: &[ToolCategory]) -> Value {
    let tools: Vec<&Value> = ALL_TOOL_DEFS
        .iter()
        .filter(|t| {
            t.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| is_tool_available(name, enabled))
        })
        .collect();
    json!({ "tools": tools })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &[
        "web_search",
        "web_scrape",
        "web_map",
        "web_extract",
        "web_fetch",
        "web_fetch_text",
        "web_fetch_headers",
        "web_search_scrape",
        "web_sitemap",
        "web_check_links",
        "browser_scrape",
        "browser_screenshot",
    ];

    #[test]
    fn test_tool_exists() {
        for name in NAMES {
            assert!(tool_exists(name), "tool_exists('{name}') should be true");
        }
        assert!(!tool_exists("nonexistent"));
        assert!(!tool_exists(""));
    }

    #[test]
    fn test_no_write_tools() {
        for name in NAMES {
            assert!(!is_write_tool(name), "is_write_tool('{name}') should be false");
        }
    }

    #[test]
    fn test_is_write_tool_unknown() {
        assert!(!is_write_tool("unknown_tool"));
    }

    #[test]
    fn test_all_tools_unique() {
        let mut names: Vec<&str> = ALL_TOOLS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL_TOOLS.len());
    }

    #[test]
    fn test_every_tool_has_category() {
        for meta in ALL_TOOLS {
            assert_eq!(category_of(meta.name), Some(meta.category));
        }
        assert_eq!(category_of("nonexistent"), None);
    }

    #[test]
    fn test_is_tool_available_gating() {
        assert!(!is_tool_available("web_search", &[]));
        assert!(is_tool_available("web_search", &[ToolCategory::Search]));
        assert!(!is_tool_available("web_search", &[ToolCategory::Fetch]));
        assert!(!is_tool_available("nonexistent", ToolCategory::ALL));
    }

    #[test]
    fn test_category_slug_roundtrip() {
        for &cat in ToolCategory::ALL {
            assert_eq!(cat.slug().parse::<ToolCategory>().unwrap(), cat);
        }
        assert!("bogus".parse::<ToolCategory>().is_err());
    }

    #[test]
    fn test_categories_within_limit() {
        assert!(ToolCategory::ALL.len() <= 10);
    }

    #[test]
    fn test_build_tools_list_filtering() {
        // Nothing enabled → empty.
        assert_eq!(build_tools_list(&[])["tools"].as_array().unwrap().len(), 0);
        // All enabled → every tool.
        let all = build_tools_list(ToolCategory::ALL);
        assert_eq!(all["tools"].as_array().unwrap().len(), ALL_TOOLS.len());
        // One category → only its tools.
        let search = build_tools_list(&[ToolCategory::Search]);
        let names: Vec<&str> = search["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"web_search"));
        assert!(!names.contains(&"web_fetch"));
    }

    #[test]
    fn test_tools_list_response_valid() {
        let resp = build_tools_list(ToolCategory::ALL);
        let tools = resp["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool.get("name").and_then(|n| n.as_str()).is_some());
            assert!(tool.get("description").and_then(|d| d.as_str()).is_some());
        }
    }
}
