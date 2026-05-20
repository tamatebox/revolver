//! UPnP device icons (SPEC §5.2).
//!
//! Two PNGs embedded at build time and served from `/icon/{size}.png`. The
//! Device Description's `<iconList>` advertises them so control points can
//! show a thumbnail next to the server name.

pub const ICON_48_PNG: &[u8] = include_bytes!("../../assets/icon-48.png");
pub const ICON_120_PNG: &[u8] = include_bytes!("../../assets/icon-120.png");

pub const MIME: &str = "image/png";
