//! Crate-level error types.

use std::error::Error as StdError;

/// Crate-level error type.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// An error with context describing what operation failed.
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },

    /// A simple error message.
    #[error("{0}")]
    Message(String),
}

impl Error {
    /// Create a simple message error.
    pub fn msg(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }

    /// Create an error with context wrapping another error.
    pub fn context(
        context: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(source),
        }
    }
}

/// Crate-level result type.
pub type Result<T> = std::result::Result<T, Error>;
