//! A minimal bencode reader/writer.
//!
//! Replaces the `bencode` npm package. Two properties matter more than
//! generality:
//!
//! 1. **Byte-exact info hashes.** The info hash is `sha1` over the *original*
//!    bytes of the `info` dictionary. Round-tripping through a decode/encode
//!    pair is only safe if the encoder is perfectly canonical, and a torrent in
//!    the wild may not be. [`decode_root`] therefore reports the byte span of
//!    the top-level `info` value so the hash can be taken over the source
//!    buffer directly.
//! 2. **Binary-safe strings.** `pieces` and path segments are arbitrary bytes,
//!    not UTF-8, so values carry `Vec<u8>`.
//!
//! Dictionaries use a `BTreeMap`, which both matches bencode's requirement that
//! keys be sorted and makes [`encode`] canonical.

use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BencodeError {
    #[error("unexpected end of input at byte {0}")]
    UnexpectedEnd(usize),
    #[error("invalid token {token:?} at byte {position}")]
    InvalidToken { token: char, position: usize },
    #[error("invalid integer at byte {0}")]
    InvalidInteger(usize),
    #[error("invalid string length at byte {0}")]
    InvalidLength(usize),
    #[error("dictionary key is not a byte string at byte {0}")]
    InvalidKey(usize),
    #[error("trailing data after the top-level value at byte {0}")]
    TrailingData(usize),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Lossy UTF-8 view — torrent names and paths are *supposed* to be UTF-8
    /// but frequently are not, and the original silently used `Buffer#toString`
    /// which is equally lossy.
    pub fn as_str(&self) -> Option<String> {
        self.as_bytes()
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict().and_then(|d| d.get(key.as_bytes()))
    }
}

/// Byte span of each dictionary value in the source buffer.
type ValueSpans = BTreeMap<Vec<u8>, Range<usize>>;

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8, BencodeError> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(BencodeError::UnexpectedEnd(self.pos))
    }

    fn value(&mut self) -> Result<Value, BencodeError> {
        match self.peek()? {
            b'i' => self.integer(),
            b'l' => self.list(),
            b'd' => self.dict().map(|(value, _)| value),
            b'0'..=b'9' => self.bytes().map(Value::Bytes),
            other => Err(BencodeError::InvalidToken {
                token: other as char,
                position: self.pos,
            }),
        }
    }

    fn integer(&mut self) -> Result<Value, BencodeError> {
        let start = self.pos;
        self.pos += 1; // 'i'
        let end = self.input[self.pos..]
            .iter()
            .position(|&b| b == b'e')
            .ok_or(BencodeError::UnexpectedEnd(self.pos))?
            + self.pos;
        let text = std::str::from_utf8(&self.input[self.pos..end])
            .map_err(|_| BencodeError::InvalidInteger(start))?;
        let parsed: i64 = text
            .parse()
            .map_err(|_| BencodeError::InvalidInteger(start))?;
        self.pos = end + 1;
        Ok(Value::Int(parsed))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, BencodeError> {
        let start = self.pos;
        let colon = self.input[self.pos..]
            .iter()
            .position(|&b| b == b':')
            .ok_or(BencodeError::UnexpectedEnd(self.pos))?
            + self.pos;
        let text = std::str::from_utf8(&self.input[self.pos..colon])
            .map_err(|_| BencodeError::InvalidLength(start))?;
        let length: usize = text
            .parse()
            .map_err(|_| BencodeError::InvalidLength(start))?;
        let from = colon + 1;
        let to = from
            .checked_add(length)
            .filter(|&to| to <= self.input.len())
            .ok_or(BencodeError::UnexpectedEnd(from))?;
        self.pos = to;
        Ok(self.input[from..to].to_vec())
    }

    fn list(&mut self) -> Result<Value, BencodeError> {
        self.pos += 1; // 'l'
        let mut items = Vec::new();
        while self.peek()? != b'e' {
            items.push(self.value()?);
        }
        self.pos += 1; // 'e'
        Ok(Value::List(items))
    }

    /// Returns the dictionary plus, for each key, the byte span of its value in
    /// the source buffer — the caller only cares about `info`, but recording
    /// them all keeps the parser simple.
    fn dict(&mut self) -> Result<(Value, ValueSpans), BencodeError> {
        self.pos += 1; // 'd'
        let mut map = BTreeMap::new();
        let mut spans: ValueSpans = BTreeMap::new();
        while self.peek()? != b'e' {
            if !self.peek()?.is_ascii_digit() {
                return Err(BencodeError::InvalidKey(self.pos));
            }
            let key = self.bytes()?;
            let value_start = self.pos;
            let value = self.value()?;
            spans.insert(key.clone(), value_start..self.pos);
            map.insert(key, value);
        }
        self.pos += 1; // 'e'
        Ok((Value::Dict(map), spans))
    }
}

/// Decodes a single bencode value.
pub fn decode(input: &[u8]) -> Result<Value, BencodeError> {
    let mut parser = Parser { input, pos: 0 };
    let value = parser.value()?;
    // Torrent files in the wild sometimes carry junk after the root dict;
    // the JS decoder ignored it, so trailing bytes are tolerated here too
    // and only reported for a caller that asks.
    Ok(value)
}

/// Decodes the top-level dictionary and returns the byte span of its `info`
/// value, for computing an info hash over the untouched source bytes.
pub fn decode_root(input: &[u8]) -> Result<(Value, Option<Range<usize>>), BencodeError> {
    let mut parser = Parser { input, pos: 0 };
    if parser.peek()? != b'd' {
        return Ok((parser.value()?, None));
    }
    let (value, spans) = parser.dict()?;
    Ok((value, spans.get(b"info".as_slice()).cloned()))
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(bytes) => {
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(bytes);
        }
        Value::List(items) => {
            out.push(b'l');
            for item in items {
                encode_into(item, out);
            }
            out.push(b'e');
        }
        Value::Dict(map) => {
            out.push(b'd');
            for (key, item) in map {
                encode_into(&Value::Bytes(key.clone()), out);
                encode_into(item, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(entries: &[(&str, Value)]) -> Value {
        Value::Dict(
            entries
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn round_trips_primitives() {
        for value in [
            Value::Int(42),
            Value::Int(-7),
            Value::Bytes(b"hello".to_vec()),
            Value::List(vec![Value::Int(1), Value::Bytes(b"a".to_vec())]),
            dict(&[("a", Value::Int(1)), ("b", Value::Bytes(b"x".to_vec()))]),
        ] {
            assert_eq!(decode(&encode(&value)).unwrap(), value);
        }
    }

    #[test]
    fn encodes_dict_keys_in_sorted_order() {
        let value = dict(&[("zebra", Value::Int(1)), ("apple", Value::Int(2))]);
        assert_eq!(encode(&value), b"d5:applei2e5:zebrai1ee".to_vec());
    }

    #[test]
    fn handles_non_utf8_byte_strings() {
        let raw = vec![0xff, 0x00, 0xfe];
        let encoded = encode(&Value::Bytes(raw.clone()));
        assert_eq!(decode(&encoded).unwrap().as_bytes().unwrap(), &raw[..]);
    }

    /// The whole point of `decode_root`: the reported span must be the exact
    /// bytes the info hash is taken over.
    #[test]
    fn reports_the_info_dict_span() {
        let torrent = b"d8:announce5:http:4:infod6:lengthi12e4:name3:abcee";
        let (_, span) = decode_root(torrent).unwrap();
        let span = span.expect("info span");
        assert_eq!(&torrent[span.clone()], b"d6:lengthi12e4:name3:abce");
        assert_eq!(
            decode(&torrent[span])
                .unwrap()
                .get("length")
                .unwrap()
                .as_int(),
            Some(12)
        );
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(matches!(
            decode(b"d6:length"),
            Err(BencodeError::UnexpectedEnd(_))
        ));
        assert!(matches!(
            decode(b"i12"),
            Err(BencodeError::UnexpectedEnd(_))
        ));
    }

    #[test]
    fn rejects_non_string_dict_keys() {
        assert!(matches!(
            decode(b"di1ei2ee"),
            Err(BencodeError::InvalidKey(_))
        ));
    }
}
