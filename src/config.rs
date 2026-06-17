use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

/// Number of logical CPUs detected at startup. Used as the scaling basis for
/// connection limits, concurrency bounds, and HTTP pool sizing.
pub static CPU_COUNT: LazyLock<usize> = LazyLock::new(num_cpus::get);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchProvider {
    Searxng,
    DuckDuckGo,
    Bing,
    Tavily,
    Google,
    Zhipu,
    Exa,
    Bocha,
}

impl SearchProvider {
    #[must_use]
    pub const fn requires_api_key(self) -> bool {
        matches!(
            self,
            SearchProvider::Bing
                | SearchProvider::Tavily
                | SearchProvider::Google
                | SearchProvider::Zhipu
                | SearchProvider::Exa
                | SearchProvider::Bocha
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SearchProvider::Searxng => "searxng",
            SearchProvider::DuckDuckGo => "duckduckgo",
            SearchProvider::Bing => "bing",
            SearchProvider::Tavily => "tavily",
            SearchProvider::Google => "google",
            SearchProvider::Zhipu => "zhipu",
            SearchProvider::Exa => "exa",
            SearchProvider::Bocha => "bocha",
        }
    }
}

impl fmt::Display for SearchProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SearchProvider {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "searxng" => Ok(SearchProvider::Searxng),
            "duckduckgo" | "ddg" => Ok(SearchProvider::DuckDuckGo),
            "bing" => Ok(SearchProvider::Bing),
            "tavily" => Ok(SearchProvider::Tavily),
            "google" => Ok(SearchProvider::Google),
            "zhipu" => Ok(SearchProvider::Zhipu),
            "exa" => Ok(SearchProvider::Exa),
            "bocha" => Ok(SearchProvider::Bocha),
            other => Err(format!(
                "Invalid search provider: {other}. Supported: searxng, duckduckgo, bing, tavily, google, zhipu, exa, bocha"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub http_port: u16,
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_extract_urls: usize,
    pub max_map_urls: usize,
    pub auth_token: Option<Arc<str>>,
    pub max_connections: usize,
    pub rate_limit: f64,
}

#[derive(Debug, Clone)]
pub struct SearchDefaults {
    pub limit: usize,
    pub language: Arc<str>,
    pub categories: Arc<str>,
    pub time_range: Arc<str>,
    pub safe_search: u8,
    pub engines: Arc<str>,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: SearchProvider,
    pub api_key: Option<Arc<str>>,
    pub api_url: Option<Arc<str>>,
    pub search: SearchDefaults,
    pub server: ServerConfig,
    pub max_response_bytes: usize,
    pub max_redirects: usize,
    pub allow_private_hosts: bool,
    pub dns_pin: bool,
}

impl Config {
    pub fn from_args(args: &super::Args) -> crate::errors::Result<Self> {
        let provider = args.search_provider;

        if provider.requires_api_key() && args.search_api_key.as_deref().unwrap_or("").is_empty() {
            return Err(crate::errors::WebSearchError::ConfigError(format!(
                "provider '{provider}' requires SEARCH_API_KEY (--search-api-key)"
            )));
        }
        let needs_url = |p: SearchProvider| -> bool {
            matches!(p, SearchProvider::Searxng | SearchProvider::Google)
        };
        if needs_url(provider) && args.search_api_url.is_none() {
            let hint = match provider {
                SearchProvider::Searxng => "SearXNG base URL",
                SearchProvider::Google => "Google Custom Search Engine id",
                _ => "API URL",
            };
            return Err(crate::errors::WebSearchError::ConfigError(format!(
                "provider '{provider}' requires SEARCH_API_URL (--search-api-url) set to the {hint}"
            )));
        }

        let cpus = *CPU_COUNT;

        Ok(Config {
            provider,
            api_key: args.search_api_key.as_ref().map(|s| Arc::from(s.as_str())),
            api_url: args.search_api_url.as_ref().map(|s| Arc::from(s.as_str())),
            search: SearchDefaults {
                limit: args.limit,
                language: Arc::from(args.language.as_str()),
                categories: Arc::from(args.categories.as_str()),
                time_range: Arc::from(args.time_range.as_str()),
                safe_search: args.safe_search,
                engines: Arc::from(args.engines.as_str()),
                timeout: Duration::from_millis(args.timeout),
            },
            server: ServerConfig {
                host: args.host.clone(),
                port: args.port,
                http_port: args.http_port,
                request_timeout: Duration::from_secs(args.request_timeout),
                max_request_bytes: args.max_request_bytes,
                max_extract_urls: if args.max_extract_urls > 0 {
                    args.max_extract_urls
                } else {
                    (cpus * 2).max(100)
                },
                max_map_urls: if args.max_map_urls > 0 {
                    args.max_map_urls
                } else {
                    (cpus * 100).clamp(1000, 100_000)
                },
                auth_token: args.auth_token.as_ref().map(|s| Arc::from(s.as_str())),
                max_connections: if args.max_connections > 0 {
                    args.max_connections
                } else {
                    (cpus * 256).max(64)
                },
                rate_limit: args.rate_limit.max(0.0),
            },
            max_response_bytes: args.max_response_bytes,
            max_redirects: args.max_redirects,
            allow_private_hosts: args.allow_private_hosts,
            dns_pin: args.dns_pin,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        let cpus = *CPU_COUNT;
        Self {
            provider: SearchProvider::DuckDuckGo,
            api_key: None,
            api_url: None,
            search: SearchDefaults {
                limit: 10,
                language: Arc::from("auto"),
                categories: Arc::from("general"),
                time_range: Arc::from(""),
                safe_search: 0,
                engines: Arc::from("all"),
                timeout: Duration::from_millis(10_000),
            },
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
                http_port: 3001,
                request_timeout: Duration::from_secs(30),
                max_request_bytes: 16 * 1024 * 1024,
                max_extract_urls: (cpus * 2).max(100),
                max_map_urls: (cpus * 100).clamp(1000, 100_000),
                auth_token: None,
                max_connections: (cpus * 256).max(64),
                rate_limit: 0.0,
            },
            max_response_bytes: 8 * 1024 * 1024,
            max_redirects: 5,
            allow_private_hosts: false,
            dns_pin: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_parse() {
        assert_eq!(
            "duckduckgo".parse::<SearchProvider>().unwrap(),
            SearchProvider::DuckDuckGo
        );
        assert_eq!(
            "SearXNG".parse::<SearchProvider>().unwrap(),
            SearchProvider::Searxng
        );
        assert_eq!(
            "ddg".parse::<SearchProvider>().unwrap(),
            SearchProvider::DuckDuckGo
        );
        assert!("nope".parse::<SearchProvider>().is_err());
    }

    #[test]
    fn test_requires_api_key() {
        assert!(SearchProvider::Tavily.requires_api_key());
        assert!(SearchProvider::Bing.requires_api_key());
        assert!(SearchProvider::Google.requires_api_key());
        assert!(SearchProvider::Zhipu.requires_api_key());
        assert!(SearchProvider::Exa.requires_api_key());
        assert!(SearchProvider::Bocha.requires_api_key());
        assert!(!SearchProvider::DuckDuckGo.requires_api_key());
        assert!(!SearchProvider::Searxng.requires_api_key());
    }

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.provider, SearchProvider::DuckDuckGo);
        assert_eq!(cfg.search.limit, 10);
        assert!(!cfg.allow_private_hosts);
        assert_eq!(&*cfg.search.language, "auto");
        assert_eq!(&*cfg.search.categories, "general");
    }

    #[test]
    fn test_search_provider_display() {
        assert_eq!(SearchProvider::DuckDuckGo.to_string(), "duckduckgo");
        assert_eq!(SearchProvider::Searxng.to_string(), "searxng");
        assert_eq!(SearchProvider::Bing.to_string(), "bing");
    }

    #[test]
    fn test_search_provider_as_str() {
        assert_eq!(SearchProvider::Exa.as_str(), "exa");
        assert_eq!(SearchProvider::Bocha.as_str(), "bocha");
        assert_eq!(SearchProvider::Zhipu.as_str(), "zhipu");
    }
}
