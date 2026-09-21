//! Agent-agnostic core for limitbar.
//!
//! Deliberately free of any Tauri dependency: the domain model, the per-agent
//! providers and the polling policy are all testable with a plain `cargo test`,
//! and the desktop shell in `src-tauri` is a thin layer on top.

pub mod model;
pub mod providers;
pub mod settings;

pub use model::{AgentSnapshot, AgentStatus, Gauge, Severity, Snapshot};
pub use providers::{default_providers, LimitProvider, Monitor, ProviderError};
pub use settings::Settings;
