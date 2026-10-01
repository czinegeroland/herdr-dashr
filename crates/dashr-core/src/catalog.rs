//! The spans the code has, and where (DASHR-VIEW-005).
//!
//! The human wants to see every span the feature's code makes and jump to
//! the line that makes it. Three sources say where a span comes from, best
//! first:
//!
//! 1. the catalog the agent writes after instrumenting (`dashr spans set`):
//!    service, span, file, line or function, why;
//! 2. a flow step's `code` (`path/File.cs:Function` or `path/file.py:42`);
//! 3. OpenTelemetry's `code.*` attributes on observed spans
//!    (`code.file.path`/`code.filepath`, `code.line.number`/`code.lineno`,
//!    `code.function.name`/`code.function`).
//!
//! The inventory joins them with what the traces show — how often a span
//! was seen, its last duration, errors, the attribute keys it carries — so
//! a span the code has but no run produced yet is visible too.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::flow::{Flow, glob};
use crate::model::{Span, SpanKind};

/// One span the agent says the code makes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogEntry {
    pub service: String,
    pub span: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SpanKind>,
    /// Relative to the catalog's root, or absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The attribute keys the span sets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// The repository the files are in; defaults to where `dashr spans set`
    /// ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub spans: Vec<CatalogEntry>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, String> {
        let catalog: Catalog =
            serde_json::from_str(json).map_err(|error| format!("invalid span catalog: {error}"))?;
        for entry in &catalog.spans {
            if entry.service.trim().is_empty() || entry.span.trim().is_empty() {
                return Err("every catalog entry needs a service and a span".into());
            }
        }
        Ok(catalog)
    }
}

/// Where a span is made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CodeRef {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
}

/// `path/File.cs:Function`, `path/file.py:42`, `path/file.ts` or
/// `path/file.go:42:Handle`. Windows drive letters (`C:\...`) are kept.
pub fn parse_code(text: &str) -> Option<CodeRef> {
    let text = text.trim();
    // Drop a trailing note: `tools/Program.cs (root span)`.
    let text = text.split(" (").next().unwrap_or(text).trim();
    if text.is_empty() || !text.contains('.') {
        return None;
    }
    let drive =
        text.len() > 2 && text.as_bytes()[1] == b':' && text.as_bytes()[0].is_ascii_alphabetic();
    let (prefix, rest) = if drive { text.split_at(2) } else { ("", text) };
    let mut parts = rest.split(':');
    let path = format!("{prefix}{}", parts.next()?);
    // The path must look like a file (an extension after the last slash).
    let name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
    if !name.contains('.') || name.ends_with('.') {
        return None;
    }
    let mut code = CodeRef {
        file: path,
        line: None,
        function: None,
    };
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.parse::<u32>() {
            Ok(line) if code.line.is_none() => code.line = Some(line),
            _ => code.function = Some(part.to_owned()),
        }
    }
    Some(code)
}

fn text_attr(span: &Span, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match span.attribute(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

/// A span's location from OpenTelemetry's `code.*` attributes.
pub fn code_from_attributes(span: &Span) -> Option<CodeRef> {
    let file = text_attr(span, &["code.file.path", "code.filepath"])?;
    Some(CodeRef {
        file,
        line: text_attr(span, &["code.line.number", "code.lineno"]).and_then(|l| l.parse().ok()),
        function: text_attr(span, &["code.function.name", "code.function"]),
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpanInfo {
    pub service: String,
    pub span: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<SpanKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<CodeRef>,
    /// Where the location came from: `catalog`, `flow` or `attributes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub located_by: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Attribute keys the catalog or a flow expects.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expected_attributes: Vec<String>,
    /// How many spans of this name the traces hold.
    pub seen: usize,
    pub errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_span_id: Option<String>,
    /// The last observed span's attributes (raw: the human's view).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub last_attributes: BTreeMap<String, Value>,
    /// In the catalog or a flow: the agent says the code makes it.
    pub planned: bool,
}

impl SpanInfo {
    fn new(service: &str, span: &str) -> Self {
        Self {
            service: service.to_owned(),
            span: span.to_owned(),
            kind: None,
            code: None,
            located_by: None,
            why: None,
            expected_attributes: Vec::new(),
            seen: 0,
            errors: 0,
            last_duration_ms: None,
            last_trace_id: None,
            last_span_id: None,
            last_attributes: BTreeMap::new(),
            planned: false,
        }
    }
}

/// The inventory: every planned span and every observed one, grouped by
/// service (services in order of appearance), planned spans first.
pub fn inventory(
    catalog: &[CatalogEntry],
    flows: &[&Flow],
    traces: &[(Vec<Span>, u64)],
) -> Vec<SpanInfo> {
    let mut out: Vec<SpanInfo> = Vec::new();
    let mut index: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut add = |out: &mut Vec<SpanInfo>, service: &str, span: &str| -> usize {
        *index
            .entry((service.to_owned(), span.to_owned()))
            .or_insert_with(|| {
                out.push(SpanInfo::new(service, span));
                out.len() - 1
            })
    };
    for entry in catalog {
        let i = add(&mut out, &entry.service, &entry.span);
        let info = &mut out[i];
        info.planned = true;
        info.kind = entry.kind.or(info.kind);
        info.why = entry.why.clone().or(info.why.take());
        if let Some(file) = &entry.file {
            info.code = Some(CodeRef {
                file: file.clone(),
                line: entry.line,
                function: entry.function.clone(),
            });
            info.located_by = Some("catalog");
        }
        for key in &entry.attributes {
            if !info.expected_attributes.contains(key) {
                info.expected_attributes.push(key.clone());
            }
        }
    }
    // Flow steps: their names may be globs; a glob step attaches to the
    // observed spans it matches, a literal one is a planned span itself.
    let mut glob_steps = Vec::new();
    for flow in flows {
        for step in &flow.steps {
            let literal = !step.service.contains(['*', '?']) && !step.span.contains(['*', '?']);
            if literal {
                let i = add(&mut out, &step.service, &step.span);
                attach_step(&mut out[i], step);
            } else {
                glob_steps.push(step);
            }
        }
    }
    // Observed spans, oldest trace first so `last_*` ends up newest.
    let mut ordered: Vec<&(Vec<Span>, u64)> = traces.iter().collect();
    ordered.sort_by_key(|(spans, _)| spans.first().map_or(0, |s| s.start_ns));
    for (spans, _) in ordered {
        for span in spans {
            let i = add(&mut out, &span.service, &span.name);
            let info = &mut out[i];
            info.seen += 1;
            info.errors += usize::from(span.is_error());
            info.kind = info.kind.or(Some(span.kind));
            info.last_duration_ms = Some(span.duration_ms());
            info.last_trace_id = Some(span.trace_id.clone());
            info.last_span_id = Some(span.span_id.clone());
            info.last_attributes = span.attributes.clone();
            if info.code.is_none()
                && let Some(code) = code_from_attributes(span)
            {
                info.code = Some(code);
                info.located_by = Some("attributes");
            }
        }
    }
    for info in &mut out {
        for step in &glob_steps {
            if glob(&step.service, &info.service) && glob(&step.span, &info.span) {
                attach_step(info, step);
            }
        }
    }
    let services: Vec<String> = {
        let mut seen = BTreeSet::new();
        out.iter()
            .filter(|i| seen.insert(i.service.clone()))
            .map(|i| i.service.clone())
            .collect()
    };
    out.sort_by_key(|info| {
        (
            services
                .iter()
                .position(|s| *s == info.service)
                .unwrap_or(usize::MAX),
            !info.planned,
            info.code.is_none(),
        )
    });
    out
}

fn attach_step(info: &mut SpanInfo, step: &crate::flow::Step) {
    info.planned = true;
    info.kind = info.kind.or(step.kind);
    if info.why.is_none() {
        info.why = step.why.clone();
    }
    if info.code.is_none()
        && let Some(code) = step.code.as_deref().and_then(parse_code)
    {
        info.code = Some(code);
        info.located_by = Some("flow");
    }
    for key in step.attributes.keys() {
        if !info.expected_attributes.contains(key) {
            info.expected_attributes.push(key.clone());
        }
    }
}

/// The 1-based line a function is defined on, by a plain search: the first
/// line that names it followed by `(`, `<` or whitespace and is not a call
/// through `.`. Good enough to land the editor in the right place.
pub fn find_function(content: &str, function: &str) -> Option<u32> {
    let name = function
        .rsplit(['.', ':'])
        .next()
        .unwrap_or(function)
        .trim();
    if name.is_empty() {
        return None;
    }
    let mut fallback = None;
    for (number, line) in content.lines().enumerate() {
        let mut from = 0;
        while let Some(found) = line[from..].find(name) {
            let at = from + found;
            let before = line[..at].chars().next_back();
            let after = line[at + name.len()..].chars().next();
            let word = before.is_none_or(|c| !c.is_alphanumeric() && c != '_')
                && after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
            if word {
                let definition =
                    matches!(after, Some('(' | '<' | ' ' | ':' | '=')) && before != Some('.');
                let keyword = [
                    "def ",
                    "fn ",
                    "func ",
                    "function ",
                    "async ",
                    "public ",
                    "private ",
                    "protected ",
                    "internal ",
                    "static ",
                    "void ",
                    "Task",
                    "class ",
                ]
                .iter()
                .any(|k| line[..at].contains(k));
                if definition && keyword {
                    return Some(number as u32 + 1);
                }
                if definition && fallback.is_none() {
                    fallback = Some(number as u32 + 1);
                }
            }
            from = at + name.len();
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::span;
    use serde_json::json;

    #[test]
    fn code_references() {
        assert_eq!(
            parse_code("src/MtaAiMapper.Core/LocationRepository.cs:ListZonesAsync"),
            Some(CodeRef {
                file: "src/MtaAiMapper.Core/LocationRepository.cs".into(),
                line: None,
                function: Some("ListZonesAsync".into())
            })
        );
        assert_eq!(parse_code("app/orders.py:42").unwrap().line, Some(42));
        assert_eq!(
            parse_code("handler.go:12:Handle").unwrap(),
            CodeRef {
                file: "handler.go".into(),
                line: Some(12),
                function: Some("Handle".into())
            }
        );
        assert_eq!(
            parse_code(r"C:\src\api\Program.cs:Main").unwrap().file,
            r"C:\src\api\Program.cs"
        );
        assert_eq!(
            parse_code("tools/McpE2E/Program.cs (root span)")
                .unwrap()
                .file,
            "tools/McpE2E/Program.cs"
        );
        assert_eq!(
            parse_code("MCP SDK (Experimental.ModelContextProtocol)"),
            None
        );
        assert_eq!(parse_code(""), None);
    }

    #[test]
    fn functions_are_found_at_their_definition() {
        let cs = "class Repo {\n    var x = repo.ListZonesAsync();\n    public async Task<IReadOnlyList<Zone>> ListZonesAsync(CancellationToken ct)\n    {\n";
        assert_eq!(find_function(cs, "ListZonesAsync"), Some(3));
        let py = "import x\n\ndef reserve(order):\n    pass\n";
        assert_eq!(find_function(py, "orders.reserve"), Some(3));
        assert_eq!(find_function(py, "nothing"), None);
    }

    #[test]
    fn the_inventory_joins_catalog_flows_and_traces() {
        let catalog = Catalog::parse(&json!({"spans": [
            {"service": "api", "span": "reserve stock", "file": "src/stock.cs", "function": "Reserve", "why": "one reservation", "attributes": ["stock.requested"]},
            {"service": "api", "span": "never ran", "file": "src/x.cs", "line": 9}
        ]}).to_string()).unwrap();
        let flow = Flow::parse(&json!({"name": "f", "steps": [
            {"service": "api", "span": "validate", "code": "src/validate.cs:Validate", "attributes": {"order.id": "*"}},
            {"service": "db*", "span": "*", "why": "the database is reached"}
        ]}).to_string()).unwrap();
        let mut reserve = span("a", "1", None, "api", "reserve stock", 0, 5_000_000);
        reserve
            .attributes
            .insert("stock.requested".into(), json!(3));
        let validate = span("a", "2", None, "api", "validate", 0, 1);
        let mut auto = span("a", "3", None, "api", "GET /", 0, 1);
        auto.attributes
            .insert("code.filepath".into(), json!("src/Program.cs"));
        auto.attributes.insert("code.lineno".into(), json!(12));
        let query = span("a", "4", None, "db-proxy", "SELECT", 0, 1);
        let traces = vec![(vec![reserve, validate, auto, query], 0)];
        let inventory = inventory(&catalog.spans, &[&flow], &traces);
        let by = |name: &str| inventory.iter().find(|i| i.span == name).unwrap();
        let reserve = by("reserve stock");
        assert!(reserve.planned && reserve.seen == 1);
        assert_eq!(reserve.located_by, Some("catalog"));
        assert_eq!(reserve.last_attributes["stock.requested"], json!(3));
        assert_eq!(by("never ran").seen, 0, "planned but not produced yet");
        assert_eq!(
            by("validate").code.as_ref().unwrap().function.as_deref(),
            Some("Validate")
        );
        assert_eq!(by("validate").located_by, Some("flow"));
        assert_eq!(by("GET /").code.as_ref().unwrap().line, Some(12));
        assert_eq!(by("GET /").located_by, Some("attributes"));
        assert!(!by("GET /").planned);
        assert_eq!(
            by("SELECT").why.as_deref(),
            Some("the database is reached"),
            "glob steps attach to what they match"
        );
        assert_eq!(inventory.last().unwrap().service, "db-proxy");
    }

    #[test]
    fn invalid_catalogs_are_refused() {
        assert!(Catalog::parse(r#"{"spans": [{"service": "", "span": "x"}]}"#).is_err());
        assert!(
            Catalog::parse(r#"{"spans": [{"service": "a", "span": "x", "colour": 1}]}"#).is_err()
        );
    }
}
