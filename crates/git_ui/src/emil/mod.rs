//! Fork-only jj panel modules.
//!
//! All jj-specific code lives in this folder so that merging upstream zed
//! only ever touches the `emil` mod declaration and the re-exports in
//! `git_ui.rs` — additions, not edits to upstream files.

pub(crate) mod graph;
pub mod log;
pub(crate) mod settings;
pub(crate) mod status;

pub use log::init;
pub use status::JjStatusIndicator;
