//! The command layer: the provisioning orchestrator ([`app`]) that executes a
//! plan through the [`CommandRunner`] seam, and the CLI dispatcher ([`cli`]).

pub mod app;
pub mod cli;
