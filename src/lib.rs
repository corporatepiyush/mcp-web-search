pub mod actions;
pub mod browser;
pub mod client;
pub mod config;
pub mod errors;
pub mod http;
pub mod protocol;
pub mod providers;
pub mod ratelimit;
pub mod server;
pub mod tls;
pub mod tools;
pub mod validation;

pub use errors::WebSearchError;

use clap::Parser;
use config::SearchProvider;

#[derive(Parser, Debug)]
#[command(name = "MCP Web Search Server")]
#[command(
    about = "High-performance, SSRF-hardened Model Context Protocol server for web search and scraping",
    long_about = None
)]
pub struct Args {
    #[arg(long, env = "SEARCH_PROVIDER", default_value = "duckduckgo")]
    pub search_provider: SearchProvider,

    #[arg(long, env = "SEARCH_API_KEY")]
    pub search_api_key: Option<String>,

    #[arg(long, env = "SEARCH_API_URL")]
    pub search_api_url: Option<String>,

    #[arg(long, env = "LIMIT", default_value = "10")]
    pub limit: usize,

    #[arg(long, env = "LANGUAGE", default_value = "auto")]
    pub language: String,

    #[arg(long, env = "CATEGORIES", default_value = "general")]
    pub categories: String,

    #[arg(long, env = "TIME_RANGE", default_value = "")]
    pub time_range: String,

    #[arg(long, env = "SAFE_SEARCH", default_value = "0")]
    pub safe_search: u8,

    #[arg(long, env = "ENGINES", default_value = "all")]
    pub engines: String,

    #[arg(long, env = "TIMEOUT", default_value = "10000")]
    pub timeout: u64,

    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: String,

    #[arg(long, default_value = "3001")]
    pub http_port: u16,

    #[arg(long)]
    pub stdio: bool,

    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    #[arg(long, default_value = "30")]
    pub request_timeout: u64,

    #[arg(long, default_value = "1048576")]
    pub max_request_bytes: usize,

    #[arg(long, default_value = "8388608")]
    pub max_response_bytes: usize,

    #[arg(long, default_value = "5")]
    pub max_redirects: usize,

    #[arg(long)]
    pub allow_private_hosts: bool,

    #[arg(long)]
    pub auth_token: Option<String>,

    #[arg(long, help = "Read auth token from file (more secure than --auth-token which is visible in process list)")]
    pub auth_token_file: Option<String>,

    #[arg(long, default_value_t = 0, help = "Max URLs for web_extract (0 = auto-scale to num_cpus * 2)")]
    pub max_extract_urls: usize,

    #[arg(long, default_value_t = 0, help = "Max URLs for web_map (0 = auto-scale to num_cpus * 100)")]
    pub max_map_urls: usize,

    #[arg(long, default_value_t = 0, help = "Tokio worker threads (0 = auto-detect num_cpus)")]
    pub worker_threads: usize,

    #[arg(long, default_value_t = 0.0, help = "Max requests per second (0 = unlimited)")]
    pub rate_limit: f64,

    #[arg(long, default_value_t = true, help = "Pin resolved DNS to prevent rebinding attacks (adds ~10ms per request). Use --no-dns-pin to disable")]
    pub dns_pin: bool,

    #[arg(long, env = "MCP_TLS_CERT", help = "PEM certificate chain to serve the HTTP transport over TLS (HTTPS). Requires --tls-key; plaintext when unset")]
    pub tls_cert: Option<String>,

    #[arg(long, env = "MCP_TLS_KEY", help = "PEM private key matching --tls-cert")]
    pub tls_key: Option<String>,

    #[arg(long, env = "BROWSER_PATH", help = "Path to Chrome/Chromium binary for headless browser tools (auto-detected from PATH when unset)")]
    pub browser_path: Option<String>,

    #[arg(long, env = "BROWSER_MAX_PAGES", default_value_t = 0, help = "Max concurrent browser pages (0 = 2×num_cpus, min 4). Each page uses ~50-200 MB RAM")]
    pub browser_max_pages: usize,

    #[arg(long, env = "BROWSER_NAV_TIMEOUT_MS", default_value_t = 30_000, help = "Per-navigation timeout for browser tools in milliseconds")]
    pub browser_nav_timeout_ms: u64,

    #[arg(long, env = "BROWSER_DISABLE", help = "Disable headless browser tools entirely; browser_scrape and browser_screenshot return errors")]
    pub browser_disable: bool,

    // ── Tool exposure ────────────────────────────────────────────────────
    // No tools are exposed unless explicitly enabled. Each flag turns on one
    // category (hidden from tools/list and rejected from tools/call when its
    // category is disabled). Use --enable-all for every category at once.
    #[arg(long, help = "Expose ALL tool categories (overrides the individual --enable-* flags)")]
    pub enable_all: bool,

    #[arg(long, help = "Enable Search tools: web_search, web_search_scrape")]
    pub enable_search: bool,

    #[arg(long, help = "Enable Scrape tools: web_scrape, web_extract, browser_scrape, browser_screenshot")]
    pub enable_scrape: bool,

    #[arg(long, help = "Enable Fetch tools: web_fetch, web_fetch_text, web_fetch_headers")]
    pub enable_fetch: bool,

    #[arg(long, help = "Enable Crawl tools: web_map, web_sitemap, web_check_links")]
    pub enable_crawl: bool,
}

impl Args {
    /// Resolve the set of enabled tool categories from the `--enable-*` flags.
    /// `--enable-all` turns on every category; otherwise only the categories
    /// whose individual flag is set. With no flags, the result is empty and no
    /// tools are exposed.
    pub fn enabled_categories(&self) -> Vec<tools::ToolCategory> {
        use tools::ToolCategory as C;
        if self.enable_all {
            return C::ALL.to_vec();
        }
        let mut cats = Vec::new();
        let mut push = |on: bool, cat: C| {
            if on {
                cats.push(cat);
            }
        };
        push(self.enable_search, C::Search);
        push(self.enable_scrape, C::Scrape);
        push(self.enable_fetch, C::Fetch);
        push(self.enable_crawl, C::Crawl);
        cats
    }
}
