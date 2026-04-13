//! Phase 4: In-process MCP server.
//!
//! An embedded `rmcp` HTTP MCP server bound to loopback. Claude Code
//! child sessions connect to it via an auto-generated `.mcp.json`
//! (Phase 4 Task 6) and call the read-only tools in [`handlers`] to
//! inspect ccom's state.
//!
//! Architecture: a dedicated OS thread named `ccom-mcp` runs a
//! current-thread tokio runtime. All async work lives inside that
//! thread; the main TUI thread only ever talks to the server via
//! `std::sync::mpsc` and `Arc<Mutex<…>>` (no tokio primitives
//! escape the mcp thread). See `docs/plans/phase-4-mcp-readonly.md`
//! for the full design and `docs/plans/notes/rmcp-spike.md` for the
//! empirical rmcp 1.4.0 findings this module was built against.

mod handlers;
mod server;
mod state;

pub use server::McpServer;
// `ReadOnlyCtx` is constructed in `src/app/mod.rs` via
// `crate::mcp::ReadOnlyCtx { .. }`. Rustc's unused-imports lint
// flags the `pub(crate) use` below even though removing it
// breaks the build — this is a known false positive for
// re-exports of items in private child modules. The `#[allow]`
// is targeted and small.
#[allow(unused_imports)]
pub(crate) use state::ReadOnlyCtx;
