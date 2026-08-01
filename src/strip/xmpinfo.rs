//! The few XMP facts worth showing in "Image info" that EXIF has no field for.
//!
//! XMP is RDF/XML and we deliberately do not pull in an XML parser for it: these
//! are flat, well-known property names, so the packet is scanned as text for both
//! spellings XMP allows - an attribute (`ns:Name="value"`) and an element
//! (`<ns:Name>value</ns:Name>`). A miss shows nothing, which is the right failure
//! for an informational panel.
//!
//! What it surfaces, and why each one earns its line:
//!
//! - **Digital source type** / **AI prompt** (`Iptc4xmpExt`, ExifTool 13.40): the
//!   standard way an image declares it was generated or edited by AI.
//! - **People** (`mwg-rs` regions): the names Lightroom and phone camera apps
//!   write into face regions. Worth knowing is in there before you share a file.

/// A labelled fact pulled from an XMP packet.
pub(super) type Fact = (&'static str, String);

/// The properties we look for, as `(label, xmp property name)`.
const WANTED: &[(&str, &str)] = &[
    ("Digital source type", "Iptc4xmpExt:DigitalSourceType"),
    ("AI prompt", "Iptc4xmpExt:AiPrompt"),
    ("AI prompt author", "Iptc4xmpExt:AiPromptWriterName"),
];

/// Every fact we can find in `packet`.
pub(super) fn facts(packet: &str) -> Vec<Fact> {
    let mut out: Vec<Fact> = WANTED
        .iter()
        .filter_map(|(label, prop)| property(packet, prop).map(|v| (*label, v)))
        .collect();
    let people = people(packet);
    if !people.is_empty() {
        out.push(("People", people.join(", ")));
    }
    out
}

/// Read one XMP property, accepting either the attribute or the element form.
fn property(packet: &str, prop: &str) -> Option<String> {
    let attr = format!("{prop}=\"");
    if let Some(i) = packet.find(&attr) {
        let rest = &packet[i + attr.len()..];
        let end = rest.find('"')?;
        return clean(&rest[..end]);
    }
    let open = format!("<{prop}>");
    let i = packet.find(&open)?;
    let rest = &packet[i + open.len()..];
    let end = rest.find("</")?;
    clean(&rest[..end])
}

/// Named face regions (`mwg-rs:PersonDisplayName`), deduplicated, in file order.
fn people(packet: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for form in ["mwg-rs:PersonDisplayName=\"", "<mwg-rs:PersonDisplayName>"] {
        let mut rest = packet;
        while let Some(i) = rest.find(form) {
            rest = &rest[i + form.len()..];
            let end = if form.ends_with('"') {
                rest.find('"')
            } else {
                rest.find("</")
            };
            let Some(end) = end else { break };
            if let Some(name) = clean(&rest[..end]) {
                if !out.contains(&name) {
                    out.push(name);
                }
            }
            rest = &rest[end..];
        }
    }
    out
}

/// Trim, unescape the handful of XML entities that appear in these values, and
/// reject anything empty or implausibly long for a one-line info row.
fn clean(raw: &str) -> Option<String> {
    let s = raw
        .trim()
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(if s.chars().count() > 300 {
        let cut: String = s.chars().take(300).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    })
}

/// Pull the XMP packet out of whatever container `bytes` is, as text.
///
/// Uses the `<?xpacket` / `x:xmpmeta` markers rather than per-format box walking:
/// XMP is stored as a literal XML packet in every one of these containers, so one
/// scan covers JPEG APP1, PNG `iTXt`, WebP `XMP `, and the HEIC/AVIF `mime` item
/// without four parsers that can each be wrong.
pub(super) fn packet(bytes: &[u8]) -> Option<String> {
    const OPEN: &[u8] = b"<x:xmpmeta";
    const CLOSE: &[u8] = b"</x:xmpmeta>";
    let start = bytes.windows(OPEN.len()).position(|w| w == OPEN)?;
    let end = bytes[start..]
        .windows(CLOSE.len())
        .position(|w| w == CLOSE)?
        + start
        + CLOSE.len();
    Some(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AI: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF><rdf:Description
        Iptc4xmpExt:DigitalSourceType="trainedAlgorithmicMedia"
        Iptc4xmpExt:AiPrompt="a cat wearing &amp; a hat"/></rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn reads_ai_tags_in_attribute_form() {
        let f = facts(AI);
        assert_eq!(
            f.iter()
                .find(|(l, _)| *l == "Digital source type")
                .map(|(_, v)| v.as_str()),
            Some("trainedAlgorithmicMedia")
        );
        assert_eq!(
            f.iter()
                .find(|(l, _)| *l == "AI prompt")
                .map(|(_, v)| v.as_str()),
            Some("a cat wearing & a hat"),
            "XML entities must be unescaped"
        );
    }

    #[test]
    fn reads_ai_tags_in_element_form() {
        let x = "<x:xmpmeta><Iptc4xmpExt:DigitalSourceType>compositeCapture\
                 </Iptc4xmpExt:DigitalSourceType></x:xmpmeta>";
        assert_eq!(facts(x).len(), 1);
    }

    #[test]
    fn collects_face_region_names_without_duplicates() {
        let x = r#"<x:xmpmeta><mwg-rs:Regions><rdf:Bag>
            <rdf:li mwg-rs:PersonDisplayName="Ada"/>
            <rdf:li mwg-rs:PersonDisplayName="Grace"/>
            <rdf:li mwg-rs:PersonDisplayName="Ada"/>
            </rdf:Bag></mwg-rs:Regions></x:xmpmeta>"#;
        let f = facts(x);
        assert_eq!(
            f.iter()
                .find(|(l, _)| *l == "People")
                .map(|(_, v)| v.as_str()),
            Some("Ada, Grace")
        );
    }

    #[test]
    fn a_packet_without_those_properties_yields_nothing() {
        assert!(facts("<x:xmpmeta><dc:title>Hello</dc:title></x:xmpmeta>").is_empty());
    }

    #[test]
    fn finds_the_packet_inside_a_container() {
        let mut file = b"\xFF\xD8\xFF\xE1..junk..".to_vec();
        file.extend_from_slice(AI.as_bytes());
        file.extend_from_slice(b"..more junk..");
        let p = packet(&file).expect("packet not found");
        assert!(p.starts_with("<x:xmpmeta"));
        assert!(p.ends_with("</x:xmpmeta>"));
        assert!(!facts(&p).is_empty());
    }

    #[test]
    fn no_packet_is_not_an_error() {
        assert!(packet(b"just some bytes").is_none());
    }
}
