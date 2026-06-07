//! Presentation: plain-stdout output discipline ([`ui`]), the provisioning
//! progress timeline ([`progress`]), the Ratatui wizard front-end ([`tui`]),
//! and the live status dashboard ([`dashboard`]). Each leaf is re-exported at
//! the crate root.

pub mod dashboard;
pub mod progress;
pub mod tui;
pub mod ui;
