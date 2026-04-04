//! Typed error enum for database operations.

use thiserror::Error;

/// Alias for `Result<T, DbError>`.
pub type DbResult<T> = Result<T, DbError>;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("{entity} '{id}' does not exist")]
    NotFound { entity: &'static str, id: String },

    #[error("cannot {operation} archived memory '{id}'")]
    AlreadyArchived { id: String, operation: String },

    #[error("memory '{id}' is not archived")]
    NotArchived { id: String },

    #[error("{message}")]
    InvalidInput { message: String },

    #[error("content is {actual} bytes, max is {max}")]
    ContentTooLarge { actual: usize, max: usize },

    #[error(
        "link already exists between '{source_id}' and '{target_id}' with relation '{relation}'"
    )]
    DuplicateLink {
        source_id: String,
        target_id: String,
        relation: String,
    },

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Internal(e.into())
    }
}

impl DbError {
    pub fn is_user_facing(&self) -> bool {
        !matches!(self, Self::Internal(..))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_is_user_facing() {
        let err = DbError::NotFound {
            entity: "memory",
            id: "abc-123".into(),
        };
        assert!(err.is_user_facing());
    }

    #[test]
    fn not_found_display() {
        let err = DbError::NotFound {
            entity: "memory",
            id: "abc-123".into(),
        };
        assert_eq!(err.to_string(), "memory 'abc-123' does not exist");
    }

    #[test]
    fn not_found_link_display() {
        let err = DbError::NotFound {
            entity: "link",
            id: "link-456".into(),
        };
        assert_eq!(err.to_string(), "link 'link-456' does not exist");
    }

    #[test]
    fn already_archived_is_user_facing() {
        let err = DbError::AlreadyArchived {
            id: "abc-123".into(),
            operation: "update".into(),
        };
        assert!(err.is_user_facing());
    }

    #[test]
    fn already_archived_display() {
        let err = DbError::AlreadyArchived {
            id: "abc-123".into(),
            operation: "update".into(),
        };
        assert_eq!(err.to_string(), "cannot update archived memory 'abc-123'");
    }

    #[test]
    fn invalid_input_is_user_facing() {
        let err = DbError::InvalidInput {
            message: "embedding is required when content is changed".into(),
        };
        assert!(err.is_user_facing());
    }

    #[test]
    fn invalid_input_display() {
        let err = DbError::InvalidInput {
            message: "embedding is required when content is changed".into(),
        };
        assert_eq!(
            err.to_string(),
            "embedding is required when content is changed"
        );
    }

    #[test]
    fn content_too_large_is_user_facing() {
        let err = DbError::ContentTooLarge {
            actual: 200_000,
            max: 100_000,
        };
        assert!(err.is_user_facing());
    }

    #[test]
    fn content_too_large_display() {
        let err = DbError::ContentTooLarge {
            actual: 200_000,
            max: 100_000,
        };
        assert_eq!(err.to_string(), "content is 200000 bytes, max is 100000");
    }

    #[test]
    fn internal_from_anyhow_is_not_user_facing() {
        let err = DbError::Internal(anyhow::anyhow!("disk full"));
        assert!(!err.is_user_facing());
    }

    #[test]
    fn internal_from_rusqlite_is_not_user_facing() {
        let sqlite_err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        );
        let err: DbError = sqlite_err.into();
        assert!(!err.is_user_facing());
    }

    #[test]
    fn not_archived_is_user_facing() {
        let err = DbError::NotArchived {
            id: "abc-123".into(),
        };
        assert!(err.is_user_facing());
    }

    #[test]
    fn not_archived_display() {
        let err = DbError::NotArchived {
            id: "abc-123".into(),
        };
        assert_eq!(err.to_string(), "memory 'abc-123' is not archived");
    }

    #[test]
    fn duplicate_link_is_user_facing() {
        let err = DbError::DuplicateLink {
            source_id: "a".into(),
            target_id: "b".into(),
            relation: "related_to".into(),
        };
        assert!(err.is_user_facing());
    }
}
