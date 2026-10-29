//! The NOSH terminal UI: a port of the TypeScript `apps/tui/src` (OpenTUI on Bun) to ratatui and
//! crossterm, run in-process by `nosh open` / `nosh tui`.
//!
//! Specified by INDEX.md (§2 cross-spec decisions, §3 crate layout, §4 facts), DECISIONS.md (TUI items
//! 1-9) and the five TUI specs: tui-transport.md, tui-controller.md, tui-viewmodel.md,
//! tui-workspace.md and tui-dialogs-visual.md. The module contract for implementers is API.md next to
//! those specs.
//!
//! Layers, bottom up: [`js`] (JavaScript value semantics) → [`view_model`], [`map_graph`] (pure
//! derivations) → [`transport`] (daemon HTTP + WebSocket) → [`controller`] (state and commands) →
//! [`theme`], [`palette`], [`editor`], [`layout`], [`dialog`], [`markdown`], [`visual`] (presentation) →
//! [`workspace`], [`render`] and the private `app` event loop behind [`run`].

pub mod config;
pub mod controller;
pub mod dialog;
pub mod editor;
pub mod js;
pub mod layout;
pub mod map_graph;
pub mod markdown;
pub mod palette;
pub mod render;
pub mod theme;
pub mod transport;
pub mod view_model;
pub mod visual;
pub mod workspace;

mod app;
mod mermaid_ascii;

pub use config::TuiConfig;

/// `launchTui` / `runTui` (`apps/tui/src/index.ts`, `ui.ts`): runs the terminal UI until the user
/// quits (INDEX.md §2 item 1, tui-workspace.md §1.5 and CA-4).
///
/// Synchronous: it builds its own `current_thread` runtime and `LocalSet`, so it must be called
/// outside any tokio runtime. It restores the terminal before returning or unwinding. `Ok(())` is a
/// normal quit (exit 0); `Err(message)` is a startup failure the CLI prints to stderr with no
/// `nosh: ` prefix (exit 1). It installs the rustls ring provider first
/// ([`transport::install_crypto_provider`]) and delegates to `app::run`.
pub fn run(config: TuiConfig) -> Result<(), String> {
    transport::install_crypto_provider();
    app::run(config)
}
