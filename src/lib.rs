//! mcp-product-infra is a small toolkit for apps that want to expose themselves to
//! agents through MCP.
//!
//! It is intentionally not an agent framework and not a generic MCP manager. It
//! gives app authors the boring pieces that are easy to get wrong: a stdio MCP
//! loop, typed tool registry, structured responses, read/write dispatch,
//! optional sidecar ownership, and no-clobber host config installers.

pub mod activity;
pub mod adapters;
pub mod agent_guard;
pub mod capture;
pub mod http;
pub mod manifest;
pub mod process;
pub mod registry;
pub mod resources;
pub mod response;
pub mod server;
pub mod service;
pub mod service_host;
pub mod shell_guard;
pub mod sidecar;
pub mod types;

pub use activity::{ActivityLease, ActivityView};
pub use adapters::{
    AdapterAction, ClaudeHook, HostConfigFact, HostInstall, HostReadinessReport, HostServer,
    HostTransport, InstallReport,
};
pub use http::{Concurrency, Hub};
pub use manifest::{
    HandlerCommand, HandlerRequest, HandlerResponse, Manifest, ManifestDefect, ManifestMutation,
    ManifestTool,
};
pub use process::{run_with_timeout, ProcessOutcome, ProcessOutput};
pub use registry::{RegistryDefect, ToolRegistry};
pub use resources::{ResourceContent, ResourceEntry, ResourceProvider};
pub use response::{error_frame, error_frame_for, error_frame_kinded, result_frame, tool_ok};
pub use server::{BeforeToolHook, McpServer, MutationHook, OwnerProse, ServerConfig};
pub use service::{Service, ServiceOutcome};
pub use service_host::WindowsServiceHostConfig;
pub use sidecar::{
    OwnerEndpoint, OwnerHealth, OwnerHealthAction, OwnerHealthReport, OwnerHealthState,
    OwnerRecovery, OwnerTransportError, RetiredOwner, SidecarConfig,
};
pub use types::kinds;
pub use types::{ExecutionPolicy, MutationKind, ToolContext, ToolError, ToolResult, ToolSpec};
