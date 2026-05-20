//! Embedded SCPD XML for ContentDirectory:1 / ConnectionManager:1.

pub const CONTENT_DIRECTORY: &str = include_str!("scpd_cd.xml");
pub const CONNECTION_MANAGER: &str = include_str!("scpd_cm.xml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sc1_content_directory_has_required_actions() {
        assert!(CONTENT_DIRECTORY.contains("<name>Browse</name>"));
        assert!(CONTENT_DIRECTORY.contains("<name>Search</name>"));
        assert!(CONTENT_DIRECTORY.contains("<name>GetSystemUpdateID</name>"));
        assert!(CONTENT_DIRECTORY.contains("<name>GetSearchCapabilities</name>"));
        assert!(CONTENT_DIRECTORY.contains("<name>GetSortCapabilities</name>"));
        assert!(CONTENT_DIRECTORY.contains("urn:schemas-upnp-org:service-1-0"));
    }

    #[test]
    fn sc2_connection_manager_has_required_actions() {
        assert!(CONNECTION_MANAGER.contains("<name>GetProtocolInfo</name>"));
        assert!(CONNECTION_MANAGER.contains("<name>GetCurrentConnectionIDs</name>"));
        assert!(CONNECTION_MANAGER.contains("<name>GetCurrentConnectionInfo</name>"));
    }
}
