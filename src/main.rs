use anyhow::Result;
use clap::Parser;
use mcp_web_search::{config, http, server, Args};
use std::sync::Arc;
use tracing::{info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    let args = Args::parse();

    let worker_threads = if args.worker_threads > 0 {
        args.worker_threads
    } else {
        *config::CPU_COUNT
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name("mcp-worker")
        .enable_all()
        .build()?;

    rt.block_on(async { inner_main(args).await })
}

async fn inner_main(args: Args) -> Result<()> {
    init_tracing(&args.log_level)?;

    // Install the rustls `ring` crypto provider as the process default up front
    // (idempotent) so the HTTPS transport can build its TLS config. See src/tls.rs.
    mcp_web_search::tls::ensure_crypto_provider();

    info!(
        name = "mcp-web-search",
        version = env!("CARGO_PKG_VERSION"),
        "Starting server"
    );

    let config = Arc::new(config::Config::from_args(&args)?);
    info!(
        provider = %config.provider,
        allow_private = %config.allow_private_hosts,
        dns_pin = %config.dns_pin,
        "Configuration loaded"
    );
    if config.allow_private_hosts {
        warn!("SSRF guard DISABLED (--allow-private-hosts): scraping may reach internal hosts");
    }
    if config.provider == config::SearchProvider::Google {
        warn!("Google provider sends API key as a URL query parameter. The key may be visible in server access logs and proxy logs.");
    }

    let mcp_server = server::MCPServer::from_arc(Arc::clone(&config));
    info!("Server initialized successfully");

    // Tool exposure: nothing is advertised unless a category was enabled.
    let enabled = &config.server.enabled_categories;
    if enabled.is_empty() {
        warn!(
            "No tool categories enabled — the server will expose ZERO tools. \
             Enable categories with --enable-<category> (e.g. --enable-search --enable-fetch) \
             or expose everything with --enable-all."
        );
    } else {
        let slugs: Vec<&str> = enabled.iter().map(|c| c.slug()).collect();
        let exposed = mcp_web_search::tools::ALL_TOOLS
            .iter()
            .filter(|t| enabled.contains(&t.category))
            .count();
        info!(categories = %slugs.join(", "), tools = exposed, "Tool categories enabled");
    }

    if !is_loopback_host(&config.server.host)
        && config.server.auth_token.is_none()
        && !args.stdio
    {
        warn!(
            host = %config.server.host,
            "Binding to non-loopback host without authentication. Set --auth-token to require a bearer token."
        );
    }

    if args.stdio {
        info!("Running in stdio mode");
        mcp_server.run_stdio().await?;
    } else {
        info!(host = %config.server.host, port = args.http_port, "Starting HTTP server");
        http::create_http_server(Arc::clone(&config), args.http_port)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }

    info!("Server shutdown complete");
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost") || host.starts_with("127.")
}

fn init_tracing(log_level: &str) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_loopback() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.255"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("localhost"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.1"));
        assert!(!is_loopback_host(""));
    }
}
