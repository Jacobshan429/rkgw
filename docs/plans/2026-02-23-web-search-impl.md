# Web Search Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make web search work through the gateway by intercepting server-side tool calls and executing searches via Exa API.

**Architecture:** The gateway converts Anthropic's `web_search` server-side tool into a regular Kiro tool. When the model calls it, the gateway executes the search via Exa, feeds results back to Kiro in a second request, then returns the full response (including `server_tool_use` and `web_search_tool_result` blocks) to the client.

**Tech Stack:** Rust, reqwest (HTTP client for Exa), serde_json, tokio

**Design doc:** `docs/plans/2026-02-23-web-search-design.md`

---

### Task 1: Add Exa config fields

**Files:**
- Modify: `src/config.rs:76-119` (Config struct)
- Modify: `src/config.rs:146-223` (Config::load)
- Modify: `src/config.rs:447-485` (create_test_config helper)

**Step 1: Write the failing test**

Add to the bottom of `src/config.rs` tests module:

```rust
#[test]
fn test_web_search_config_defaults() {
    let (config, _tmp) = create_test_config("127.0.0.1", false);
    assert_eq!(config.exa_api_key, None);
    assert_eq!(config.web_search_max_results, 5);
    assert_eq!(config.web_search_max_iterations, 3);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib test_web_search_config_defaults`
Expected: FAIL — fields don't exist on Config

**Step 3: Add fields to Config struct**

In `src/config.rs`, add after line 118 (`tls_key_path`), before the closing brace:

```rust
    // Web search
    pub exa_api_key: Option<String>,
    pub web_search_max_results: usize,
    pub web_search_max_iterations: usize,
```

In `Config::load()`, add after the TLS section (after line 222), before `};`:

```rust
            // Web search
            exa_api_key: std::env::var("EXA_API_KEY").ok(),
            web_search_max_results: std::env::var("WEB_SEARCH_MAX_RESULTS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            web_search_max_iterations: std::env::var("WEB_SEARCH_MAX_ITERATIONS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3),
```

In `create_test_config` helper, add after `tls_key_path: None,`:

```rust
            exa_api_key: None,
            web_search_max_results: 5,
            web_search_max_iterations: 3,
```

Also update the `create_test_config` in `src/routes/mod.rs:635-658` and `src/converters/openai_to_kiro.rs:551-576` with the same three fields.

**Step 4: Run test to verify it passes**

Run: `cargo test --lib test_web_search_config_defaults`
Expected: PASS

**Step 5: Commit**

```bash
git add src/config.rs src/routes/mod.rs src/converters/openai_to_kiro.rs
git commit -m "feat: add Exa web search config fields"
```

---

### Task 2: Create Exa API client module

**Files:**
- Create: `src/web_search.rs`
- Modify: `src/lib.rs:17` (add module)
- Modify: `src/main.rs:22` (add module)

**Step 1: Write the failing test**

Create `src/web_search.rs` with the test first:

```rust
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
                // Use first 500 chars of text if no highlights
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
```

**Step 2: Register the module**

In `src/lib.rs`, add after line 17 (`pub mod utils;`):
```rust
pub mod web_search;
```

In `src/main.rs`, add after line 22 (`mod utils;`):
```rust
mod web_search;
```

**Step 3: Run tests to verify they pass**

Run: `cargo test --lib web_search::`
Expected: PASS (3 tests)

**Step 4: Commit**

```bash
git add src/web_search.rs src/lib.rs src/main.rs
git commit -m "feat: add Exa API client for web search"
```

---

### Task 3: Add server-side content block types to Anthropic models

**Files:**
- Modify: `src/models/anthropic.rs:10-36` (ContentBlock enum)

**Step 1: Write the failing test**

Add to the end of `src/models/anthropic.rs` (create a tests module):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_server_tool_use() {
        let json = r#"{"type": "server_tool_use", "id": "srvtoolu_123", "name": "web_search", "input": {"query": "rust"}}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::ServerToolUse { id, name, input } => {
                assert_eq!(id, "srvtoolu_123");
                assert_eq!(name, "web_search");
                assert_eq!(input["query"], "rust");
            }
            _ => panic!("Expected ServerToolUse"),
        }
    }

    #[test]
    fn test_deserialize_web_search_tool_result() {
        let json = r#"{"type": "web_search_tool_result", "tool_use_id": "srvtoolu_123", "content": [{"type": "web_search_result", "url": "https://example.com", "title": "Example"}]}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ContentBlock::WebSearchToolResult { tool_use_id, content } => {
                assert_eq!(tool_use_id, "srvtoolu_123");
                assert!(content.is_array());
            }
            _ => panic!("Expected WebSearchToolResult"),
        }
    }

    #[test]
    fn test_serialize_server_tool_use() {
        let block = ContentBlock::ServerToolUse {
            id: "srvtoolu_123".to_string(),
            name: "web_search".to_string(),
            input: serde_json::json!({"query": "test"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "server_tool_use");
        assert_eq!(json["id"], "srvtoolu_123");
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib models::anthropic::tests::`
Expected: FAIL — variants don't exist

**Step 3: Add variants to ContentBlock**

In `src/models/anthropic.rs`, add two new variants to `ContentBlock` enum after `ToolResult` (before the closing `}`):

```rust
    ServerToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: serde_json::Value,
    },
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib models::anthropic::tests::`
Expected: PASS

**Step 5: Commit**

```bash
git add src/models/anthropic.rs
git commit -m "feat: add ServerToolUse and WebSearchToolResult content blocks"
```

---

### Task 4: Convert web_search server-side tool to regular Kiro tool

**Files:**
- Modify: `src/converters/anthropic_to_kiro.rs:254-280` (convert_anthropic_tools)

**Step 1: Update the existing filter test**

In `src/converters/anthropic_to_kiro.rs`, update `test_convert_anthropic_tools_filters_server_side_tools` (around line 825):

```rust
#[test]
fn test_convert_anthropic_tools_converts_web_search_to_regular_tool() {
    let tools = vec![
        AnthropicTool {
            name: "web_search".to_string(),
            description: None,
            input_schema: None,
            tool_type: Some("web_search_20250305".to_string()),
        },
        AnthropicTool {
            name: "get_weather".to_string(),
            description: Some("Get weather".to_string()),
            input_schema: Some(json!({"type": "object"})),
            tool_type: None,
        },
    ];

    let unified = convert_anthropic_tools(&Some(tools));
    assert!(unified.is_some());
    let tools = unified.unwrap();
    // web_search should now be included as a regular tool, not filtered
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "web_search");
    assert!(tools[0].input_schema.is_some());
    assert_eq!(tools[1].name, "get_weather");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --lib test_convert_anthropic_tools_converts_web_search`
Expected: FAIL — web_search is still filtered out (len is 1)

**Step 3: Modify convert_anthropic_tools to convert web_search**

Replace the `convert_anthropic_tools` function in `src/converters/anthropic_to_kiro.rs`:

```rust
/// Converts Anthropic tools to unified format.
///
/// Server-side tools with `web_search` in the name are converted to regular tools
/// with a synthetic input_schema so Kiro can present them to the model.
/// Other server-side tools without `input_schema` are filtered out.
pub fn convert_anthropic_tools(tools: &Option<Vec<AnthropicTool>>) -> Option<Vec<UnifiedTool>> {
    tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(|tool| {
                if tool.input_schema.is_some() {
                    // Regular tool — pass through
                    Some(UnifiedTool {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    })
                } else if tool.name == "web_search" || tool.tool_type.as_ref().is_some_and(|t| t.starts_with("web_search")) {
                    // web_search server-side tool — convert to regular tool
                    debug!(
                        "Converting server-side tool '{}' (type: {:?}) to regular tool for Kiro",
                        tool.name, tool.tool_type
                    );
                    Some(UnifiedTool {
                        name: "web_search".to_string(),
                        description: Some(
                            "Search the web for current information. Use this when you need up-to-date data, news, or facts beyond your knowledge cutoff.".to_string()
                        ),
                        input_schema: Some(serde_json::json!({
                            "type": "object",
                            "properties": {
                                "query": {
                                    "type": "string",
                                    "description": "The search query"
                                }
                            },
                            "required": ["query"]
                        })),
                    })
                } else {
                    // Unknown server-side tool — filter out
                    debug!(
                        "Filtering out unsupported server-side tool '{}' (type: {:?})",
                        tool.name, tool.tool_type
                    );
                    None
                }
            })
            .collect()
    })
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib converters::anthropic_to_kiro::tests::`
Expected: PASS (the old filter test is replaced by the new one)

**Step 5: Commit**

```bash
git add src/converters/anthropic_to_kiro.rs
git commit -m "feat: convert web_search server-side tool to regular Kiro tool"
```

---

### Task 5: Handle server_tool_use / web_search_tool_result in multi-turn history

**Files:**
- Modify: `src/converters/anthropic_to_kiro.rs` (convert_anthropic_messages)

Claude Code sends back `server_tool_use` and `web_search_tool_result` blocks in conversation history from previous turns. These need to be converted to regular `tool_use` / `tool_result` for Kiro history. This is handled via ContentBlock deserialization — since ContentBlock now has `ServerToolUse` and `WebSearchToolResult` variants, the message converter must map them.

**Step 1: Write a test**

```rust
#[test]
fn test_convert_messages_with_server_tool_use_in_history() {
    let messages = vec![
        AnthropicMessage {
            role: "user".to_string(),
            content: json!("What's the latest on Rust?"),
        },
        AnthropicMessage {
            role: "assistant".to_string(),
            content: json!([
                {"type": "text", "text": "Let me search for that."},
                {"type": "server_tool_use", "id": "srvtoolu_123", "name": "web_search", "input": {"query": "latest Rust news"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_123", "content": [{"type": "web_search_result", "url": "https://example.com", "title": "Rust News"}]},
                {"type": "text", "text": "Based on search results, here is the latest."}
            ]),
        },
        AnthropicMessage {
            role: "user".to_string(),
            content: json!("Tell me more"),
        },
    ];

    let converted = convert_anthropic_messages(&messages);
    // Should not panic; server_tool_use and web_search_tool_result blocks should be handled gracefully
    assert_eq!(converted.len(), 3);
}
```

**Step 2: Run test and check if it passes or fails**

Run: `cargo test --lib test_convert_messages_with_server_tool_use_in_history`

If it fails, update the message converter to handle the new ContentBlock variants (map `ServerToolUse` to `ToolUse` and `WebSearchToolResult` to `ToolResult` or skip them). The exact fix depends on how `convert_anthropic_messages` processes content blocks — read the function and handle the new variants.

**Step 3: Commit**

```bash
git add src/converters/anthropic_to_kiro.rs
git commit -m "feat: handle server_tool_use blocks in multi-turn history"
```

---

### Task 6: Add ExaClient to AppState and wire in main.rs

**Files:**
- Modify: `src/routes/mod.rs:36-45` (AppState struct)
- Modify: `src/main.rs:166-174` (AppState initialization)

**Step 1: Add ExaClient to AppState**

In `src/routes/mod.rs`, add the import at the top:
```rust
use crate::web_search::ExaClient;
```

Add to AppState struct (after `metrics`):
```rust
    pub exa_client: Option<Arc<ExaClient>>,
```

Update `create_test_state` in tests (after `metrics`):
```rust
            exa_client: None,
```

**Step 2: Initialize in main.rs**

In `src/main.rs`, after the metrics initialization (around line 164), add:

```rust
    let exa_client = config.exa_api_key.as_ref().map(|key| {
        tracing::info!("✅ Exa web search client initialized");
        Arc::new(web_search::ExaClient::new(key.clone(), config.web_search_max_results))
    });
    if exa_client.is_none() {
        tracing::info!("ℹ️  Web search disabled (no EXA_API_KEY configured)");
    }
```

Add to app_state (after `metrics`):
```rust
        exa_client,
```

**Step 3: Verify compilation**

Run: `cargo build`
Expected: Success

**Step 4: Commit**

```bash
git add src/routes/mod.rs src/main.rs
git commit -m "feat: wire ExaClient into AppState"
```

---

### Task 7: Implement the agentic web search loop

This is the core task. The `anthropic_messages_handler` needs an agentic loop that:
1. Detects if web_search is requested
2. Collects the first Kiro response
3. If model calls web_search → execute via Exa → send results back to Kiro
4. Assembles the final response with server_tool_use blocks

**Files:**
- Create: `src/web_search_loop.rs` (agentic loop logic, separate from route handler)
- Modify: `src/lib.rs` (add module)
- Modify: `src/main.rs` (add module)
- Modify: `src/routes/mod.rs` (call loop from handler)

**Step 1: Create `src/web_search_loop.rs` with the loop logic**

This module contains:
- `has_web_search_tool(tools)` — check if request includes web_search
- `WebSearchLoop` struct — orchestrates the agentic loop
- `execute()` method — runs the loop and returns assembled content blocks

```rust
use std::sync::Arc;

use serde_json::{json, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::ApiError;
use crate::streaming::{self, StreamResult};
use crate::web_search::{ExaClient, WebSearchResult};

/// Check if the request tools include a web_search server-side tool.
pub fn has_web_search_tool(tools: &Option<Vec<crate::models::anthropic::AnthropicTool>>) -> bool {
    tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|t| {
            t.name == "web_search"
                && t.input_schema.is_none()
        })
    })
}

/// Result of the web search agentic loop.
pub struct WebSearchLoopResult {
    /// All content blocks to return to the client (text + server_tool_use + web_search_tool_result).
    pub content_blocks: Vec<Value>,
    /// Final stop reason.
    pub stop_reason: String,
    /// Accumulated usage.
    pub input_tokens: i32,
    pub output_tokens: i32,
}

/// Execute a Kiro request and collect the full response (non-streaming).
/// Returns the parsed StreamResult.
async fn collect_kiro_response(
    http_client: &crate::http_client::KiroHttpClient,
    auth_manager: &crate::auth::AuthManager,
    region: &str,
    payload: &Value,
    first_token_timeout: u64,
    model: &str,
) -> Result<(StreamResult, Value), ApiError> {
    let access_token = auth_manager.get_access_token().await
        .map_err(|e| ApiError::AuthError(format!("Failed to get access token: {}", e)))?;

    let kiro_api_url = format!(
        "https://codewhisperer.{}.amazonaws.com/generateAssistantResponse",
        region
    );

    let req = http_client
        .client()
        .post(&kiro_api_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(payload)
        .build()
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("Failed to build request: {}", e)))?;

    let response = http_client.request_with_retry(req).await?;

    // Use the internal stream collector to get content + tool_calls
    let stream = streaming::parse_kiro_stream(response, first_token_timeout).await?;
    let result = streaming::collect_stream_result(stream).await?;

    Ok((result, payload.clone()))
}

/// Run the agentic web search loop.
///
/// 1. Send initial request to Kiro
/// 2. If model calls web_search, execute via Exa
/// 3. Build follow-up request with results
/// 4. Repeat until no more web_search calls or max iterations
pub async fn run_web_search_loop(
    http_client: &crate::http_client::KiroHttpClient,
    auth_manager: &crate::auth::AuthManager,
    exa_client: &ExaClient,
    region: &str,
    initial_payload: &Value,
    first_token_timeout: u64,
    model: &str,
    max_iterations: usize,
) -> Result<WebSearchLoopResult, ApiError> {
    let mut all_content_blocks: Vec<Value> = Vec::new();
    let mut total_input_tokens = 0i32;
    let mut total_output_tokens = 0i32;
    let mut current_payload = initial_payload.clone();

    for iteration in 0..max_iterations {
        info!(iteration = iteration, "Web search loop iteration");

        let (result, _) = collect_kiro_response(
            http_client, auth_manager, region, &current_payload,
            first_token_timeout, model,
        ).await?;

        // Track usage
        if let Some(usage) = &result.usage {
            total_input_tokens += usage.input_tokens;
            total_output_tokens += usage.output_tokens;
        }

        // Add any text content from this iteration
        if !result.content.is_empty() {
            all_content_blocks.push(json!({"type": "text", "text": result.content}));
        }

        // Check for web_search tool calls
        let web_search_calls: Vec<_> = result.tool_calls.iter()
            .filter(|tc| tc.name == "web_search")
            .collect();

        if web_search_calls.is_empty() {
            // No more searches — we're done
            debug!("No web_search calls in iteration {}, loop complete", iteration);
            return Ok(WebSearchLoopResult {
                content_blocks: all_content_blocks,
                stop_reason: "end_turn".to_string(),
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
            });
        }

        // Process each web_search call
        for tool_call in &web_search_calls {
            let query = tool_call.input.get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("");

            let server_tool_id = format!("srvtoolu_{}", Uuid::new_v4().to_string().replace('-', "")[..24].to_string());

            // Emit server_tool_use block
            all_content_blocks.push(json!({
                "type": "server_tool_use",
                "id": server_tool_id,
                "name": "web_search",
                "input": {"query": query}
            }));

            // Execute search via Exa
            let search_results = match exa_client.search(query).await {
                Ok(results) => results,
                Err(e) => {
                    warn!(error = %e, "Exa search failed");
                    // Emit error result matching Anthropic format
                    all_content_blocks.push(json!({
                        "type": "web_search_tool_result",
                        "tool_use_id": server_tool_id,
                        "content": {
                            "type": "web_search_tool_result_error",
                            "error_code": "unavailable"
                        }
                    }));
                    continue;
                }
            };

            // Emit web_search_tool_result block
            let result_content: Vec<Value> = search_results.iter().map(|r| {
                json!({
                    "type": "web_search_result",
                    "url": r.url,
                    "title": r.title,
                    "page_age": r.published_date
                })
            }).collect();

            all_content_blocks.push(json!({
                "type": "web_search_tool_result",
                "tool_use_id": server_tool_id,
                "content": result_content
            }));

            // Build tool result text for Kiro follow-up
            let result_text = ExaClient::format_results_as_text(query, &search_results);

            // Build follow-up payload: add assistant response + tool result to history
            current_payload = build_followup_payload(
                &current_payload,
                &result.content,
                &tool_call.tool_use_id,
                &result_text,
            );
        }
    }

    warn!("Web search loop hit max iterations ({})", max_iterations);
    Ok(WebSearchLoopResult {
        content_blocks: all_content_blocks,
        stop_reason: "end_turn".to_string(),
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
    })
}

/// Build a follow-up Kiro payload with the assistant's response and tool result in history.
fn build_followup_payload(
    original_payload: &Value,
    assistant_text: &str,
    tool_use_id: &str,
    tool_result_text: &str,
) -> Value {
    let mut payload = original_payload.clone();

    // Get existing history or create empty array
    let history = payload["conversationState"]["history"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut new_history = history;

    // Add the assistant response + user tool result as a turn pair
    new_history.push(json!({
        "userInputMessage": {
            "content": payload["conversationState"]["currentMessage"]["userInputMessage"]["content"].clone()
        },
        "assistantResponseMessage": {
            "content": assistant_text,
            "toolUses": [{
                "toolUseId": tool_use_id,
                "name": "web_search",
                "input": {}
            }]
        }
    }));

    // New current message is the tool result
    payload["conversationState"]["history"] = json!(new_history);
    payload["conversationState"]["currentMessage"]["userInputMessage"]["content"] = json!(tool_result_text);

    // Add tool result to userInputMessageContext
    payload["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"] = json!([{
        "content": [{"text": tool_result_text}],
        "status": "success",
        "toolUseId": tool_use_id
    }]);

    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::anthropic::AnthropicTool;

    #[test]
    fn test_has_web_search_tool_true() {
        let tools = vec![
            AnthropicTool {
                name: "web_search".to_string(),
                description: None,
                input_schema: None,
                tool_type: Some("web_search_20250305".to_string()),
            },
        ];
        assert!(has_web_search_tool(&Some(tools)));
    }

    #[test]
    fn test_has_web_search_tool_false_regular_tool() {
        let tools = vec![
            AnthropicTool {
                name: "web_search".to_string(),
                description: Some("desc".to_string()),
                input_schema: Some(json!({"type": "object"})),
                tool_type: None,
            },
        ];
        // Has input_schema, so it's a regular tool, not server-side
        assert!(!has_web_search_tool(&Some(tools)));
    }

    #[test]
    fn test_has_web_search_tool_false_none() {
        assert!(!has_web_search_tool(&None));
    }

    #[test]
    fn test_build_followup_payload() {
        let original = json!({
            "conversationState": {
                "chatTriggerType": "MANUAL",
                "conversationId": "test-123",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "What is Rust?",
                        "modelId": "claude-sonnet-4.6",
                        "origin": "AI_EDITOR",
                        "userInputMessageContext": {
                            "tools": []
                        }
                    }
                }
            }
        });

        let followup = build_followup_payload(
            &original,
            "Let me search for that.",
            "toolu_123",
            "Search results for \"Rust\":\n\n1. Rust Lang (https://rust-lang.org)\n",
        );

        // Should have history with one turn
        let history = followup["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);

        // Current message should be the tool result
        let current_content = followup["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
        assert!(current_content.contains("Search results"));
    }
}
```

**Step 2: Register the module**

In `src/lib.rs`, add:
```rust
pub mod web_search_loop;
```

In `src/main.rs`, add:
```rust
mod web_search_loop;
```

**Step 3: Run tests**

Run: `cargo test --lib web_search_loop::`
Expected: PASS

**Step 4: Commit**

```bash
git add src/web_search_loop.rs src/lib.rs src/main.rs
git commit -m "feat: implement agentic web search loop"
```

---

### Task 8: Add `collect_stream_result` to streaming module

The agentic loop needs to collect a Kiro stream into a `StreamResult` (content + tool_calls + usage). This utility may need to be exposed publicly.

**Files:**
- Modify: `src/streaming/mod.rs`

**Step 1: Check if `collect_stream_result` exists**

Search for `collect_stream_result` or equivalent in `src/streaming/mod.rs`. The existing functions `collect_openai_response` and `collect_anthropic_response` return formatted JSON — we need a raw `StreamResult` collector.

If it doesn't exist, add:

```rust
/// Collect all events from a Kiro stream into a StreamResult.
///
/// This is used by the web search agentic loop to inspect tool calls
/// before deciding how to format the final response.
pub async fn collect_stream_result(
    stream: impl Stream<Item = Result<KiroEvent, ApiError>>,
) -> Result<StreamResult, ApiError> {
    use futures::pin_mut;
    pin_mut!(stream);

    let mut result = StreamResult::default();

    while let Some(event) = stream.next().await {
        let event = event?;
        match event.event_type.as_str() {
            "content" => {
                if let Some(content) = event.content {
                    result.content.push_str(&content);
                }
            }
            "thinking" => {
                if let Some(thinking) = event.thinking_content {
                    result.thinking_content.push_str(&thinking);
                }
            }
            "tool_use" => {
                if let Some(tool_use) = event.tool_use {
                    result.tool_calls.push(tool_use);
                }
            }
            "usage" => {
                result.usage = event.usage;
            }
            "context_usage" => {
                result.context_usage_percentage = event.context_usage_percentage;
            }
            _ => {}
        }
    }

    // Deduplicate tool calls
    result.tool_calls = deduplicate_tool_calls(result.tool_calls);

    Ok(result)
}
```

**Step 2: Run compilation**

Run: `cargo build`
Expected: Success

**Step 3: Commit**

```bash
git add src/streaming/mod.rs
git commit -m "feat: add collect_stream_result for agentic loop"
```

---

### Task 9: Integrate web search loop into Anthropic handler

**Files:**
- Modify: `src/routes/mod.rs:395-605` (anthropic_messages_handler)

**Step 1: Modify the handler**

In `anthropic_messages_handler`, after the Kiro payload is built (around line 462) and before sending the request, add the web search detection and loop.

The logic:
1. Check `has_web_search_tool(&request.tools)`
2. If true AND `state.exa_client.is_some()`:
   - Run `web_search_loop::run_web_search_loop(...)` which handles everything
   - Assemble the final Anthropic response from the loop result
   - Return it (streaming or non-streaming)
3. If false: continue with existing flow unchanged

Add the import at the top of `src/routes/mod.rs`:
```rust
use crate::web_search_loop;
```

In the handler, after `let kiro_payload = kiro_payload_result.payload;` (line 462), before the debug logging, add:

```rust
    // Check if this request needs the web search agentic loop
    let needs_web_search = web_search_loop::has_web_search_tool(&request.tools)
        && state.exa_client.is_some();

    if needs_web_search {
        let exa_client = state.exa_client.as_ref().unwrap();
        let region = state.auth_manager.get_region().await;

        let loop_result = web_search_loop::run_web_search_loop(
            &state.http_client,
            &state.auth_manager,
            exa_client,
            &region,
            &kiro_payload,
            state.config.first_token_timeout,
            &request.model,
            state.config.web_search_max_iterations,
        )
        .await
        .inspect_err(|e| {
            state.metrics.record_error(error_type_from_api_error(e));
        })?;

        // Build Anthropic response from loop result
        let response_id = format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")[..24].to_string());
        let anthropic_response = json!({
            "id": response_id,
            "type": "message",
            "role": "assistant",
            "content": loop_result.content_blocks,
            "model": request.model,
            "stop_reason": loop_result.stop_reason,
            "stop_sequence": null,
            "usage": {
                "input_tokens": loop_result.input_tokens,
                "output_tokens": loop_result.output_tokens
            }
        });

        guard.complete(loop_result.input_tokens as u64, loop_result.output_tokens as u64);

        if request.stream {
            // Convert to SSE stream
            let sse_events = format_web_search_response_as_sse(
                &anthropic_response,
                &request.model,
            );
            let byte_stream = futures::stream::iter(sse_events.into_iter().map(Ok::<_, std::io::Error>).map(|r| r.map(Bytes::from)));
            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .body(Body::from_stream(byte_stream))
                .map_err(|e| ApiError::Internal(anyhow::anyhow!("Failed to build response: {}", e)))?;
            return Ok(response);
        } else {
            return Ok(Json(anthropic_response).into_response());
        }
    }
```

Also add a helper function `format_web_search_response_as_sse` in `routes/mod.rs`:

```rust
/// Convert a complete Anthropic response to a series of SSE event strings.
fn format_web_search_response_as_sse(response: &Value, model: &str) -> Vec<String> {
    let mut events = Vec::new();

    // message_start
    events.push(format!("event: message_start\ndata: {}\n\n", json!({
        "type": "message_start",
        "message": {
            "id": response["id"],
            "type": "message",
            "role": "assistant",
            "content": [],
            "model": model,
            "usage": response["usage"]
        }
    })));

    // content blocks
    if let Some(blocks) = response["content"].as_array() {
        for (i, block) in blocks.iter().enumerate() {
            // content_block_start
            events.push(format!("event: content_block_start\ndata: {}\n\n", json!({
                "type": "content_block_start",
                "index": i,
                "content_block": block
            })));

            // content_block_stop
            events.push(format!("event: content_block_stop\ndata: {}\n\n", json!({
                "type": "content_block_stop",
                "index": i
            })));
        }
    }

    // message_delta
    events.push(format!("event: message_delta\ndata: {}\n\n", json!({
        "type": "message_delta",
        "delta": {"stop_reason": response["stop_reason"]},
        "usage": {"output_tokens": response["usage"]["output_tokens"]}
    })));

    // message_stop
    events.push("event: message_stop\ndata: {\"type\": \"message_stop\"}\n\n".to_string());

    events
}
```

**Step 2: Verify compilation**

Run: `cargo build`
Expected: Success

**Step 3: Run existing tests**

Run: `cargo test --lib`
Expected: PASS — no regressions

**Step 4: Commit**

```bash
git add src/routes/mod.rs
git commit -m "feat: integrate web search agentic loop into Anthropic handler"
```

---

### Task 10: End-to-end manual testing

**Step 1: Set up environment**

Add to `.env`:
```
EXA_API_KEY=your-exa-api-key-here
```

**Step 2: Start the gateway**

Run: `cargo run --bin kiro-gateway --release`

**Step 3: Test with curl (non-streaming)**

```bash
curl -X POST http://localhost:8000/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: YOUR_PROXY_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-4.6",
    "max_tokens": 1024,
    "tools": [{"type": "web_search_20250305", "name": "web_search"}],
    "messages": [{"role": "user", "content": "What happened in tech news today?"}]
  }'
```

Expected: Response with `server_tool_use` and `web_search_tool_result` blocks, followed by text.

**Step 4: Test with curl (streaming)**

Same request with `"stream": true`. Verify SSE events are properly formatted.

**Step 5: Test with Claude Code**

Point Claude Code at the gateway and trigger a web search.

**Step 6: Commit final state**

Run `cargo fmt && cargo clippy` and fix any warnings, then:

```bash
git add -A
git commit -m "feat: complete web search via gateway agentic loop with Exa"
```
