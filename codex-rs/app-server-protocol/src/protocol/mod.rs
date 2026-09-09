// Module declarations for the app-server protocol namespace.
// Exposes protocol pieces used by `lib.rs` via `pub use protocol::common::*;`.

pub mod common;
pub mod event_mapping;
pub mod item_builders;
#[cfg(test)]
#[path = "mailbox_presentation_tests.rs"]
mod mailbox_presentation_tests;
mod mappers;
mod serde_helpers;
#[cfg(test)]
#[path = "task_path_presentation_tests.rs"]
mod task_path_presentation_tests;
pub mod thread_history;
pub mod thread_history_projection;
pub mod v1;
pub mod v2;
