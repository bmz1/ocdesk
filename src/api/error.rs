//! HTTP failures retain their status through anyhow context. Recovery policy
//! belongs to the caller; text from the server is only a bounded diagnostic.

use std::fmt;

use reqwest::StatusCode;

#[derive(Debug)]
pub(crate) struct HttpError {
    status: StatusCode,
    detail: String,
}

impl HttpError {
    pub(crate) fn new(status: StatusCode, body: &str) -> Self {
        Self {
            status,
            detail: body.chars().take(300).collect(),
        }
    }

    pub(crate) fn is_not_found(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<Self>()
            .is_some_and(|error| error.status == StatusCode::NOT_FOUND)
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {}: {}", self.status, self.detail)
    }
}

impl std::error::Error for HttpError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_uses_status_through_context_not_server_or_context_text() {
        let missing = anyhow::Error::new(HttpError::new(StatusCode::NOT_FOUND, "not JSON"))
            .context("fetch approvals");
        assert!(HttpError::is_not_found(&missing));
        for status in [StatusCode::UNAUTHORIZED, StatusCode::INTERNAL_SERVER_ERROR] {
            let error = anyhow::Error::new(HttpError::new(status, "LocationNotFoundError 404"))
                .context("HTTP 404 appeared in an unrelated message");
            assert!(!HttpError::is_not_found(&error));
        }
        assert!(!HttpError::is_not_found(&anyhow::anyhow!("404")));
    }

    #[test]
    fn diagnostic_is_bounded_at_unicode_character_boundaries() {
        let error = HttpError::new(StatusCode::BAD_REQUEST, &"é".repeat(1000));
        assert_eq!(error.detail.chars().count(), 300);
    }
}
