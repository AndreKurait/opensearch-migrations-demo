//! Domain logic: the answer model, the wizard state machine, and the pure
//! provisioning planners (k8s manifests, KIND config, terraform, the MA
//! install + launch handoff). Each leaf module is re-exported at the crate
//! root.

pub mod manifests;
pub mod model;
pub mod plan;
pub mod terraform;
pub mod wizard;
