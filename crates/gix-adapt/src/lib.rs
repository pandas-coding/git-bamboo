//! Adapter utilities for gix read operations.
//!
//! This crate provides thin wrappers and helpers around `gix` APIs to
//! abstract platform differences and simplify usage in the engine.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AdaptError {
    #[error("gix error: {0}")]
    Gix(#[from] gix::open::Error),
}

/// Result type alias for this crate.
pub type Result<T> = std::result::Result<T, AdaptError>;
