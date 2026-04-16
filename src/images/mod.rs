//! First-class image registry for Apiary.
//!
//! Manages Docker images as named rootfs sources.  At daemon startup the
//! registry ensures every requested image has its layers extracted to a
//! local content-addressable cache (`layers_dir/{sha256_hex}/`), then
//! provides fast name → layer-path resolution for session creation.
//!
//! ## Modules
//!
//! - [`extractor`] — Docker layer extraction with whiteout conversion
//! - [`manifest`] — Optional JSON manifest I/O for the image→layers map
//! - [`registry`] — In-memory image registry built at startup

pub mod extractor;
pub mod manifest;
pub mod registry;

pub use extractor::LayerExtractor;
pub use registry::ImageRegistry;
