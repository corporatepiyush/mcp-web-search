use crate::config::BrowserSettings;
use crate::errors::{Result, WebSearchError};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, OwnedSemaphorePermit, Semaphore};

/// Global singleton: initialized lazily on the first browser tool call.
static POOL: OnceCell<Arc<BrowserPool>> = OnceCell::const_new();

/// Returns the global browser pool, lazily initializing it from `settings` on
/// the first call. Returns `None` when browser support is disabled.
pub async fn get_pool(settings: &BrowserSettings) -> Option<Arc<BrowserPool>> {
    if settings.disabled {
        return None;
    }
    let pool = POOL
        .get_or_init(|| {
            let s = settings.clone();
            async move { Arc::new(BrowserPool::new(&s)) }
        })
        .await;
    Some(Arc::clone(pool))
}

// ─── Pool ────────────────────────────────────────────────────────────────────

pub struct BrowserPool {
    /// Bounds simultaneous open tabs (each ≈ 50-200 MB RAM).
    page_sem: Arc<Semaphore>,
    /// Per-navigation timeout used by callers.
    pub nav_timeout: std::time::Duration,
    chrome_path: Option<std::path::PathBuf>,
    /// Owns the `Browser` (not Clone). Mutex serializes page creation and
    /// health checks; it is NOT held while pages are active.
    state: Arc<Mutex<PoolState>>,
}

enum PoolState {
    NotStarted,
    Running(Box<RunningBrowser>),
}

struct RunningBrowser {
    browser: Browser,
    /// Drives the CDP WebSocket connection. When this exits Chrome is gone.
    handler_handle: tokio::task::JoinHandle<()>,
}

/// A browser tab that holds one concurrency slot until dropped / `.close()`d.
#[derive(Debug)]
pub struct PooledPage {
    pub page: Page,
    _permit: OwnedSemaphorePermit,
}

impl BrowserPool {
    pub(crate) fn new(settings: &BrowserSettings) -> Self {
        BrowserPool {
            page_sem: Arc::new(Semaphore::new(settings.max_pages)),
            nav_timeout: settings.nav_timeout,
            chrome_path: settings.chrome_path.clone(),
            state: Arc::new(Mutex::new(PoolState::NotStarted)),
        }
    }

    /// Acquire a page slot (bounded by `max_pages`) and open a blank tab.
    /// The semaphore slot is released when the returned `PooledPage` is dropped
    /// or `.close()`d. Page creation serializes through the state mutex briefly;
    /// once the `Page` is returned the lock is released and all pages run
    /// concurrently.
    pub async fn acquire_page(&self) -> Result<PooledPage> {
        // Acquire the concurrency permit BEFORE taking the state lock so callers
        // queue on the semaphore, not on the mutex.
        let permit = Arc::clone(&self.page_sem)
            .acquire_owned()
            .await
            .map_err(|_| WebSearchError::HttpError("browser page semaphore closed".into()))?;

        // Hold the state lock only for launch + page creation.
        let page: Page = {
            let mut state = self.state.lock().await;

            // Reset if the CDP handler task has exited (browser crashed/quit).
            if let PoolState::Running(ref rb) = *state && rb.handler_handle.is_finished() {
                tracing::warn!("Browser CDP handler exited; relaunching on next request");
                *state = PoolState::NotStarted;
            }

            // Launch Chrome on first use (or after a crash).
            if matches!(*state, PoolState::NotStarted) {
                self.do_launch(&mut state).await?;
            }

            match *state {
                PoolState::Running(ref mut rb) => {
                    rb.browser.new_page("about:blank").await.map_err(|e| {
                        WebSearchError::HttpError(format!("failed to open browser tab: {e}"))
                    })?
                }
                PoolState::NotStarted => {
                    return Err(WebSearchError::HttpError(
                        "browser failed to launch".into(),
                    ));
                }
            }
            // state lock released here
        };

        Ok(PooledPage { page, _permit: permit })
    }

    /// Launch Chrome and populate `state`. Caller must hold the state lock.
    async fn do_launch(&self, state: &mut PoolState) -> Result<()> {
        // Security / performance defaults that work in containers, CI, and on
        // developer machines.
        let mut builder = BrowserConfig::builder()
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-setuid-sandbox")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-background-timer-throttling")
            .arg("--disable-client-side-phishing-detection")
            .arg("--disable-default-apps")
            .arg("--disable-hang-monitor")
            .arg("--disable-prompt-on-repost")
            .arg("--disable-sync")
            .arg("--disable-translate")
            .arg("--metrics-recording-only")
            .arg("--no-first-run")
            .arg("--safebrowsing-disable-auto-update")
            .arg("--disable-software-rasterizer")
            .arg("--mute-audio")
            // Block popups to prevent a malicious page from opening a second
            // navigation that bypasses our per-URL SSRF guard.
            .arg("--block-new-web-contents")
            .window_size(1280, 800);

        if let Some(ref p) = self.chrome_path {
            builder = builder.chrome_executable(p);
        }

        let config = builder
            .build()
            .map_err(|e| WebSearchError::ConfigError(format!("browser config error: {e}")))?;

        let (browser, mut handler) = Browser::launch(config).await.map_err(|e| {
            WebSearchError::HttpError(format!(
                "headless browser unavailable: {e}. \
                 Install Chrome or Chromium and ensure it is on PATH, \
                 or set --browser-path to the binary. \
                 Use --browser-disable to suppress this error and fall back to \
                 non-JS scraping tools (web_scrape, web_fetch)."
            ))
        })?;

        let handle = tokio::spawn(async move {
            // The handler stream MUST be driven for CDP to function. It ends
            // when Chrome exits or the WebSocket closes.
            while handler.next().await.is_some() {}
            tracing::warn!("Browser CDP handler stream ended");
        });

        *state = PoolState::Running(Box::new(RunningBrowser {
            browser,
            handler_handle: handle,
        }));
        tracing::info!("Headless Chrome launched");
        Ok(())
    }

    /// Available page slots — for testing only.
    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.page_sem.available_permits()
    }
}

impl PooledPage {
    /// Close the underlying tab and release the pool slot. Close errors are
    /// swallowed; the semaphore permit is always released via `Drop`.
    pub async fn close(self) {
        let _ = self.page.close().await;
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserSettings;
    use std::time::Duration;

    fn settings(max_pages: usize) -> BrowserSettings {
        BrowserSettings {
            disabled: false,
            max_pages,
            nav_timeout: Duration::from_secs(30),
            chrome_path: None,
        }
    }

    // ── Pool construction ──────────────────────────────────────────────────

    #[test]
    fn test_pool_semaphore_capacity() {
        let pool = BrowserPool::new(&settings(5));
        assert_eq!(pool.available_permits(), 5);
        assert_eq!(pool.nav_timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_pool_nav_timeout_propagated() {
        let s = BrowserSettings {
            nav_timeout: Duration::from_secs(60),
            ..settings(4)
        };
        let pool = BrowserPool::new(&s);
        assert_eq!(pool.nav_timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_pool_chrome_path_propagated() {
        let s = BrowserSettings {
            chrome_path: Some(std::path::PathBuf::from("/usr/bin/chromium")),
            ..settings(4)
        };
        let pool = BrowserPool::new(&s);
        assert_eq!(
            pool.chrome_path,
            Some(std::path::PathBuf::from("/usr/bin/chromium"))
        );
    }

    // ── Semaphore concurrency bound ────────────────────────────────────────

    #[tokio::test]
    async fn test_semaphore_bounds_concurrency() {
        let pool = Arc::new(BrowserPool::new(&settings(2)));
        assert_eq!(pool.available_permits(), 2);

        let p1 = Arc::clone(&pool.page_sem).acquire_owned().await.unwrap();
        let p2 = Arc::clone(&pool.page_sem).acquire_owned().await.unwrap();
        assert_eq!(pool.available_permits(), 0);

        // Third acquire must block — not immediately available.
        assert!(pool.page_sem.try_acquire().is_err());

        drop(p1);
        drop(p2);
        assert_eq!(pool.available_permits(), 2);
    }

    #[tokio::test]
    async fn test_semaphore_released_on_drop() {
        let pool = BrowserPool::new(&settings(1));
        {
            let _p = Arc::clone(&pool.page_sem).acquire_owned().await.unwrap();
            assert_eq!(pool.available_permits(), 0);
        }
        assert_eq!(pool.available_permits(), 1);
    }

    // ── Bad chrome path → error ────────────────────────────────────────────

    #[tokio::test]
    async fn test_bad_chrome_path_returns_http_error() {
        let s = BrowserSettings {
            chrome_path: Some(std::path::PathBuf::from("/no/such/chrome-binary")),
            ..settings(4)
        };
        let pool = BrowserPool::new(&s);
        let result = pool.acquire_page().await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("headless browser unavailable") || msg.contains("failed"),
            "unexpected error: {msg}"
        );
    }

    // ── Live browser (skipped without Chrome) ─────────────────────────────

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_acquire_and_close_page() {
        let pool = BrowserPool::new(&settings(2));
        let pg = pool.acquire_page().await.unwrap();
        assert_eq!(pool.available_permits(), 1);
        pg.close().await;
        assert_eq!(pool.available_permits(), 2);
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_concurrent_pages_bounded_to_max() {
        use tokio::sync::Barrier;

        let pool = Arc::new(BrowserPool::new(&settings(2)));
        let barrier = Arc::new(Barrier::new(2));

        let (pool1, pool2) = (Arc::clone(&pool), Arc::clone(&pool));
        let (b1, b2) = (Arc::clone(&barrier), Arc::clone(&barrier));

        let h1 = tokio::spawn(async move {
            let pg = pool1.acquire_page().await.unwrap();
            b1.wait().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            pg.close().await;
        });
        let h2 = tokio::spawn(async move {
            let pg = pool2.acquire_page().await.unwrap();
            b2.wait().await;
            pg.close().await;
        });

        h1.await.unwrap();
        h2.await.unwrap();
        assert_eq!(pool.available_permits(), 2);
    }

    #[tokio::test]
    #[ignore = "requires Chrome/Chromium on PATH"]
    async fn test_page_content_is_accessible() {
        let pool = BrowserPool::new(&settings(1));
        let pg = pool.acquire_page().await.unwrap();
        // Navigate to a public URL
        pg.page.goto("https://example.com").await.unwrap();
        let html = pg.page.content().await.unwrap();
        assert!(!html.is_empty());
        pg.close().await;
    }
}
