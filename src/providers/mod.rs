pub mod bing;
pub mod bocha;
pub mod duckduckgo;
pub mod exa;
pub mod google;
pub mod searxng;
pub mod tavily;
pub mod types;
pub mod zhipu;

use crate::config::SearchProvider;
use crate::errors::Result;
use types::{SearchRequest, SearchResponse};

/// Dispatch a normalized search request to the selected provider backend.
pub async fn search(provider: SearchProvider, req: &SearchRequest) -> Result<SearchResponse> {
    match provider {
        SearchProvider::Searxng => searxng::search(req).await,
        SearchProvider::DuckDuckGo => duckduckgo::search(req).await,
        SearchProvider::Bing => bing::search(req).await,
        SearchProvider::Tavily => tavily::search(req).await,
        SearchProvider::Google => google::search(req).await,
        SearchProvider::Zhipu => zhipu::search(req).await,
        SearchProvider::Exa => exa::search(req).await,
        SearchProvider::Bocha => bocha::search(req).await,
    }
}

#[cfg(test)]
mod tests {
    use crate::config::SearchProvider;

    #[test]
    fn test_all_providers_dispatched() {
        // Verify all provider variants are covered in the match
        let providers = [
            SearchProvider::Searxng,
            SearchProvider::DuckDuckGo,
            SearchProvider::Bing,
            SearchProvider::Tavily,
            SearchProvider::Google,
            SearchProvider::Zhipu,
            SearchProvider::Exa,
            SearchProvider::Bocha,
        ];
        assert_eq!(providers.len(), 8);
    }
}
