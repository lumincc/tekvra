# Spec Delta

## ADDED Requirements

### Requirement: Streamable HTTP `text/event-stream` MCP framing and catalog refresh
The system SHALL accept both `application/json` and `text/event-stream` responses from HTTP MCP endpoints in `ConfiguredMcpGateway` and refresh cached `McpToolDescriptor` entries when a server notifies `notifications/tools/list_changed`, without mutating top-level LLM tool schemas.

#### Scenario: HTTP MCP server responds with `text/event-stream`
- **WHEN** an enabled HTTP MCP server replies to `tools/list` or `tools/call` with `Content-Type: text/event-stream` framing JSON-RPC `data:` events
- **THEN** `ConfiguredMcpGateway` decodes the SSE stream, matches the JSON-RPC response `id`, and returns the result to Monty `code` or `mcpServerStatus/list`
