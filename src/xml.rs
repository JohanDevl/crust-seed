//! Entity handling shared by the two hand-written XML parsers (torznab and
//! XML-RPC).
//!
//! quick-xml 0.38 split `&…;` out of `Event::Text` into its own
//! `Event::GeneralRef`. A reader that matches only on `Event::Text` therefore
//! drops every reference silently instead of failing loudly: a Prowlarr
//! download link
//!
//! ```text
//! <link>http://host/26/download?apikey=KEY&amp;link=TOKEN&amp;file=NAME</link>
//! ```
//!
//! collapsed to `?apikey=KEYlink=TOKENfile=NAME` — one parameter holding a
//! bogus key — and every snatch came back `401 Unauthorized`.

use quick_xml::events::BytesRef;

/// Resolves one entity or character reference to the text it stands for.
///
/// xml2js parses with sax in strict mode, which resolves numeric references
/// and the whole HTML entity table, then calls `strictFail` on anything else —
/// so an unknown name made xml2js reject and cross-seed report the response as
/// invalid XML. Two deliberate differences:
///
/// - only the five XML predefined entities are resolved by name. Generated
///   torznab and XML-RPC never emit the HTML-only names, which exist in sax
///   because it doubles as an HTML parser.
/// - an unresolvable name is kept verbatim (`&foo;`) rather than failing the
///   document. sax returns that same literal alongside its error, and losing a
///   whole indexer response over one stray entity in a release title is a worse
///   outcome than carrying the entity through.
pub fn resolve_reference(reference: &BytesRef<'_>) -> String {
    if let Ok(Some(resolved)) = reference.resolve_char_ref() {
        return resolved.to_string();
    }
    let name = reference.decode().unwrap_or_default();
    match name.as_ref() {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "apos" => "'".to_string(),
        "quot" => "\"".to_string(),
        other => format!("&{other};"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(reference: &str) -> String {
        resolve_reference(&BytesRef::new(reference))
    }

    #[test]
    fn predefined_entities_resolve() {
        assert_eq!(resolve("amp"), "&");
        assert_eq!(resolve("lt"), "<");
        assert_eq!(resolve("gt"), ">");
        assert_eq!(resolve("apos"), "'");
        assert_eq!(resolve("quot"), "\"");
    }

    #[test]
    fn numeric_references_resolve_in_both_bases() {
        assert_eq!(resolve("#38"), "&");
        assert_eq!(resolve("#x26"), "&");
        assert_eq!(resolve("#233"), "é");
    }

    #[test]
    fn unknown_names_are_kept_verbatim() {
        assert_eq!(resolve("nbsp"), "&nbsp;");
        assert_eq!(resolve("#xZZ"), "&#xZZ;");
    }
}
