use serde_json::{json, Value};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::error::ApiError;
use crate::streaming;
use crate::web_search::ExaClient;

/// Check if the request tools include a web_search server-side tool.
pub fn has_web_search_tool(tools: &Option<Vec<crate::models::anthropic::AnthropicTool>>) -> bool {
    tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.name == "web_search" && t.input_schema.is_none())
    })
}

/// Result of the web search agentic loop.
pub struct WebSearchLoopResult {
    pub content_blocks: Vec<Value>,
    pub stop_reason: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
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
    _model: &str,
    max_iterations: usize,
) -> Result<WebSearchLoopResult, ApiError> {
    let mut all_content_blocks: Vec<Value> = Vec::new();
    let mut total_input_tokens = 0i32;
    let mut total_output_tokens = 0i32;
    let mut current_payload = initial_payload.clone();

    for iteration in 0..max_iterations {
        info!(iteration = iteration, "Web search loop iteration");

        // Send request to Kiro and collect full response
        let result = collect_kiro_response(
            http_client,
            auth_manager,
            region,
            &current_payload,
            first_token_timeout,
        )
        .await?;

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
        let web_search_calls: Vec<_> = result
            .tool_calls
            .iter()
            .filter(|tc| tc.name == "web_search")
            .collect();

        if web_search_calls.is_empty() {
            debug!(
                "No web_search calls in iteration {}, loop complete",
                iteration
            );
            return Ok(WebSearchLoopResult {
                content_blocks: all_content_blocks,
                stop_reason: "end_turn".to_string(),
                input_tokens: total_input_tokens,
                output_tokens: total_output_tokens,
            });
        }

        // Process each web_search call
        for tool_call in &web_search_calls {
            let query = tool_call
                .input
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("");

            let server_tool_id = format!(
                "srvtoolu_{}",
                &Uuid::new_v4().to_string().replace('-', "")[..24]
            );

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
            let result_content: Vec<Value> = search_results
                .iter()
                .map(|r| {
                    json!({
                        "type": "web_search_result",
                        "url": r.url,
                        "title": r.title,
                        "page_age": r.published_date
                    })
                })
                .collect();

            all_content_blocks.push(json!({
                "type": "web_search_tool_result",
                "tool_use_id": server_tool_id,
                "content": result_content
            }));

            // Build tool result text for Kiro follow-up
            let result_text = ExaClient::format_results_as_text(query, &search_results);

            // Build follow-up payload
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

/// Execute a Kiro request and collect the full response.
async fn collect_kiro_response(
    http_client: &crate::http_client::KiroHttpClient,
    auth_manager: &crate::auth::AuthManager,
    region: &str,
    payload: &Value,
    first_token_timeout: u64,
) -> Result<streaming::StreamResult, ApiError> {
    let access_token = auth_manager
        .get_access_token()
        .await
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

    let stream = streaming::parse_kiro_stream(response, first_token_timeout).await?;
    streaming::collect_stream_result(stream).await
}

/// Build a follow-up Kiro payload with the assistant's response and tool result in history.
fn build_followup_payload(
    original_payload: &Value,
    assistant_text: &str,
    tool_use_id: &str,
    tool_result_text: &str,
) -> Value {
    let mut payload = original_payload.clone();

    let history = payload["conversationState"]["history"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let mut new_history = history;

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

    payload["conversationState"]["history"] = json!(new_history);
    payload["conversationState"]["currentMessage"]["userInputMessage"]["content"] =
        json!(tool_result_text);
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
        let tools = vec![AnthropicTool {
            name: "web_search".to_string(),
            description: None,
            input_schema: None,
            tool_type: Some("web_search_20250305".to_string()),
        }];
        assert!(has_web_search_tool(&Some(tools)));
    }

    #[test]
    fn test_has_web_search_tool_false_regular_tool() {
        let tools = vec![AnthropicTool {
            name: "web_search".to_string(),
            description: Some("desc".to_string()),
            input_schema: Some(json!({"type": "object"})),
            tool_type: None,
        }];
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

        let history = followup["conversationState"]["history"]
            .as_array()
            .unwrap();
        assert_eq!(history.len(), 1);

        let current_content = followup["conversationState"]["currentMessage"]["userInputMessage"]
            ["content"]
            .as_str()
            .unwrap();
        assert!(current_content.contains("Search results"));
    }
}
