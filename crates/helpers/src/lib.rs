//! Common simple helpers for this workspace.

#![warn(missing_docs)]
#![warn(clippy::missing_docs_in_private_items)]

pub mod macros;
pub mod truncated_display;

pub use macros::Mergeable;
pub use truncated_display::TruncatedDisplay;
