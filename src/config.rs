use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

pub use crate::tools::ToolCategory;

/// Configuration for the optional headless browser (Chrome/Chromium via CDP).
#[derive(Debug, Clone)]
pub struct BrowserSettings {
    /// When true all browser tools return a descriptive error without launching Chrome.
    pub disabled: bool,
    /// Maximum number of simultaneously open browser pages.
    /// Each page uses roughly 50–200 MB of RAM; tune to your machine.
    pub max_pages: usize,
    /// How long to wait for a page to navigate before timing out.
    pub nav_timeout: Duration,
    /// Path to the Chrome/Chromium binary. `None` = auto-detect from PATH.
    pub chrome_path: Option<std::path::PathBuf>,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        let cpus = *CPU_COUNT;
        BrowserSettings {
            disabled: false,
            max_pages: (cpus * 2).max(4),
            nav_timeout: Duration::from_secs(30),
            chrome_path: None,
        }
    }
}

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
    pub http_port: u16,
    pub request_timeout: Duration,
    pub max_request_bytes: usize,
    pub max_extract_urls: usize,
    pub max_map_urls: usize,
    pub auth_token: Option<Arc<str>>,
    pub rate_limit: f64,
    /// Tool categories exposed by this server. Empty (the default) means no
    /// tools are advertised or callable until enabled with `--enable-*`.
    pub enabled_categories: Vec<ToolCategory>,
    /// PEM certificate chain for serving the HTTP transport over TLS (HTTPS).
    /// `None` (the default) keeps the HTTP transport plaintext. Engaged only
    /// when both `tls_cert` and `tls_key` are set.
    pub tls_cert: Option<std::path::PathBuf>,
    /// PEM private key matching `tls_cert`.
    pub tls_key: Option<std::path::PathBuf>,
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
    pub browser: BrowserSettings,
    /// Pre-built `{"tools":[...]}` payload for `tools/list`, filtered to the
    /// enabled categories (see `server.enabled_categories`). Built once at
    /// construction so every request serves an identical, filtered list.
    pub tools_list: Arc<Value>,
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

        // TLS cert/key for the HTTP transport (CLI flags or MCP_TLS_CERT/KEY env,
        // resolved by clap). Both must be supplied together.
        let tls_cert = args
            .tls_cert
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        let tls_key = args
            .tls_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        if tls_cert.is_some() != tls_key.is_some() {
            return Err(crate::errors::WebSearchError::ConfigError(
                "--tls-cert and --tls-key must be provided together (or both omitted for plaintext HTTP)"
                    .to_string(),
            ));
        }

        let cpus = *CPU_COUNT;
        let enabled_categories = args.enabled_categories();
        let tools_list = Arc::new(crate::tools::build_tools_list(&enabled_categories));

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
                auth_token: {
                    let raw = if let Some(ref t) = args.auth_token {
                        Some(t.clone())
                    } else if let Some(ref path) = args.auth_token_file {
                        // Fail closed: a misconfigured token file (missing,
                        // unreadable, or empty) must abort startup, never
                        // silently launch the server with authentication off.
                        let contents = std::fs::read_to_string(path).map_err(|e| {
                            crate::errors::WebSearchError::ConfigError(format!(
                                "failed to read --auth-token-file '{path}': {e}"
                            ))
                        })?;
                        let token = contents.trim().to_string();
                        if token.is_empty() {
                            return Err(crate::errors::WebSearchError::ConfigError(format!(
                                "--auth-token-file '{path}' is empty; refusing to start with authentication disabled"
                            )));
                        }
                        Some(token)
                    } else {
                        None
                    };
                    raw.map(|s| Arc::from(s.as_str()))
                },
                rate_limit: args.rate_limit.max(0.0),
                enabled_categories,
                tls_cert,
                tls_key,
            },
            max_response_bytes: args.max_response_bytes,
            max_redirects: args.max_redirects,
            allow_private_hosts: args.allow_private_hosts,
            dns_pin: args.dns_pin,
            browser: BrowserSettings {
                disabled: args.browser_disable,
                max_pages: if args.browser_max_pages > 0 {
                    args.browser_max_pages
                } else {
                    (cpus * 2).max(4)
                },
                nav_timeout: Duration::from_millis(args.browser_nav_timeout_ms),
                chrome_path: args
                    .browser_path
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from),
            },
            tools_list,
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
                http_port: 3001,
                request_timeout: Duration::from_secs(30),
                max_request_bytes: 1024 * 1024,
                max_extract_urls: (cpus * 2).max(100),
                max_map_urls: (cpus * 100).clamp(1000, 100_000),
                auth_token: None,
                rate_limit: 0.0,
                enabled_categories: Vec::new(),
                tls_cert: None,
                tls_key: None,
            },
            max_response_bytes: 8 * 1024 * 1024,
            max_redirects: 5,
            allow_private_hosts: false,
            dns_pin: true,
            browser: BrowserSettings::default(),
            tools_list: Arc::new(crate::tools::build_tools_list(&[])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid `Args` for the default (no-API-key) provider, used to
    /// exercise `Config::from_args` paths in tests.
    fn base_args() -> crate::Args {
        crate::Args {
            search_provider: SearchProvider::DuckDuckGo,
            search_api_key: None,
            search_api_url: None,
            limit: 10,
            language: "auto".into(),
            categories: "general".into(),
            time_range: String::new(),
            safe_search: 0,
            engines: "all".into(),
            timeout: 10_000,
            host: "127.0.0.1".into(),
            http_port: 3001,
            stdio: false,
            log_level: "info".into(),
            request_timeout: 30,
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 8 * 1024 * 1024,
            max_redirects: 5,
            allow_private_hosts: false,
            auth_token: None,
            auth_token_file: None,
            max_extract_urls: 0,
            max_map_urls: 0,
            worker_threads: 0,
            rate_limit: 0.0,
            dns_pin: true,
            tls_cert: None,
            tls_key: None,
            browser_path: None,
            browser_max_pages: 0,
            browser_nav_timeout_ms: 30_000,
            browser_disable: false,
            enable_all: false,
            enable_search: false,
            enable_scrape: false,
            enable_fetch: false,
            enable_crawl: false,
        }
    }

    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mcp-auth-test-{tag}-{}-{nanos}", std::process::id()))
    }

    // EXPLOIT REGRESSION (#3): a missing/unreadable --auth-token-file must abort
    // startup, never silently boot the server with authentication disabled.
    #[test]
    fn test_auth_token_file_missing_fails_closed() {
        let mut args = base_args();
        args.auth_token_file = Some("/no/such/path/definitely-missing-token".into());
        let res = Config::from_args(&args);
        assert!(
            matches!(res, Err(crate::errors::WebSearchError::ConfigError(_))),
            "missing auth-token-file must be a hard ConfigError, got {res:?}"
        );
    }

    // An empty token file is also a hard error (otherwise auth is effectively off).
    #[test]
    fn test_auth_token_file_empty_fails_closed() {
        let path = unique_temp_path("empty");
        std::fs::write(&path, "   \n\t  ").unwrap();
        let mut args = base_args();
        args.auth_token_file = Some(path.to_string_lossy().into_owned());
        let res = Config::from_args(&args);
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(res, Err(crate::errors::WebSearchError::ConfigError(_))),
            "empty auth-token-file must be a hard ConfigError, got {res:?}"
        );
    }

    // A valid token file loads (and is trimmed of surrounding whitespace).
    #[test]
    fn test_auth_token_file_valid_loads_trimmed() {
        let path = unique_temp_path("valid");
        std::fs::write(&path, "  s3cret-token\n").unwrap();
        let mut args = base_args();
        args.auth_token_file = Some(path.to_string_lossy().into_owned());
        let cfg = Config::from_args(&args).expect("valid token file should load");
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.server.auth_token.as_deref(), Some("s3cret-token"));
    }

    // --auth-token still takes precedence and never touches the filesystem.
    #[test]
    fn test_auth_token_inline_takes_precedence() {
        let mut args = base_args();
        args.auth_token = Some("inline".into());
        args.auth_token_file = Some("/no/such/path".into());
        let cfg = Config::from_args(&args).expect("inline token should win");
        assert_eq!(cfg.server.auth_token.as_deref(), Some("inline"));
    }

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
