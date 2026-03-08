//! Shared overlay driver configuration types.

use serde::{Deserialize, Serialize};

/// Which overlay implementation to use.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayDriver {
    /// Try kernel overlayfs first, fall back to fuse-overlayfs.
    #[default]
    Auto,
    /// Force kernel overlayfs (may require privileges or kernel >= 5.11).
    KernelOverlay,
    /// Force fuse-overlayfs (requires the binary to be installed).
    FuseOverlayfs,
}

#[cfg(test)]
mod tests {
    use super::OverlayDriver;

    #[test]
    fn overlay_driver_default_is_auto() {
        let driver = OverlayDriver::default();
        assert_eq!(driver, OverlayDriver::Auto);
    }

    #[test]
    fn overlay_driver_serde_round_trips() {
        let json = serde_json::to_string(&OverlayDriver::FuseOverlayfs)
            .expect("overlay driver should serialize");
        assert_eq!(json, "\"fuse_overlayfs\"");

        let parsed: OverlayDriver =
            serde_json::from_str("\"auto\"").expect("auto should deserialize");
        assert_eq!(parsed, OverlayDriver::Auto);

        let parsed: OverlayDriver =
            serde_json::from_str("\"kernel_overlay\"").expect("kernel_overlay should deserialize");
        assert_eq!(parsed, OverlayDriver::KernelOverlay);
    }
}
