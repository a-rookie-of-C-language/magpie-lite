//! Errors produced while decoding or encoding wire requests and responses.

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum CoreError {
    #[error("missing field `{0}`")]
    MissingField(&'static str),

    #[error("invalid `{field}`: {detail}")]
    Invalid { field: &'static str, detail: String },
}

/// Convenience alias for core results.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
