# Web Search via Gateway Agentic Loop

## Problem

Claude Code sends `web_search` as an Anthropic server-side tool (`{"type": "web_search_20250305", "name": "web_search"}`). This tool has no `input_schema` because Anthropic's server executes the search. Kiro has no equivalent server-side tool support. The current fix filters out `web_search` to prevent a 422 deserialization error, but web search silently does nothing.

## Solution

The gateway implements an agentic loop: it converts `web_search` into a regular Kiro tool, intercepts when the model calls it, executes the search via Exa API, feeds results back to Kiro, and returns the final response to the client in Anthropic's `server_tool_use` / `web_search_tool_result` format.

## Architecture

```
Claude Code ──► Gateway ──► Kiro (web_search as regular tool)
                  │                    │
                  │              tool_use: web_search
                  │                    │
                  │◄───────────────────┘
                  │
                  ├──► Exa API (execute search)
                  │◄── search results
                  │
                  ├──► Kiro (2nd request: history + tool_result)
                  │◄── final response text
                  │
                  ▼
Claude Code ◄── server_tool_use + web_search_tool_result + text
```

## Request Flow

### 1. Inbound (Anthropic → Gateway)

Claude Code sends:
```json
{
  "tools": [
    {"type": "web_search_20250305", "name": "web_search"},
    {"type": "function", "name": "Bash", "input_schema": {...}}
  ]
}
```

Gateway separates server-side tools (no `input_schema`) from regular tools.

### 2. Convert web_search to regular Kiro tool

```json
{
  "toolSpecification": {
    "name": "web_search",
    "description": "Search the web for current information. Use this when you need up-to-date data, news, or facts beyond your knowledge cutoff.",
    "inputSchema": {
      "json": {
        "type": "object",
        "properties": {
          "query": {
            "type": "string",
            "description": "The search query"
          }
        },
        "required": ["query"]
      }
    }
  }
}
```

### 3. First Kiro request — collect response

Send to Kiro with all tools (regular + converted web_search). **Collect the full response** (do not stream) to determine if the model wants to search.

- If no `web_search` tool call: return/stream the response normally.
- If `web_search` tool call detected: proceed to step 4.

### 4. Execute search via Exa

```
POST https://api.exa.ai/search
x-api-key: $EXA_API_KEY

{
  "query": "<model's search query>",
  "type": "auto",
  "numResults": 5,
  "contents": {
    "text": {"maxCharacters": 3000},
    "highlights": {"numSentences": 3}
  }
}
```

### 5. Second Kiro request — with search results

Build a new request with:
- Original conversation history
- Model's first response (text + tool_use) as an assistant turn
- Tool result containing formatted search results as a user turn

The tool result content:
```
Search results for "<query>":

1. <title> (<url>)
   <highlight or text excerpt>

2. <title> (<url>)
   <highlight or text excerpt>
...
```

Send to Kiro and collect or stream the response.

### 6. Loop if needed

The model may call `web_search` again in the second response. Repeat steps 4-5 up to `WEB_SEARCH_MAX_ITERATIONS` (default 3).

### 7. Outbound response to Claude Code

Assemble the full response with all iterations' content blocks:

```json
{
  "content": [
    {"type": "text", "text": "Let me search for that."},
    {
      "type": "server_tool_use",
      "id": "srvtoolu_<generated>",
      "name": "web_search",
      "input": {"query": "..."}
    },
    {
      "type": "web_search_tool_result",
      "tool_use_id": "srvtoolu_<generated>",
      "content": [
        {
          "type": "web_search_result",
          "url": "https://...",
          "title": "...",
          "page_age": "..."
        }
      ]
    },
    {"type": "text", "text": "Based on the search results, ..."}
  ],
  "stop_reason": "end_turn"
}
```

For streaming: emit all blocks as Anthropic SSE events (`content_block_start`, `content_block_delta`, `content_block_stop`).

## New Components

### `src/web_search.rs` — Exa API client

- `ExaClient` struct with `reqwest::Client` and API key
- `search(query, num_results)` method
- Response parsing into `WebSearchResult { url, title, text, highlights, published_date }`

### `src/routes/mod.rs` — Agentic loop in handler

- Detect server-side tools in the incoming request
- After first Kiro response, check for web_search tool calls
- Execute search + build follow-up request loop
- Assemble final response with server_tool_use blocks

### Changes to existing code

- `models/anthropic.rs`: Add `ServerToolUse` and `WebSearchToolResult` content block types
- `converters/anthropic_to_kiro.rs`: Convert web_search to regular Kiro tool instead of filtering
- `streaming/mod.rs`: Support emitting `server_tool_use` and `web_search_tool_result` in Anthropic stream
- `config.rs`: Add `exa_api_key`, `web_search_max_results`, `web_search_max_iterations`

## Configuration

| Env Var | Default | Description |
|---------|---------|-------------|
| `EXA_API_KEY` | (required if web search used) | Exa API key |
| `WEB_SEARCH_MAX_RESULTS` | `5` | Max search results per query |
| `WEB_SEARCH_MAX_ITERATIONS` | `3` | Max search round-trips per request |

## Edge Cases

- **No EXA_API_KEY set**: If client sends web_search but no API key configured, return an error in the `web_search_tool_result` block (matching Anthropic's error format: `{"type": "web_search_tool_result_error", "error_code": "unavailable"}`).
- **Exa API failure**: Return error result, let model respond without search data.
- **Model doesn't call web_search**: Normal flow, no extra latency.
- **Multiple searches in one turn**: Loop handles it, capped at max iterations.
- **OpenAI format clients**: Web search only applies to the Anthropic `/v1/messages` endpoint. OpenAI clients don't send server-side tools.
- **Multi-turn with previous search results**: Claude Code sends back `server_tool_use` and `web_search_tool_result` blocks in conversation history. These should be converted to regular tool_use/tool_result in Kiro history.
