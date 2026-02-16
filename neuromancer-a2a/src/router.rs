use axum::Router;
use axum::routing::{get, post};

use crate::handlers;
use crate::state::A2aState;

/// Build the A2A axum Router with all endpoints.
pub fn a2a_router(state: A2aState) -> Router {
    Router::new()
        .route(
            "/.well-known/agent-card.json",
            get(handlers::agent_card_handler),
        )
        .route("/message:send", post(handlers::message_send_handler))
        .route("/message:stream", post(handlers::message_stream_handler))
        .route("/tasks", get(handlers::list_tasks_handler))
        .route("/tasks/{id}", get(handlers::get_task_handler))
        .route("/tasks/{id}:cancel", post(handlers::cancel_task_handler))
        .route(
            "/tasks/{id}:subscribe",
            post(handlers::subscribe_task_handler),
        )
        .with_state(state)
}
