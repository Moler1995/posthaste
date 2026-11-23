//! Error types shared by the domain layer.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, PosthasteError>;

#[derive(Debug, Error)]
pub enum PosthasteError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("request timed out")]
    Timeout,
    #[error("request cancelled")]
    Cancelled,
    #[error("too many redirects")]
    TooManyRedirects,
    #[error("invalid header: {0}")]
    InvalidHeader(String),
    #[error("invalid body: {0}")]
    InvalidBody(String),
    #[error("WebSocket error: {message}")]
    WebSocket { message: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_context_for_each_variant() {
        assert_eq!(
            PosthasteError::InvalidUrl("not a url".into()).to_string(),
            "invalid URL: not a url"
        );
        assert_eq!(
            PosthasteError::Storage("disk full".into()).to_string(),
            "storage error: disk full"
        );
        assert_eq!(
            PosthasteError::Serialization("bad json".into()).to_string(),
            "serialization error: bad json"
        );
        assert_eq!(
            PosthasteError::Http("connection reset".into()).to_string(),
            "HTTP error: connection reset"
        );
        assert_eq!(PosthasteError::Timeout.to_string(), "request timed out");
        assert_eq!(PosthasteError::Cancelled.to_string(), "request cancelled");
        assert_eq!(
            PosthasteError::TooManyRedirects.to_string(),
            "too many redirects"
        );
        assert_eq!(
            PosthasteError::InvalidHeader("bad name".into()).to_string(),
            "invalid header: bad name"
        );
        assert_eq!(
            PosthasteError::InvalidBody("expected value".into()).to_string(),
            "invalid body: expected value"
        );
        assert_eq!(
            PosthasteError::WebSocket {
                message: "handshake rejected".into()
            }
            .to_string(),
            "WebSocket error: handshake rejected"
        );
    }

    #[test]
    fn converts_io_errors() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let error: PosthasteError = io.into();
        assert!(matches!(error, PosthasteError::Io(_)));
        assert_eq!(error.to_string(), "missing");
    }
}
