use serde_json::Value;
use std::sync::LazyLock;

pub struct ToolMeta {
    pub name: &'static str,
    pub write: bool,
    pub idempotent: bool,
    pub destructive: bool,
}

#[rustfmt::skip]
pub const ALL_TOOLS: &[ToolMeta] = &[
    ToolMeta { name: "web_search",  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_scrape",  write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_map",     write: false, idempotent: true, destructive: false },
    ToolMeta { name: "web_extract", write: false, idempotent: true, destructive: false },
];

/// Pre-deserialized tools list response, cached for the lifetime of the process.
static CACHED_TOOLS_RESPONSE: LazyLock<Value> = LazyLock::new(|| {
    let tools_json = include_str!("../tools.json");
    let tools: Vec<Value> = serde_json::from_str(tools_json).expect("Failed to parse tools.json");
    serde_json::json!({ "tools": tools })
});

#[inline]
#[must_use]
pub fn tool_exists(name: &str) -> bool {
    ALL_TOOLS.iter().any(|t| t.name == name)
}

#[inline]
#[must_use]
pub fn is_write_tool(name: &str) -> bool {
    ALL_TOOLS
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.write)
        .unwrap_or(false)
}

#[inline]
#[must_use]
pub fn tools_list_response() -> &'static Value {
    &CACHED_TOOLS_RESPONSE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_exists() {
        assert!(tool_exists("web_search"));
        assert!(tool_exists("web_scrape"));
        assert!(tool_exists("web_map"));
        assert!(tool_exists("web_extract"));
        assert!(!tool_exists("nonexistent"));
        assert!(!tool_exists(""));
    }

    #[test]
    fn test_no_write_tools() {
        assert!(!is_write_tool("web_search"));
        assert!(!is_write_tool("web_scrape"));
        assert!(!is_write_tool("web_map"));
        assert!(!is_write_tool("web_extract"));
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
    fn test_tools_list_response_valid() {
        let resp = tools_list_response();
        assert!(resp.get("tools").and_then(|t| t.as_array()).is_some());
        let tools = resp["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool.get("name").and_then(|n| n.as_str()).is_some());
            assert!(tool.get("description").and_then(|d| d.as_str()).is_some());
        }
    }

    #[test]
    fn test_tools_list_response_includes_all() {
        let resp = tools_list_response();
        let names: Vec<&str> = resp["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in ["web_search", "web_scrape", "web_map", "web_extract"] {
            assert!(names.contains(&expected), "Missing tool: {expected}");
        }
    }

    #[test]
    fn test_tool_meta_const() {
        let meta = ToolMeta {
            name: "web_search",
            write: false,
            idempotent: true,
            destructive: false,
        };
        assert_eq!(meta.name, "web_search");
        assert!(!meta.write);
        assert!(meta.idempotent);
        assert!(!meta.destructive);
    }
}
