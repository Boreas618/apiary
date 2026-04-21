//! First-class image registry for Apiary.
//!
//! Manages Docker images as named rootfs sources.  The registry starts
//! empty; clients register images at runtime via the HTTP API, which
//! delegates to [`ImageLoader`] to pull, extract layers into a
//! content-addressable cache (`layers_dir/{sha256_hex}/`), and update
//! the registry. Subsequent session creation uses the registry to
//! resolve image name → layer-path list.
//!
//! ## Modules
//!
//! - [`extractor`] — Docker layer extraction with whiteout conversion
//! - [`loader`] — Async pull+extract+register pipeline with dedupe
//! - [`manifest`] — JSON manifest I/O for the image→layers map
//! - [`registry`] — Mutable in-memory image registry

pub mod extractor;
pub mod loader;
pub mod manifest;
pub mod registry;

pub use extractor::LayerExtractor;
pub use loader::{ImageLoader, ImageStage, LoadOutcome};
pub use registry::ImageRegistry;
