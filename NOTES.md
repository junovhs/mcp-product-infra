# Project notes

## Name

Working name: `turnkey-mcp`.

Taglines:

- Turnkey MCP for apps.
- The boring parts of app-owned MCP, packaged.
- A tiny runtime for adding a reliable MCP server to your app.

## Audience

Developers building apps that agents should operate through MCP.

Not users managing a pile of random MCP servers.  
Not agent framework authors.  
Not workflow automation users.

## Critical positioning

This is Rust-native, not Rust-only.

- Rust apps import the crate.
- Non-Rust apps use a sidecar/manifest/process bridge.

## Extraction discipline

This package now includes the actual copied Ishoo source files under `origin/ishoo/`. Continue by extracting from those files, not by redesigning from memory.

The right workflow is:

```text
copy Ishoo file
make generic version next to it
remove product-specific nouns
preserve behavior and failure-mode comments
add tests
```

## Non-goals

- no cloud service
- no marketplace
- no desktop manager
- no generic multi-MCP gateway
- no agent orchestration
- no LLM calls
- no repo writes except explicit install commands

## First public promise

Install this when your app wants to expose a clean MCP surface with sane defaults for host setup, lifecycle, structured output, and read/write dispatch.
