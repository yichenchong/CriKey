//! Core error type.

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("result rejected: generation {got} is stale, current is {current}")]
    StaleGeneration { got: u64, current: u64 },
    #[error("operation was cancelled")]
    Cancelled,
    #[error("capacity exceeded: {0}")]
    CapacityExceeded(&'static str),
    #[error("{0}")]
    Invalid(String),
}
