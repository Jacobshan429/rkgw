use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// A single web search result from Exa API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResult {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub highlights: Vec<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<String>,
}

/// Response from Exa search API.
#[derive(Debug, Clone, Deserialize)]
pub struct ExaSearchResponse {
    pub results: Vec<WebSearchResult>,
}

/// Client for the Exa search API.
pub struct ExaClient {
    client: Client,
    api_key: String,
    max_results: usize,
}

impl ExaClient {
    pub fn new(api_key: String, max_results: usize) -> Self {
        Self {
            client: Client::new(),
            api_key,
            max_results,
        }
    }

    /// Execute a web search query via Exa API.
    pub async fn search(&self, query: &str) -> Result<Vec<WebSearchResult>> {
        let body = serde_json::json!({
            "query": query,
            "type": "auto",
            "numResults": self.max_results,
            "contents": {
                "text": {"maxCharacters": 3000},
                "highlights": {"numSentences": 3}
            }
        });

        debug!(query = %query, "Executing Exa web search");

        let response = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Exa API error {}: {}", status, error_text);
        }

        let exa_response: ExaSearchResponse = response.json().await?;
        debug!(count = exa_response.results.len(), "Exa search returned results");
        Ok(exa_response.results)
    }

    /// Format search results as text for inclusion in a tool result.
    pub fn format_results_as_text(query: &str, results: &[WebSearchResult]) -> String {
        let mut text = format!("Search results for \"{}\":\n\n", query);
        for (i, result) in results.iter().enumerate() {
            text.push_str(&format!("{}. {} ({})\n", i + 1, result.title, result.url));
            if !result.highlights.is_empty() {
                for highlight in &result.highlights {
                    text.push_str(&format!("   {}\n", highlight));
                }
            } else if !result.text.is_empty() {
                let excerpt = if result.text.len() > 500 {
                    &result.text[..500]
                } else {
                    &result.text
                };
                text.push_str(&format!("   {}\n", excerpt));
            }
            text.push('\n');
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_results_as_text() {
        let results = vec![
            WebSearchResult {
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
                text: "Some content here".to_string(),
                highlights: vec!["Key finding".to_string()],
                published_date: Some("2025-01-01".to_string()),
            },
            WebSearchResult {
                url: "https://other.com".to_string(),
                title: "Other Site".to_string(),
                text: "More content".to_string(),
                highlights: vec![],
                published_date: None,
            },
        ];

        let text = ExaClient::format_results_as_text("test query", &results);
        assert!(text.contains("Search results for \"test query\""));
        assert!(text.contains("1. Example (https://example.com)"));
        assert!(text.contains("Key finding"));
        assert!(text.contains("2. Other Site (https://other.com)"));
        assert!(text.contains("More content"));
    }

    #[test]
    fn test_format_results_empty() {
        let text = ExaClient::format_results_as_text("test", &[]);
        assert!(text.contains("Search results for \"test\""));
    }

    #[test]
    fn test_exa_client_new() {
        let client = ExaClient::new("test-key".to_string(), 5);
        assert_eq!(client.api_key, "test-key");
        assert_eq!(client.max_results, 5);
    }
}
