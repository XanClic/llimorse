//! A web_search tool using SearXNG

use crate::CallableTool;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

crate::tool! {
    'name: "web_search";

    /// Look up information on the web. Returns the top results with titles, URLs, and snippets.
    #[derive(Debug)]
    'params: pub struct WebSearchParams {
        /// Search query
        query: String,

        /// Limit the number of results
        max_results: Option<usize>,
    }

    /// Return web search results.
    #[derive(Debug)]
    'result: pub struct WebSearchResults {
        /// Original query
        query: String,

        /// Search results
        results: Vec<WebSearchResult>,
    }

    /// Execute a web search via SearXNG.
    #[derive(Clone, Debug)]
    'state: pub struct WebSearch {
        /// URL base to query the SearXNG instance
        searxng_url_base: String,
    }
}

/// A single web search result
#[derive(Debug, Deserialize, Serialize)]
struct WebSearchResult {
    /// The page title
    title: String,

    /// The source URL
    url: String,

    /// A snippet summarizing the content
    snippet: String,
}

impl WebSearch {
    /// Create a new [`WebSearch`] tool instance, accessing the given URL for queries.
    pub fn new(searxng_url: &str) -> Self {
        WebSearch {
            searxng_url_base: searxng_url.trim_end_matches('/').to_string(),
        }
    }
}

impl CallableTool for WebSearch {
    async fn execute(&self, arguments: WebSearchParams) -> Result<WebSearchResults> {
        let searxng_url = format!(
            "{}/search?q={}&format=json&categories=general",
            self.searxng_url_base,
            urlencoding::encode(&arguments.query),
        );

        let http = reqwest::Client::new();

        let resp: Value = http
            .get(&searxng_url)
            .send()
            .await
            .context("SearXNG request failed")?
            .json()
            .await
            .context("SearXNG response not valid JSON")?;

        let mut results: &[Value] = resp["results"]
            .as_array()
            .ok_or_else(|| anyhow!("No results returned"))?;

        if let Some(max_results) = arguments.max_results
            && results.len() > max_results
        {
            results = &results[..max_results];
        }

        let results = results
            .iter()
            .map(|r| WebSearchResult {
                title: r["title"].as_str().unwrap_or("").to_string(),
                url: r["url"].as_str().unwrap_or("").to_string(),
                snippet: r["content"].as_str().unwrap_or("").to_string(),
            })
            .collect::<Vec<_>>();

        Ok(WebSearchResults {
            query: arguments.query,
            results,
        })
    }
}

impl fmt::Display for WebSearchParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "query={:?}", self.query)?;
        if let Some(max_results) = self.max_results {
            write!(f, " max_results={max_results}")?;
        }
        Ok(())
    }
}

impl fmt::Display for WebSearchResults {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[")?;
        let result_count = self.results.len();
        for (i, result) in self.results.iter().enumerate() {
            if i == result_count - 1 {
                write!(f, "{result}")?;
            } else {
                write!(f, "{result}, ")?;
            }
        }
        write!(f, "]")
    }
}

impl fmt::Display for WebSearchResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.title, self.url)
    }
}
