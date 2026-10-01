//! The session's spans, grouped into traces (DASHR-TRACE-003).
//!
//! Spans arrive in any order and more than once: an exporter batches, a
//! pull source re-reads an overlapping window, X-Ray completes a segment
//! after first showing it in progress. A span is keyed by trace and span
//! id; a later copy replaces an earlier one. The store keeps the most
//! recently updated traces and drops the oldest past its limits.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use crate::flow::{glob, value_matches};
use crate::model::Span;

pub const MAX_TRACES: usize = 2_000;
pub const MAX_SPANS_PER_TRACE: usize = 10_000;

struct Entry {
    spans: BTreeMap<String, Span>,
    updated_ms: u64,
}

#[derive(Default)]
pub struct TraceStore {
    traces: HashMap<String, Entry>,
    /// Bumped on every change, so a viewer can ask "anything new?".
    version: u64,
    spans_total: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub trace_id: String,
    /// The earliest span without a parent in the trace.
    pub root: String,
    pub root_service: String,
    pub services: Vec<String>,
    pub spans: usize,
    pub errors: usize,
    /// Spans whose parent has not arrived: a broken propagation, or a
    /// parent still on its way.
    pub orphans: usize,
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ms: f64,
    pub updated_ms: u64,
    pub sources: Vec<String>,
}

/// Which traces to list. Every field narrows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    /// Traces that ended at or after this time.
    pub since_ns: Option<u64>,
    /// A glob on any span's service.
    pub service: Option<String>,
    /// A glob on any span's name.
    pub name: Option<String>,
    /// Attribute expectations any one span must meet (see `flow`).
    pub attributes: Vec<(String, Value)>,
    pub errors_only: bool,
    pub limit: Option<usize>,
}

impl TraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn spans_total(&self) -> u64 {
        self.spans_total
    }

    pub fn trace_count(&self) -> usize {
        self.traces.len()
    }

    /// Adds or replaces spans; returns how many were new.
    pub fn insert(&mut self, spans: Vec<Span>, now_ms: u64) -> usize {
        self.insert_with(spans, now_ms, true)
    }

    /// Like [`insert`](Self::insert), but a span already held from another
    /// source is kept: Jaeger hands back the pulled spans dashr sent it,
    /// and the pulled copy (with its source name) is the one to show.
    pub fn insert_keep_sources(&mut self, spans: Vec<Span>, now_ms: u64) -> usize {
        self.insert_with(spans, now_ms, false)
    }

    fn insert_with(&mut self, spans: Vec<Span>, now_ms: u64, replace_other_sources: bool) -> usize {
        let mut added = 0;
        for span in spans {
            if span.trace_id.len() != 32 || span.span_id.len() != 16 {
                continue;
            }
            let entry = self.traces.entry(span.trace_id.clone()).or_insert_with(|| Entry {
                spans: BTreeMap::new(),
                updated_ms: now_ms,
            });
            if !replace_other_sources
                && entry.spans.get(&span.span_id).is_some_and(|held| held.source != span.source)
            {
                continue;
            }
            if !entry.spans.contains_key(&span.span_id) {
                if entry.spans.len() >= MAX_SPANS_PER_TRACE {
                    continue;
                }
                added += 1;
                self.spans_total += 1;
            } else if entry.spans.get(&span.span_id) == Some(&span) {
                continue;
            }
            entry.spans.insert(span.span_id.clone(), span);
            entry.updated_ms = now_ms;
            self.version += 1;
        }
        self.evict();
        added
    }

    fn evict(&mut self) {
        if self.traces.len() <= MAX_TRACES {
            return;
        }
        let mut by_age: Vec<(u64, String)> = self.traces.iter().map(|(id, e)| (e.updated_ms, id.clone())).collect();
        by_age.sort();
        for (_, id) in by_age.into_iter().take(self.traces.len() - MAX_TRACES) {
            self.traces.remove(&id);
        }
    }

    /// A trace's spans by start time.
    pub fn trace(&self, trace_id: &str) -> Option<Vec<Span>> {
        let entry = self.traces.get(trace_id)?;
        let mut spans: Vec<Span> = entry.spans.values().cloned().collect();
        sort(&mut spans);
        Some(spans)
    }

    pub fn updated_ms(&self, trace_id: &str) -> Option<u64> {
        self.traces.get(trace_id).map(|e| e.updated_ms)
    }

    /// Every trace as `(spans by start time, last update)`, newest first.
    pub fn all(&self) -> Vec<(Vec<Span>, u64)> {
        let mut out: Vec<(Vec<Span>, u64)> = self
            .traces
            .values()
            .map(|entry| {
                let mut spans: Vec<Span> = entry.spans.values().cloned().collect();
                sort(&mut spans);
                (spans, entry.updated_ms)
            })
            .collect();
        out.sort_by(|a, b| start(&b.0).cmp(&start(&a.0)));
        out
    }

    /// Summaries of the traces matching `filter`, newest first.
    pub fn summaries(&self, filter: &Filter) -> Vec<Summary> {
        let mut out: Vec<Summary> = self
            .traces
            .values()
            .filter(|entry| matches(entry, filter))
            .map(|entry| summarize(entry.spans.values(), entry.updated_ms))
            .collect();
        out.sort_by(|a, b| b.start_ns.cmp(&a.start_ns).then_with(|| a.trace_id.cmp(&b.trace_id)));
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        out
    }

    pub fn summary(&self, trace_id: &str) -> Option<Summary> {
        let entry = self.traces.get(trace_id)?;
        Some(summarize(entry.spans.values(), entry.updated_ms))
    }

    pub fn clear(&mut self) {
        self.traces.clear();
        self.version += 1;
    }
}

fn start(spans: &[Span]) -> u64 {
    spans.first().map_or(0, |s| s.start_ns)
}

pub fn sort(spans: &mut [Span]) {
    spans.sort_by(|a, b| {
        a.start_ns
            .cmp(&b.start_ns)
            .then_with(|| a.parent_id.is_some().cmp(&b.parent_id.is_some()))
            .then_with(|| a.span_id.cmp(&b.span_id))
    });
}

fn matches(entry: &Entry, filter: &Filter) -> bool {
    let spans = || entry.spans.values();
    if let Some(since) = filter.since_ns
        && spans().map(|s| s.end_ns).max().unwrap_or(0) < since
    {
        return false;
    }
    if let Some(service) = &filter.service
        && !spans().any(|s| glob(service, &s.service))
    {
        return false;
    }
    if let Some(name) = &filter.name
        && !spans().any(|s| glob(name, &s.name))
    {
        return false;
    }
    if !filter.attributes.is_empty()
        && !spans().any(|s| filter.attributes.iter().all(|(k, v)| value_matches(v, s.attribute(k))))
    {
        return false;
    }
    if filter.errors_only && !spans().any(Span::is_error) {
        return false;
    }
    true
}

pub fn summarize<'a>(spans: impl Iterator<Item = &'a Span> + Clone, updated_ms: u64) -> Summary {
    let ids: BTreeSet<&str> = spans.clone().map(|s| s.span_id.as_str()).collect();
    let mut ordered: Vec<&Span> = spans.collect();
    ordered.sort_by_key(|s| (s.start_ns, s.parent_id.is_some()));
    let is_root = |s: &&Span| s.parent_id.as_deref().is_none_or(|p| !ids.contains(p));
    let root = ordered.iter().find(|s| s.parent_id.is_none()).or_else(|| ordered.iter().find(|s| is_root(s)));
    let mut services = Vec::new();
    let mut sources = BTreeSet::new();
    for span in &ordered {
        if !services.contains(&span.service) {
            services.push(span.service.clone());
        }
        sources.insert(span.source.clone());
    }
    let start_ns = ordered.iter().map(|s| s.start_ns).min().unwrap_or(0);
    let end_ns = ordered.iter().map(|s| s.end_ns).max().unwrap_or(0);
    Summary {
        trace_id: ordered.first().map(|s| s.trace_id.clone()).unwrap_or_default(),
        root: root.map(|s| s.name.clone()).unwrap_or_default(),
        root_service: root.map(|s| s.service.clone()).unwrap_or_default(),
        services,
        spans: ordered.len(),
        errors: ordered.iter().filter(|s| s.is_error()).count(),
        orphans: ordered.iter().filter(|s| s.parent_id.as_deref().is_some_and(|p| !ids.contains(p))).count(),
        start_ns,
        end_ns,
        duration_ms: end_ns.saturating_sub(start_ns) as f64 / 1e6,
        updated_ms,
        sources: sources.into_iter().collect(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{SpanKind, Status};
    use serde_json::json;

    pub fn span(trace: &str, id: &str, parent: Option<&str>, service: &str, name: &str, start: u64, end: u64) -> Span {
        Span {
            trace_id: format!("{trace:0>32}"),
            span_id: format!("{id:0>16}"),
            parent_id: parent.map(|p| format!("{p:0>16}")),
            name: name.into(),
            service: service.into(),
            kind: SpanKind::Internal,
            start_ns: start,
            end_ns: end,
            status: Status::Unset,
            status_message: None,
            attributes: BTreeMap::new(),
            resource: BTreeMap::new(),
            events: vec![],
            links: vec![],
            source: "otlp".into(),
        }
    }

    #[test]
    fn spans_group_into_traces_and_later_copies_replace_earlier_ones() {
        let mut store = TraceStore::new();
        assert_eq!(store.insert(vec![span("a", "1", None, "api", "POST /orders", 10, 20)], 1), 1);
        let mut later = span("a", "2", Some("1"), "stock", "reserve", 12, 15);
        later.status = Status::Error;
        assert_eq!(store.insert(vec![later.clone(), span("b", "9", Some("8"), "x", "orphan", 5, 6)], 2), 2);
        let v = store.version();
        assert_eq!(store.insert(vec![later.clone()], 3), 0, "a duplicate changes nothing");
        assert_eq!(store.version(), v);
        later.end_ns = 18;
        assert_eq!(store.insert(vec![later], 4), 0);
        assert!(store.version() > v, "a changed copy replaces the old one");
        let summaries = store.summaries(&Filter::default());
        assert_eq!(summaries.len(), 2);
        let a = summaries.iter().find(|s| s.root == "POST /orders").unwrap();
        assert_eq!((a.spans, a.errors, a.orphans, a.services.clone()), (2, 1, 0, vec!["api".to_owned(), "stock".to_owned()]));
        let b = summaries.iter().find(|s| s.root == "orphan").unwrap();
        assert_eq!(b.orphans, 1);
        assert_eq!(store.trace(&a.trace_id).unwrap()[1].end_ns, 18);
    }

    #[test]
    fn jaeger_echoes_do_not_replace_pulled_spans() {
        let mut store = TraceStore::new();
        let mut pulled = span("a", "1", None, "sfn", "run", 1, 2);
        pulled.source = "xray".into();
        store.insert(vec![pulled], 1);
        let echo = span("a", "1", None, "sfn", "run", 1, 3);
        store.insert_keep_sources(vec![echo], 2);
        assert_eq!(store.trace(&format!("{:0>32}", "a")).unwrap()[0].source, "xray");
    }

    #[test]
    fn filters_narrow_the_list() {
        let mut store = TraceStore::new();
        let mut tagged = span("a", "1", None, "api", "POST /orders", 10, 20);
        tagged.attributes.insert("test.run".into(), json!("r-1"));
        store.insert(vec![tagged, span("b", "2", None, "web", "GET /", 30, 40)], 1);
        let only = |f: Filter| store.summaries(&f).into_iter().map(|s| s.root).collect::<Vec<_>>();
        assert_eq!(only(Filter { service: Some("ap*".into()), ..Filter::default() }), ["POST /orders"]);
        assert_eq!(only(Filter { attributes: vec![("test.run".into(), json!("r-1"))], ..Filter::default() }), ["POST /orders"]);
        assert_eq!(only(Filter { since_ns: Some(25), ..Filter::default() }), ["GET /"]);
        assert_eq!(only(Filter { limit: Some(1), ..Filter::default() }), ["GET /"]);
        assert!(only(Filter { errors_only: true, ..Filter::default() }).is_empty());
    }
}
