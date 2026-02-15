#[derive(Debug, thiserror::Error)]
pub enum OrchestratorRuntimeError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("runtime unavailable: {0}")]
    Unavailable(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("execution timed out after {0}")]
    Timeout(String),

    #[error("resource not found: {0}")]
    ResourceNotFound(String),

    #[error("path policy violation: {0}")]
    PathViolation(String),

    #[error("execution guard blocked action: {0}")]
    GuardBlocked(String),

    #[error("internal runtime error: {0}")]
    Internal(String),
}

impl OrchestratorRuntimeError {
    pub fn is_invalid_request(&self) -> bool {
        matches!(self, Self::InvalidRequest(_))
    }

    pub fn is_resource_not_found(&self) -> bool {
        matches!(self, Self::ResourceNotFound(_))
    }
}
