//! OTLP/protobuf, decoded by hand.
//!
//! `http/protobuf` is the default OTLP protocol of most SDKs. The trace
//! messages are small and stable (opentelemetry-proto `trace/v1`), so a
//! wire-format reader of a hundred lines replaces a code generator and its
//! build step. Unknown fields are skipped, as protobuf requires.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::model::{Span, SpanEvent, SpanKind, Status, hex};

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at).ok_or("truncated varint")?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or("truncated field")?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    /// The next `(field number, value)`, or `None` at the end.
    fn next(&mut self) -> Result<Option<(u64, Wire<'a>)>, String> {
        if self.at >= self.bytes.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => Wire::Fixed64(u64::from_le_bytes(
                self.take(8)?.try_into().map_err(|_| "bad fixed64")?,
            )),
            2 => {
                let len = usize::try_from(self.varint()?).map_err(|_| "length too large")?;
                Wire::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Wire::Fixed32
            }
            other => return Err(format!("unsupported wire type {other}")),
        };
        Ok(Some((key >> 3, value)))
    }
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn any_value(bytes: &[u8]) -> Result<Value, String> {
    let mut reader = Reader::new(bytes);
    let mut out = Value::Null;
    while let Some((field, wire)) = reader.next()? {
        out = match (field, wire) {
            (1, Wire::Bytes(b)) => Value::String(utf8(b)),
            (2, Wire::Varint(v)) => Value::Bool(v != 0),
            (3, Wire::Varint(v)) => Value::from(v as i64),
            (4, Wire::Fixed64(v)) => {
                serde_json::Number::from_f64(f64::from_bits(v)).map_or(Value::Null, Value::Number)
            }
            (5, Wire::Bytes(b)) => {
                let mut values = Vec::new();
                let mut array = Reader::new(b);
                while let Some((field, wire)) = array.next()? {
                    if let (1, Wire::Bytes(item)) = (field, wire) {
                        values.push(any_value(item)?);
                    }
                }
                Value::Array(values)
            }
            (6, Wire::Bytes(b)) => {
                let mut map = Map::new();
                let mut list = Reader::new(b);
                while let Some((field, wire)) = list.next()? {
                    if let (1, Wire::Bytes(item)) = (field, wire) {
                        let (key, value) = key_value(item)?;
                        map.insert(key, value);
                    }
                }
                Value::Object(map)
            }
            (7, Wire::Bytes(b)) => Value::String(hex(b)),
            _ => continue,
        };
    }
    Ok(out)
}

fn key_value(bytes: &[u8]) -> Result<(String, Value), String> {
    let mut reader = Reader::new(bytes);
    let (mut key, mut value) = (String::new(), Value::Null);
    while let Some((field, wire)) = reader.next()? {
        match (field, wire) {
            (1, Wire::Bytes(b)) => key = utf8(b),
            (2, Wire::Bytes(b)) => value = any_value(b)?,
            _ => {}
        }
    }
    Ok((key, value))
}

fn resource(bytes: &[u8]) -> Result<BTreeMap<String, Value>, String> {
    let mut reader = Reader::new(bytes);
    let mut out = BTreeMap::new();
    while let Some((field, wire)) = reader.next()? {
        if let (1, Wire::Bytes(b)) = (field, wire) {
            let (key, value) = key_value(b)?;
            out.insert(key, value);
        }
    }
    Ok(out)
}

fn id(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty() && bytes.iter().any(|b| *b != 0)).then(|| hex(bytes))
}

fn span(
    bytes: &[u8],
    service: &str,
    resource: &BTreeMap<String, Value>,
    source: &str,
) -> Result<Option<Span>, String> {
    let mut reader = Reader::new(bytes);
    let mut out = Span {
        trace_id: String::new(),
        span_id: String::new(),
        parent_id: None,
        name: String::new(),
        service: service.to_owned(),
        kind: SpanKind::Internal,
        start_ns: 0,
        end_ns: 0,
        status: Status::Unset,
        status_message: None,
        attributes: BTreeMap::new(),
        resource: resource.clone(),
        events: Vec::new(),
        links: Vec::new(),
        source: source.to_owned(),
    };
    while let Some((field, wire)) = reader.next()? {
        match (field, wire) {
            (1, Wire::Bytes(b)) => out.trace_id = id(b).unwrap_or_default(),
            (2, Wire::Bytes(b)) => out.span_id = id(b).unwrap_or_default(),
            (4, Wire::Bytes(b)) => out.parent_id = id(b),
            (5, Wire::Bytes(b)) => out.name = utf8(b),
            (6, Wire::Varint(v)) => out.kind = SpanKind::from_otlp(v as i64),
            (7, Wire::Fixed64(v)) => out.start_ns = v,
            (8, Wire::Fixed64(v)) => out.end_ns = v,
            (9, Wire::Bytes(b)) => {
                let (key, value) = key_value(b)?;
                out.attributes.insert(key, value);
            }
            (11, Wire::Bytes(b)) => {
                let mut event = SpanEvent {
                    name: String::new(),
                    time_ns: 0,
                    attributes: BTreeMap::new(),
                };
                let mut r = Reader::new(b);
                while let Some((field, wire)) = r.next()? {
                    match (field, wire) {
                        (1, Wire::Fixed64(v)) => event.time_ns = v,
                        (2, Wire::Bytes(b)) => event.name = utf8(b),
                        (3, Wire::Bytes(b)) => {
                            let (key, value) = key_value(b)?;
                            event.attributes.insert(key, value);
                        }
                        _ => {}
                    }
                }
                out.events.push(event);
            }
            (13, Wire::Bytes(b)) => {
                let (mut trace, mut own) = (None, None);
                let mut r = Reader::new(b);
                while let Some((field, wire)) = r.next()? {
                    match (field, wire) {
                        (1, Wire::Bytes(b)) => trace = id(b),
                        (2, Wire::Bytes(b)) => own = id(b),
                        _ => {}
                    }
                }
                if let (Some(trace), Some(own)) = (trace, own) {
                    out.links.push((trace, own));
                }
            }
            (15, Wire::Bytes(b)) => {
                let mut r = Reader::new(b);
                while let Some((field, wire)) = r.next()? {
                    match (field, wire) {
                        (2, Wire::Bytes(b)) if !b.is_empty() => out.status_message = Some(utf8(b)),
                        (3, Wire::Varint(1)) => out.status = Status::Ok,
                        (3, Wire::Varint(2)) => out.status = Status::Error,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if out.end_ns == 0 {
        out.end_ns = out.start_ns;
    }
    Ok((out.trace_id.len() == 32 && out.span_id.len() == 16).then_some(out))
}

pub fn parse(bytes: &[u8], source: &str) -> Result<Vec<Span>, String> {
    let mut out = Vec::new();
    let mut request = Reader::new(bytes);
    while let Some((field, wire)) = request.next()? {
        let (1, Wire::Bytes(resource_spans)) = (field, wire) else {
            continue;
        };
        let mut attrs = BTreeMap::new();
        let mut scopes = Vec::new();
        let mut reader = Reader::new(resource_spans);
        while let Some((field, wire)) = reader.next()? {
            match (field, wire) {
                (1, Wire::Bytes(b)) => attrs = resource(b)?,
                (2, Wire::Bytes(b)) => scopes.push(b),
                _ => {}
            }
        }
        let service = attrs
            .get("service.name")
            .and_then(Value::as_str)
            .unwrap_or("unknown_service")
            .to_owned();
        for scope in scopes {
            let mut reader = Reader::new(scope);
            while let Some((field, wire)) = reader.next()? {
                if let (2, Wire::Bytes(b)) = (field, wire)
                    && let Some(span) = span(b, &service, &attrs, source)?
                {
                    out.push(span);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod encode {
    //! A protobuf writer for tests: builds requests the decoder must read.

    pub fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    pub fn bytes(field: u64, data: &[u8], out: &mut Vec<u8>) {
        varint(field << 3 | 2, out);
        varint(data.len() as u64, out);
        out.extend_from_slice(data);
    }

    pub fn uint(field: u64, value: u64, out: &mut Vec<u8>) {
        varint(field << 3, out);
        varint(value, out);
    }

    pub fn fixed64(field: u64, value: u64, out: &mut Vec<u8>) {
        varint(field << 3 | 1, out);
        out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn string_kv(key: &str, value: &str) -> Vec<u8> {
        let mut any = Vec::new();
        bytes(1, value.as_bytes(), &mut any);
        let mut kv = Vec::new();
        bytes(1, key.as_bytes(), &mut kv);
        bytes(2, &any, &mut kv);
        kv
    }

    pub fn int_kv(key: &str, value: u64) -> Vec<u8> {
        let mut any = Vec::new();
        uint(3, value, &mut any);
        let mut kv = Vec::new();
        bytes(1, key.as_bytes(), &mut kv);
        bytes(2, &any, &mut kv);
        kv
    }
}

#[cfg(test)]
mod tests {
    use super::encode::*;
    use super::*;
    use serde_json::json;

    fn request() -> Vec<u8> {
        let mut span = Vec::new();
        bytes(1, &[0xab; 16], &mut span);
        bytes(
            2,
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            &mut span,
        );
        bytes(4, &[], &mut span);
        bytes(5, b"reserve stock", &mut span);
        uint(6, 3, &mut span);
        fixed64(7, 1_000, &mut span);
        fixed64(8, 5_000, &mut span);
        bytes(9, &int_kv("items", 3), &mut span);
        bytes(9, &string_kv("peer.service", "stock"), &mut span);
        uint(99, 7, &mut span); // an unknown field
        let mut status = Vec::new();
        bytes(2, b"out of stock", &mut status);
        uint(3, 2, &mut status);
        bytes(15, &status, &mut span);
        let mut scope = Vec::new();
        bytes(2, &span, &mut scope);
        let mut resource = Vec::new();
        bytes(1, &string_kv("service.name", "orders"), &mut resource);
        let mut resource_spans = Vec::new();
        bytes(1, &resource, &mut resource_spans);
        bytes(2, &scope, &mut resource_spans);
        let mut out = Vec::new();
        bytes(1, &resource_spans, &mut out);
        out
    }

    #[test]
    fn decodes_an_export_request() {
        let spans = parse(&request(), "otlp").unwrap();
        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.trace_id, "ab".repeat(16));
        assert_eq!(span.span_id, "0102030405060708");
        assert_eq!(span.parent_id, None);
        assert_eq!(span.service, "orders");
        assert_eq!(span.kind, SpanKind::Client);
        assert_eq!(span.duration_ns(), 4_000);
        assert_eq!(span.attributes["items"], json!(3));
        assert_eq!(span.peer().as_deref(), Some("stock"));
        assert_eq!(span.status, Status::Error);
        assert_eq!(span.status_message.as_deref(), Some("out of stock"));
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        let full = request();
        for cut in 1..full.len() {
            let _ = parse(&full[..cut], "otlp");
        }
        assert!(parse(&[0x0a, 0xff], "otlp").is_err());
    }
}
