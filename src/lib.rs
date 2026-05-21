//! Library entry point of the revolver crate.
//!
//! All modules are declared here so `tests/*.rs` can call `revolver::*` as
//! integration tests. `main.rs` stays thin and only handles the startup sequence.

pub mod art;
pub mod browse;
pub mod config;
pub mod config_catalog;
pub mod db;
pub mod error;
pub mod http;
pub mod normalize;
pub mod random;
pub mod scan;
pub mod ssdp;
pub mod state;
pub mod upnp;
