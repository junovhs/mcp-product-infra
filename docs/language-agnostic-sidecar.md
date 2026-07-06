# Language-agnostic sidecar mode

`turnkey-mcp` is implemented in Rust, but the useful product boundary is not "Rust apps only." The intended app model is:

```text
host MCP client
  -> turnkey-mcp stdio server
  -> app-owned handler process / local service
  -> app behavior
```

The Rust sidecar should own:

- MCP JSON-RPC framing
- `initialize`
- `tools/list`
- `tools/call`
- structured response wrapping
- read/write dispatch
- host config installation
- readiness checks
- optional resident owner lifecycle

The app process should own:

- business logic
- auth
- persistence
- domain-specific validation
- app-specific recovery guidance

## Handler protocol sketch

For manifest mode, `turnkey-mcp` can call a handler process with newline-delimited JSON:

Request:

```json
{"tool":"todo_create","arguments":{"title":"Ship it"},"workspace":"/repo"}
```

Response:

```json
{"ok":true,"result":{"id":"TODO-1","title":"Ship it"}}
```

Error response:

```json
{"ok":false,"code":-32602,"message":"todo_create requires title"}
```

This keeps non-Rust adapters trivial. Python, Node, Go, Swift, Ruby, or anything else can implement the handler side.

## Manifest sketch

```json
{
  "name": "todo",
  "version": "0.1.0",
  "instructions": "Use todo_* tools to operate Todo.",
  "handler": {
    "command": "python",
    "args": ["./todo_handlers.py"]
  },
  "tools": [
    {
      "name": "todo_status",
      "description": "Return app status.",
      "mutation": "never",
      "inputSchema": {
        "type": "object",
        "properties": {},
        "additionalProperties": false
      }
    }
  ]
}
```

## Why not start with JS/Python packages?

The smallest good first version is:

```text
Rust crate + Rust binary + manifest bridge
```

Language-specific SDKs can come later as thin wrappers around the manifest/handler protocol.
