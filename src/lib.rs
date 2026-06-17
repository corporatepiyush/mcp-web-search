pub mod actions;
pub mod client;
pub mod config;
pub mod errors;
pub mod http;
pub mod protocol;
pub mod providers;
pub mod server;
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

    #[arg(short = 'p', long, default_value = "3000")]
    pub port: u16,

    #[arg(long, default_value = "3001")]
    pub http_port: u16,

    #[arg(long)]
    pub stdio: bool,

    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    #[arg(long, default_value = "30")]
    pub request_timeout: u64,

    #[arg(long, default_value = "16777216")]
    pub max_request_bytes: usize,

    #[arg(long, default_value = "8388608")]
    pub max_response_bytes: usize,

    #[arg(long, default_value = "5")]
    pub max_redirects: usize,

    #[arg(long)]
    pub allow_private_hosts: bool,

    #[arg(long)]
    pub auth_token: Option<String>,

    #[arg(long, default_value_t = 0, help = "Max concurrent TCP connections (0 = auto-scale to num_cpus * 256)")]
    pub max_connections: usize,

    #[arg(long, default_value_t = 0, help = "Max URLs for web_extract (0 = auto-scale to num_cpus * 2)")]
    pub max_extract_urls: usize,

    #[arg(long, default_value_t = 0, help = "Max URLs for web_map (0 = auto-scale to num_cpus * 100)")]
    pub max_map_urls: usize,

    #[arg(long, default_value_t = 0, help = "Tokio worker threads (0 = auto-detect num_cpus)")]
    pub worker_threads: usize,
}
