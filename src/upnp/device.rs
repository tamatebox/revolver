//! UPnP Device Description XML generation (SPEC §5.2).

use crate::upnp::xml::escape_text as xml_escape;

const TEMPLATE: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion>
    <major>1</major>
    <minor>0</minor>
  </specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>{friendly_name}</friendlyName>
    <manufacturer>revolver</manufacturer>
    <modelName>revolver</modelName>
    <modelNumber>0.1.0</modelNumber>
    <UDN>uuid:{uuid}</UDN>
    <iconList>
      <icon>
        <mimetype>image/png</mimetype>
        <width>1024</width>
        <height>1024</height>
        <depth>32</depth>
        <url>/icon/1024.png</url>
      </icon>
      <icon>
        <mimetype>image/png</mimetype>
        <width>512</width>
        <height>512</height>
        <depth>32</depth>
        <url>/icon/512.png</url>
      </icon>
      <icon>
        <mimetype>image/png</mimetype>
        <width>120</width>
        <height>120</height>
        <depth>32</depth>
        <url>/icon/120.png</url>
      </icon>
      <icon>
        <mimetype>image/png</mimetype>
        <width>48</width>
        <height>48</height>
        <depth>32</depth>
        <url>/icon/48.png</url>
      </icon>
    </iconList>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <SCPDURL>/scpd/cd.xml</SCPDURL>
        <controlURL>/control/cd</controlURL>
        <eventSubURL>/event/cd</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/scpd/cm.xml</SCPDURL>
        <controlURL>/control/cm</controlURL>
        <eventSubURL>/event/cm</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>
"#;

/// Generate the Device Description XML. `uuid` is the raw UUID value (no `uuid:` prefix);
/// `friendly_name` is XML-entity-escaped.
pub fn description_xml(uuid: &str, friendly_name: &str) -> String {
    TEMPLATE
        .replace("{uuid}", &xml_escape(uuid))
        .replace("{friendly_name}", &xml_escape(friendly_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d1_description_contains_uuid_friendly_name_and_services() {
        let xml = description_xml("AAAA", "Test Server");
        assert!(xml.contains("<UDN>uuid:AAAA</UDN>"));
        assert!(xml.contains("<friendlyName>Test Server</friendlyName>"));
        assert!(xml.contains("urn:schemas-upnp-org:service:ContentDirectory:1"));
        assert!(xml.contains("urn:schemas-upnp-org:service:ConnectionManager:1"));
        assert!(xml.contains("<SCPDURL>/scpd/cd.xml</SCPDURL>"));
        assert!(xml.contains("<SCPDURL>/scpd/cm.xml</SCPDURL>"));
    }

    #[test]
    fn d3_description_advertises_icon_list() {
        let xml = description_xml("AAAA", "Test Server");
        assert!(xml.contains("<iconList>"));
        assert!(xml.contains("<url>/icon/48.png</url>"));
        assert!(xml.contains("<url>/icon/120.png</url>"));
        assert!(xml.contains("<url>/icon/512.png</url>"));
        assert!(xml.contains("<url>/icon/1024.png</url>"));
        assert!(xml.contains("<mimetype>image/png</mimetype>"));

        // Order: largest → smallest. UPnP MediaServer 1.0 does not assign
        // meaning to `<icon>` order, but some control points (Linn DSM/2 /
        // Linn App verified 2026-05-23) pick the first entry regardless
        // of advertised dimensions. Putting 1024 first biases those
        // clients toward the high-resolution asset; clients that respect
        // the declared dimensions are free to pick whichever size fits.
        let i1024 = xml.find("<url>/icon/1024.png</url>").unwrap();
        let i512 = xml.find("<url>/icon/512.png</url>").unwrap();
        let i120 = xml.find("<url>/icon/120.png</url>").unwrap();
        let i48 = xml.find("<url>/icon/48.png</url>").unwrap();
        assert!(
            i1024 < i512 && i512 < i120 && i120 < i48,
            "iconList must list 1024 → 512 → 120 → 48"
        );
    }

    #[test]
    fn d2_friendly_name_is_xml_escaped() {
        // `'` is not escaped in text nodes per XML 1.0 — only `& < >` matter here.
        let xml = description_xml("AAAA", "Tom & Jerry's <Server>");
        assert!(xml.contains("Tom &amp; Jerry's &lt;Server&gt;"));
        assert!(!xml.contains("Tom & Jerry's <Server>"));
    }
}
