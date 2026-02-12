use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use std::convert::Infallible;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use crate::state::{A2aState, A2aTaskRequest};
use crate::types::{
    A2aErrorResponse, A2aTaskStatus, MessageSendRequest, MessageSendResponse, TaskCancelRequest,
    TaskListResponse, TaskStatusResponse,
};

const A2A_CONTENT_TYPE: &str = "application/a2a+json";

/// Helper to build an A2A JSON response with the correct content type.
fn a2a_json<T: serde::Serialize>(status: StatusCode, body: &T) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        A2A_CONTENT_TYPE.parse().unwrap(),
    );
    (status, headers, Json(body).into_response()).into_response()
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    a2a_json(
        status,
        &A2aErrorResponse {
            code: code.to_string(),
            message: message.to_string(),
            details: None,
        },
    )
}

/// GET /.well-known/agent-card.json
#[instrument(skip(state))]
pub async fn agent_card_handler(State(state): State<A2aState>) -> Response {
    let card = state.agent_card().await;
    a2a_json(StatusCode::OK, &card)
}

/// POST /message:send — send a message and get a synchronous task response.
#[instrument(skip(state, body))]
pub async fn message_send_handler(
    State(state): State<A2aState>,
    Json(body): Json<MessageSendRequest>,
) -> Response {
    let a2a_task_id = if let Some(ref existing_id) = body.task_id {
        // Append to existing task.
        match state.get_task(existing_id).await {
            Some(_) => existing_id.clone(),
            None => {
                return error_response(StatusCode::NOT_FOUND, "task_not_found", "task not found");
            }
        }
    } else {
        state.create_task(body.message.clone()).await
    };

    let request = A2aTaskRequest {
        a2a_task_id: a2a_task_id.clone(),
        message: body.message,
        task_id: body.task_id,
    };

    if let Err(e) = state.submit_task(request).await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "submit_failed", &e);
    }

    // Return current task state.
    match state.get_task(&a2a_task_id).await {
        Some(record) => {
            let resp = MessageSendResponse {
                id: record.id,
                status: record.status,
                artifacts: record.artifacts,
                history: record.history,
            };
            a2a_json(StatusCode::OK, &resp)
        }
        None => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "task vanished",
        ),
    }
}

/// POST /message:stream — send a message and get an SSE streaming response.
#[instrument(skip(state, body))]
pub async fn message_stream_handler(
    State(state): State<A2aState>,
    Json(body): Json<MessageSendRequest>,
) -> Response {
    let a2a_task_id = if let Some(ref existing_id) = body.task_id {
        match state.get_task(existing_id).await {
            Some(_) => existing_id.clone(),
            None => {
                return error_response(StatusCode::NOT_FOUND, "task_not_found", "task not found");
            }
        }
    } else {
        state.create_task(body.message.clone()).await
    };

    let rx = state.subscribe(&a2a_task_id).await;

    let request = A2aTaskRequest {
        a2a_task_id: a2a_task_id.clone(),
        message: body.message,
        task_id: body.task_id,
    };

    if let Err(e) = state.submit_task(request).await {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "submit_failed", &e);
    }

    let stream = BroadcastStream::new(rx).map(|result| match result {
        Ok(event) => {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Ok::<_, Infallible>(Event::default().data(data))
        }
        Err(_) => Ok(Event::default().data("{\"type\":\"error\",\"message\":\"stream error\"}")),
    });

    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

/// GET /tasks/{id} — get task status.
#[instrument(skip(state))]
pub async fn get_task_handler(State(state): State<A2aState>, Path(id): Path<String>) -> Response {
    match state.get_task(&id).await {
        Some(record) => {
            let resp = TaskStatusResponse {
                id: record.id,
                status: record.status,
                artifacts: record.artifacts,
                history: record.history,
                metadata: None,
            };
            a2a_json(StatusCode::OK, &resp)
        }
        None => error_response(StatusCode::NOT_FOUND, "task_not_found", "task not found"),
    }
}

/// GET /tasks — list all tasks.
#[instrument(skip(state))]
pub async fn list_tasks_handler(State(state): State<A2aState>) -> Response {
    let records = state.list_tasks().await;
    let tasks: Vec<TaskStatusResponse> = records
        .into_iter()
        .map(|r| TaskStatusResponse {
            id: r.id,
            status: r.status,
            artifacts: r.artifacts,
            history: r.history,
            metadata: None,
        })
        .collect();
    a2a_json(StatusCode::OK, &TaskListResponse { tasks })
}

/// POST /tasks/{id}:cancel — cancel a task.
#[instrument(skip(state))]
pub async fn cancel_task_handler(
    State(state): State<A2aState>,
    Path(id): Path<String>,
    body: Option<Json<TaskCancelRequest>>,
) -> Response {
    match state.get_task(&id).await {
        Some(record) => {
            if record.status == A2aTaskStatus::Completed || record.status == A2aTaskStatus::Failed {
                return error_response(
                    StatusCode::CONFLICT,
                    "task_terminal",
                    "task is already in a terminal state",
                );
            }
            state.set_task_status(&id, A2aTaskStatus::Failed).await;
            state.notify_done(&id).await;

            let updated = state.get_task(&id).await.unwrap();
            let resp = TaskStatusResponse {
                id: updated.id,
                status: updated.status,
                artifacts: updated.artifacts,
                history: updated.history,
                metadata: None,
            };
            a2a_json(StatusCode::OK, &resp)
        }
        None => error_response(StatusCode::NOT_FOUND, "task_not_found", "task not found"),
    }
}

/// POST /tasks/{id}:subscribe — SSE subscription for an existing task.
#[instrument(skip(state))]
pub async fn subscribe_task_handler(
    State(state): State<A2aState>,
    Path(id): Path<String>,
) -> Response {
    match state.get_task(&id).await {
        Some(_) => {
            let rx = state.subscribe(&id).await;
            let stream = BroadcastStream::new(rx).map(|result| match result {
                Ok(event) => {
                    let data = serde_json::to_string(&event).unwrap_or_default();
                    Ok::<_, Infallible>(Event::default().data(data))
                }
                Err(_) => {
                    Ok(Event::default().data("{\"type\":\"error\",\"message\":\"stream error\"}"))
                }
            });
            Sse::new(stream)
                .keep_alive(axum::response::sse::KeepAlive::default())
                .into_response()
        }
        None => error_response(StatusCode::NOT_FOUND, "task_not_found", "task not found"),
    }
}
