//! System0 orchestrator: admin turns, sub-agent delegation, self-improvement, event journaling.

pub mod actions;
pub mod adaptation;
pub mod bootstrap;
pub mod error;
pub mod llm_clients;
mod mock_llm;
pub mod prompt;
pub mod proposals;
pub mod runtime;
pub mod security;
pub mod skills;
pub mod state;
pub(crate) mod tool_id;
pub mod tools;
pub mod tracing;

pub use error::System0Error;
pub use runtime::System0Runtime;
