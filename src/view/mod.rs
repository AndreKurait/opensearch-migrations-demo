//! Presentation: plain-stdout output discipline ([`ui`]), the provisioning
//! progress timeline ([`progress`]), and the Ratatui wizard front-end
//! ([`tui`]). Each leaf is re-exported at the crate root.

pub mod progress;
pub mod tui;
pub mod ui;
