# Spec Delta

## ADDED Requirements

### Requirement: Configured stdio and HTTP JSON-RPC MCP client gateway
The system SHALL connect enabled `[mcp_servers.<name>]` entries from trusted `tekvra.toml` configuration over child-process `stdio` JSON-RPC 2.0 (`initialize`, `notifications/initialized`, `tools/list`, `tools/call`) and remote HTTP JSON-RPC 2.0 `POST` endpoints, exposing discovered tools through `McpGateway` to `code` (`mcp_list` / `mcp_call`) and `app-server` (`mcpServerStatus/list`) without mutating top-level LLM tool schemas.

#### Scenario: Stdio MCP server discovered and invoked through `code`
- **WHEN** trusted `tekvra.toml` enables a stdio MCP server with `command` and `args` and a session runs Python `code` calling `mcp_list()` and `await mcp_call(server, tool, params)`
- **THEN** the gateway completes the MCP `initialize` handshake, caches `tools/list` descriptors, authorizes and dispatches `tools/call` over stdio JSON-RPC, and leaves top-level LLM `ToolDefinition`s unchanged

#### Scenario: HTTP JSON-RPC MCP server invoked and reported in `app-server`
- **WHEN** trusted `tekvra.toml` enables an HTTP MCP server with `url` and a client calls `mcpServerStatus/list` or invokes `mcp_call`
- **THEN** the gateway discovers tools via JSON-RPC `POST` to `url`, executes `tools/call` over HTTP, and reports the server's live tool map in `mcpServerStatus/list`
