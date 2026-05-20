//! Album art: extract embedded / folder images and cache them in memory
//! (SPEC §8.3, Phase 2 step 15).
//!
//! The HTTP handler lives in [`crate::http::art`]; the logic lives here.

pub mod cache;
pub mod extract;

pub use cache::{ArtCache, CachedArt};
