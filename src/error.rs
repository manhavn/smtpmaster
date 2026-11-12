use thiserror::Error;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    Conflict(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("no smtp account has remaining capacity")]
    NoCapacity,
    #[error("message rejected: {0}")]
    TooLarge(String),
    #[error("database error: {0}")]
    Mongo(#[from] mongodb::error::Error),
    #[error("smtp error: {0}")]
    Smtp(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl AppError {
    /// SMTP server reply for a dispatch failure.
    pub fn smtp_code(&self) -> (u16, &'static str) {
        match self {
            AppError::NoCapacity => (451, "4.3.0 no smtp account has remaining capacity"),
            AppError::TooLarge(_) => (552, "5.3.4 message exceeds the size limit"),
            AppError::Invalid(_) => (550, "5.5.1 invalid routing request"),
            _ => (451, "4.3.0 upstream delivery failed, try again later"),
        }
    }
}
