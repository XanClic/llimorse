//! A web_search tool using SearXNG

use crate::CallableTool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

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

    /// Execute a web search via SearXNG.
    #[derive(Clone, Debug)]
    'state: pub struct WebSearch {
        /// URL base to query the SearXNG instance
        searxng_url_base: String,
    }
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
    async fn execute(&self, arguments: WebSearchParams) -> Result<Value> {
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
            .map(|r| {
                json!({
                    "title": r["title"].as_str().unwrap_or(""),
                    "url": r["url"].as_str().unwrap_or(""),
                    "snippet": r["content"].as_str().unwrap_or(""),
                })
            })
            .collect::<Vec<_>>();

        Ok(json!({ "query": arguments.query, "results": results }))
    }
}
