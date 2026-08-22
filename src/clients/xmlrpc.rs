//! A minimal XML-RPC client for rTorrent.
//!
//! rTorrent's SCGI/HTTP interface speaks XML-RPC and nothing else. The surface
//! crust-seed needs is small — string, int, base64, array, struct, plus
//! `system.multicall` and fault detection — so this is hand-written rather than
//! pulled from a general-purpose crate, which keeps the dependency graph (and
//! the audit surface) smaller.

use std::fmt::Write as _;

use base64::Engine;
use quick_xml::events::Event;

#[derive(Debug, Clone, PartialEq)]
pub enum XmlRpcValue {
    Int(i64),
    Bool(bool),
    Str(String),
    Base64(Vec<u8>),
    Array(Vec<XmlRpcValue>),
    Struct(Vec<(String, XmlRpcValue)>),
}

impl XmlRpcValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            XmlRpcValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            XmlRpcValue::Int(i) => Some(*i),
            // rTorrent returns most numerics as strings.
            XmlRpcValue::Str(s) => s.parse().ok(),
            XmlRpcValue::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[XmlRpcValue]> {
        match self {
            XmlRpcValue::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&XmlRpcValue> {
        match self {
            XmlRpcValue::Struct(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// rTorrent wraps most scalar results in a one-element array.
    pub fn unwrap_singleton(&self) -> &XmlRpcValue {
        match self {
            XmlRpcValue::Array(items) if items.len() == 1 => &items[0],
            other => other,
        }
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

fn write_value(value: &XmlRpcValue, out: &mut String) {
    out.push_str("<value>");
    match value {
        XmlRpcValue::Int(i) => {
            let _ = write!(out, "<i8>{i}</i8>");
        }
        XmlRpcValue::Bool(b) => {
            let _ = write!(out, "<boolean>{}</boolean>", *b as u8);
        }
        XmlRpcValue::Str(s) => {
            let _ = write!(out, "<string>{}</string>", escape(s));
        }
        XmlRpcValue::Base64(bytes) => {
            let _ = write!(
                out,
                "<base64>{}</base64>",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
        }
        XmlRpcValue::Array(items) => {
            out.push_str("<array><data>");
            for item in items {
                write_value(item, out);
            }
            out.push_str("</data></array>");
        }
        XmlRpcValue::Struct(fields) => {
            out.push_str("<struct>");
            for (name, item) in fields {
                let _ = write!(out, "<member><name>{}</name>", escape(name));
                write_value(item, out);
                out.push_str("</member>");
            }
            out.push_str("</struct>");
        }
    }
    out.push_str("</value>");
}

pub fn build_request(method: &str, params: &[XmlRpcValue]) -> String {
    let mut out = String::from("<?xml version=\"1.0\"?><methodCall><methodName>");
    out.push_str(&escape(method));
    out.push_str("</methodName><params>");
    for param in params {
        out.push_str("<param>");
        write_value(param, &mut out);
        out.push_str("</param>");
    }
    out.push_str("</params></methodCall>");
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fault {
    pub code: i64,
    pub string: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum XmlRpcResponse {
    Value(XmlRpcValue),
    Fault(Fault),
}

/// Parses a `<methodResponse>` document.
pub fn parse_response(xml: &str) -> Result<XmlRpcResponse, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();

    // Stack of partially-built containers; scalars resolve immediately.
    enum Frame {
        Array(Vec<XmlRpcValue>),
        Struct(Vec<(String, XmlRpcValue)>),
        Member(Option<String>, Option<XmlRpcValue>),
    }

    let mut stack: Vec<Frame> = Vec::new();
    let mut completed: Vec<XmlRpcValue> = Vec::new();
    let mut text = String::new();
    let mut is_fault = false;
    // A <value> with no typed child is a string by the XML-RPC spec. Detecting
    // that needs a per-<value> record rather than a single flag, because
    // <value><struct>…</struct></value> nests <value> elements: the inner one
    // closing would otherwise clear the outer one's state and turn a whole
    // struct into an empty string.
    let mut value_marks: Vec<usize> = Vec::new();

    fn sink_len(stack: &[Frame], completed: &[XmlRpcValue]) -> usize {
        match stack.last() {
            Some(Frame::Array(items)) => items.len(),
            Some(Frame::Member(_, slot)) => slot.is_some() as usize,
            _ => completed.len(),
        }
    }

    fn push(stack: &mut [Frame], completed: &mut Vec<XmlRpcValue>, value: XmlRpcValue) {
        match stack.last_mut() {
            Some(Frame::Array(items)) => items.push(value),
            Some(Frame::Member(_, slot)) => *slot = Some(value),
            _ => completed.push(value),
        }
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(format!("invalid XML-RPC response: {e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "fault" => is_fault = true,
                    "value" => value_marks.push(sink_len(&stack, &completed)),
                    "array" => stack.push(Frame::Array(Vec::new())),
                    "struct" => stack.push(Frame::Struct(Vec::new())),
                    "member" => stack.push(Frame::Member(None, None)),
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Text(e)) => {
                text.push_str(&e.xml10_content().unwrap_or_default());
            }
            // quick-xml reports `&…;` separately from the text around it, so
            // references have to be stitched back in here or they vanish.
            // See `crate::xml`.
            Ok(Event::GeneralRef(e)) => {
                text.push_str(&crate::xml::resolve_reference(&e));
            }
            Ok(Event::CData(e)) => {
                text.push_str(&String::from_utf8_lossy(e.into_inner().as_ref()));
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                match name.as_str() {
                    "i4" | "int" | "i8" => {
                        let value = XmlRpcValue::Int(text.trim().parse().unwrap_or(0));
                        push(&mut stack, &mut completed, value);
                    }
                    "double" => {
                        let value =
                            XmlRpcValue::Int(text.trim().parse::<f64>().unwrap_or(0.0) as i64);
                        push(&mut stack, &mut completed, value);
                    }
                    "boolean" => {
                        let value = XmlRpcValue::Bool(text.trim() == "1");
                        push(&mut stack, &mut completed, value);
                    }
                    "string" => {
                        push(&mut stack, &mut completed, XmlRpcValue::Str(text.clone()));
                    }
                    "base64" => {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(text.trim())
                            .unwrap_or_default();
                        push(&mut stack, &mut completed, XmlRpcValue::Base64(bytes));
                    }
                    "value" => {
                        let mark = value_marks.pop().unwrap_or(0);
                        if sink_len(&stack, &completed) == mark {
                            // Nothing typed was produced: untyped
                            // <value>text</value> is a string.
                            push(&mut stack, &mut completed, XmlRpcValue::Str(text.clone()));
                        }
                    }
                    "name" => {
                        if let Some(Frame::Member(slot, _)) = stack.last_mut() {
                            *slot = Some(text.clone());
                        }
                    }
                    "array" => {
                        if let Some(Frame::Array(items)) = stack.pop() {
                            push(&mut stack, &mut completed, XmlRpcValue::Array(items));
                        }
                    }
                    "struct" => {
                        if let Some(Frame::Struct(fields)) = stack.pop() {
                            push(&mut stack, &mut completed, XmlRpcValue::Struct(fields));
                        }
                    }
                    "member" => {
                        if let Some(Frame::Member(name, value)) = stack.pop()
                            && let (Some(name), Some(value)) = (name, value)
                            && let Some(Frame::Struct(fields)) = stack.last_mut()
                        {
                            fields.push((name, value));
                        }
                    }
                    _ => {}
                }
                text.clear();
            }
            _ => {}
        }
        buf.clear();
    }

    let Some(value) = completed.pop() else {
        return Err("XML-RPC response contained no value".to_string());
    };

    if is_fault {
        return Ok(XmlRpcResponse::Fault(Fault {
            code: value
                .get("faultCode")
                .and_then(XmlRpcValue::as_i64)
                .unwrap_or(0),
            string: value
                .get("faultString")
                .and_then(XmlRpcValue::as_str)
                .unwrap_or_default()
                .to_string(),
        }));
    }
    Ok(XmlRpcResponse::Value(value))
}

/// `system.multicall` payload: a list of `{ methodName, params }` structs.
pub fn multicall_param(calls: Vec<(&str, Vec<XmlRpcValue>)>) -> XmlRpcValue {
    XmlRpcValue::Array(
        calls
            .into_iter()
            .map(|(method, params)| {
                XmlRpcValue::Struct(vec![
                    (
                        "methodName".to_string(),
                        XmlRpcValue::Str(method.to_string()),
                    ),
                    ("params".to_string(), XmlRpcValue::Array(params)),
                ])
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_every_value_kind() {
        let request = build_request(
            "d.name",
            &[
                XmlRpcValue::Str("ABC".into()),
                XmlRpcValue::Int(7),
                XmlRpcValue::Base64(vec![0x61, 0x62]),
            ],
        );
        assert!(request.contains("<methodName>d.name</methodName>"));
        assert!(request.contains("<string>ABC</string>"));
        assert!(request.contains("<i8>7</i8>"));
        assert!(request.contains("<base64>YWI=</base64>"));
    }

    #[test]
    fn special_characters_are_escaped() {
        let request = build_request("x", &[XmlRpcValue::Str("a<b&c".into())]);
        assert!(request.contains("a&lt;b&amp;c"));
    }

    #[test]
    fn scalar_responses_parse() {
        let xml = r#"<?xml version="1.0"?><methodResponse><params><param>
            <value><string>Some.Show.S01E01</string></value>
        </param></params></methodResponse>"#;
        match parse_response(xml).unwrap() {
            XmlRpcResponse::Value(value) => {
                assert_eq!(value.as_str(), Some("Some.Show.S01E01"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The XML-RPC spec says a <value> with no type element is a string, and
    /// rTorrent relies on that for `d.directory`.
    #[test]
    fn untyped_values_are_strings() {
        let xml = r#"<methodResponse><params><param><value>/downloads</value></param></params></methodResponse>"#;
        match parse_response(xml).unwrap() {
            XmlRpcResponse::Value(value) => assert_eq!(value.as_str(), Some("/downloads")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nested_arrays_parse() {
        let xml = r#"<methodResponse><params><param><value><array><data>
            <value><array><data><value><string>a</string></value></data></array></value>
            <value><array><data><value><string>b</string></value></data></array></value>
        </data></array></value></param></params></methodResponse>"#;
        let XmlRpcResponse::Value(value) = parse_response(xml).unwrap() else {
            panic!("expected a value");
        };
        let outer = value.as_array().unwrap();
        assert_eq!(outer.len(), 2);
        assert_eq!(outer[0].unwrap_singleton().as_str(), Some("a"));
        assert_eq!(outer[1].unwrap_singleton().as_str(), Some("b"));
    }

    #[test]
    fn structs_parse_by_member_name() {
        let xml = r#"<methodResponse><params><param><value><struct>
            <member><name>size</name><value><i4>42</i4></value></member>
            <member><name>path</name><value><string>a.mkv</string></value></member>
        </struct></value></param></params></methodResponse>"#;
        let XmlRpcResponse::Value(value) = parse_response(xml).unwrap() else {
            panic!("expected a value");
        };
        assert_eq!(value.get("size").and_then(XmlRpcValue::as_i64), Some(42));
        assert_eq!(
            value.get("path").and_then(XmlRpcValue::as_str),
            Some("a.mkv")
        );
    }

    /// rTorrent reports a missing torrent as a fault, which callers translate
    /// into NOT_FOUND rather than a hard error.
    #[test]
    fn faults_are_reported_separately() {
        let xml = r#"<methodResponse><fault><value><struct>
            <member><name>faultCode</name><value><i4>-501</i4></value></member>
            <member><name>faultString</name><value><string>Could not find info-hash.</string></value></member>
        </struct></value></fault></methodResponse>"#;
        match parse_response(xml).unwrap() {
            XmlRpcResponse::Fault(fault) => {
                assert_eq!(fault.code, -501);
                assert_eq!(fault.string, "Could not find info-hash.");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn multicall_params_have_the_expected_shape() {
        let param = multicall_param(vec![
            ("d.name", vec![XmlRpcValue::Str("ABC".into())]),
            ("d.directory", vec![XmlRpcValue::Str("ABC".into())]),
        ]);
        let calls = param.as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].get("methodName").and_then(XmlRpcValue::as_str),
            Some("d.name")
        );
    }

    #[test]
    fn numeric_strings_coerce_to_integers() {
        assert_eq!(XmlRpcValue::Str("123".into()).as_i64(), Some(123));
        assert_eq!(XmlRpcValue::Str("nope".into()).as_i64(), None);
    }
}
