//! Tool action handlers. Only `dispatch::dispatch_tool` is used externally.

pub(crate) mod dispatch;
mod adaptive_actions;
mod authenticated_adaptive_actions;
mod runtime_actions;
