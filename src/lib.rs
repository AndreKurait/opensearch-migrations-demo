//! migration-assistant-demo — an interactive TUI harness that provisions a test
//! environment for the OpenSearch Migration Assistant, then hands off to the
//! `migration-assistant` CLI.
//!
//! The crate is grouped into four layered folders, mirroring the
//! migration-assistant CLI it sets up for; each leaf module is re-exported at
//! the crate root so the public path stays a flat `ma_demo::<module>`.
//!
//! ```text
//! src/
//!   core/      error · runner (I/O seam) · state · util
//!   domain/    model · wizard · manifests · terraform · plan
//!   view/      ui (output discipline) · tui (Ratatui wizard) · progress
//!   command/   app (provisioning orchestrator) · cli (dispatcher)
//! ```
//!
//! The split is deliberate: **pure decision logic** (the wizard state machine,
//! the manifest/terraform/compose emitters, the install-script builder) are
//! plain functions over plain data, unit-tested directly; **all external I/O**
//! (`docker`/`kind`/`kubectl`/`helm`/`curl`) goes through the
//! [`core::runner::CommandRunner`] seam so the whole provisioning pipeline is
//! asserted against a [`core::runner::MockRunner`] with no Docker access; and
//! the **TUI** is an Elm-architecture Ratatui front-end whose rendered `Buffer`
//! is asserted via `TestBackend`.

pub mod core;
pub mod domain;
pub mod view;

pub mod command;

// Flat re-exports — the public path is `ma_demo::<module>` regardless of folder.
pub use core::{error, runner, state, util};
pub use domain::{aws, manifests, model, plan, terraform, wizard};
pub use view::{dashboard, progress, tui, ui};

pub use command::{app, cli};
