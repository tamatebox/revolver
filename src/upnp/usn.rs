//! Centralized handling of the USN / NT strings from SPEC §9.3.

const MEDIA_SERVER_DEVICE: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const CONTENT_DIRECTORY_SERVICE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CONNECTION_MANAGER_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// The 5 targets matched in SSDP advertiser NOTIFYs and in M-SEARCH ST receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NtTarget {
    RootDevice,
    Uuid,
    MediaServerDevice,
    ContentDirectoryService,
    ConnectionManagerService,
}

impl NtTarget {
    pub const ALL: [NtTarget; 5] = [
        NtTarget::RootDevice,
        NtTarget::Uuid,
        NtTarget::MediaServerDevice,
        NtTarget::ContentDirectoryService,
        NtTarget::ConnectionManagerService,
    ];

    /// Value used in the `NT` header of NOTIFY and the `ST` header of M-SEARCH responses.
    pub fn nt(&self, uuid: &str) -> String {
        match self {
            NtTarget::RootDevice => "upnp:rootdevice".to_string(),
            NtTarget::Uuid => format!("uuid:{}", uuid),
            NtTarget::MediaServerDevice => MEDIA_SERVER_DEVICE.to_string(),
            NtTarget::ContentDirectoryService => CONTENT_DIRECTORY_SERVICE.to_string(),
            NtTarget::ConnectionManagerService => CONNECTION_MANAGER_SERVICE.to_string(),
        }
    }

    /// Value used in the `USN` header.
    pub fn usn(&self, uuid: &str) -> String {
        match self {
            NtTarget::RootDevice => format!("uuid:{}::upnp:rootdevice", uuid),
            NtTarget::Uuid => format!("uuid:{}", uuid),
            NtTarget::MediaServerDevice => format!("uuid:{}::{}", uuid, MEDIA_SERVER_DEVICE),
            NtTarget::ContentDirectoryService => {
                format!("uuid:{}::{}", uuid, CONTENT_DIRECTORY_SERVICE)
            }
            NtTarget::ConnectionManagerService => {
                format!("uuid:{}::{}", uuid, CONNECTION_MANAGER_SERVICE)
            }
        }
    }

    /// Whether the M-SEARCH `ST` header matches this target.
    /// `ssdp:all` matches every target.
    pub fn matches_st(&self, st: &str, uuid: &str) -> bool {
        st == "ssdp:all" || st == self.nt(uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u1_nt_and_usn_strings() {
        let uuid = "AAAA";
        assert_eq!(NtTarget::RootDevice.nt(uuid), "upnp:rootdevice");
        assert_eq!(NtTarget::RootDevice.usn(uuid), "uuid:AAAA::upnp:rootdevice");
        assert_eq!(NtTarget::Uuid.nt(uuid), "uuid:AAAA");
        assert_eq!(NtTarget::Uuid.usn(uuid), "uuid:AAAA");
        assert_eq!(
            NtTarget::MediaServerDevice.nt(uuid),
            "urn:schemas-upnp-org:device:MediaServer:1"
        );
        assert_eq!(
            NtTarget::ContentDirectoryService.usn(uuid),
            "uuid:AAAA::urn:schemas-upnp-org:service:ContentDirectory:1"
        );
        assert_eq!(
            NtTarget::ConnectionManagerService.usn(uuid),
            "uuid:AAAA::urn:schemas-upnp-org:service:ConnectionManager:1"
        );
    }

    #[test]
    fn u2_matches_st() {
        let uuid = "XYZ";
        // `ssdp:all` matches every target.
        for t in NtTarget::ALL {
            assert!(t.matches_st("ssdp:all", uuid));
        }
        // Specific STs match pinpoint.
        assert!(NtTarget::RootDevice.matches_st("upnp:rootdevice", uuid));
        assert!(!NtTarget::RootDevice.matches_st("uuid:XYZ", uuid));
        assert!(NtTarget::Uuid.matches_st("uuid:XYZ", uuid));
        assert!(!NtTarget::Uuid.matches_st("uuid:OTHER", uuid));
        assert!(NtTarget::MediaServerDevice
            .matches_st("urn:schemas-upnp-org:device:MediaServer:1", uuid));
    }

    #[test]
    fn u3_empty_uuid_does_not_panic_and_produces_valid_strings() {
        // Even with a misconfigured empty uuid we must not panic, and the output
        // is still formally a valid string (in main.rs this is the UUID-generation
        // failure path; here it's a defensive check).
        let n = NtTarget::Uuid.nt("");
        assert_eq!(n, "uuid:");
        let u = NtTarget::RootDevice.usn("");
        assert_eq!(u, "uuid:::upnp:rootdevice");
    }

    #[test]
    fn u4_uuid_with_dashes_is_preserved_in_usn() {
        // The standard UUID v4 format must survive intact when embedded in the USN.
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let usn = NtTarget::ContentDirectoryService.usn(uuid);
        assert_eq!(
            usn,
            "uuid:550e8400-e29b-41d4-a716-446655440000::urn:schemas-upnp-org:service:ContentDirectory:1"
        );
        // Dashes are not escaped (SSDP uses plain text).
        assert!(usn.contains("550e8400-e29b-41d4-a716"));
    }

    #[test]
    fn u5_matches_st_with_unrelated_target_returns_false() {
        let uuid = "X";
        // Must not respond to another service's ST.
        assert!(!NtTarget::ContentDirectoryService.matches_st(
            "urn:schemas-upnp-org:service:ConnectionManager:1",
            uuid
        ));
        assert!(!NtTarget::Uuid.matches_st("upnp:rootdevice", uuid));
        // A completely unknown ST.
        assert!(!NtTarget::RootDevice.matches_st("urn:bogus:service:Foo:1", uuid));
    }
}
