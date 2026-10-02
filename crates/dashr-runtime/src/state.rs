//! A running session's state and everything that changes it: the trace
//! store fed by Jaeger and by pull sources, the flows with their
//! verdicts, the span catalog and code root behind the Spans tab, and the
//! sources with their health.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use dashr_core::catalog::{self, Catalog, CatalogEntry};
use dashr_core::flow::{self, Candidate, Flow, Verdict, VerdictStatus};
use dashr_core::ingest::{self, Format};
use dashr_core::privacy::{Masker, Pseudonyms};
use dashr_core::{Config, Span, TraceStore, sequence};

use crate::command;
use crate::jaeger::Jaeger;
use crate::session::now_ms;

/// Traces that started this long before a flow was armed still count:
/// clocks of other machines (an AWS region) drift a little.
pub const ARM_SLACK_MS: u64 = 5_000;
pub const POLL_EVERY: Duration = Duration::from_millis(1_500);
pub const MIN_SOURCE_EVERY_SECS: u64 = 5;
pub const TRIAL_TIMEOUT: Duration = Duration::from_secs(120);

/// A file the human saved in the viewer's editor, for the agent to see.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edit {
    pub file: String,
    pub lines_changed: usize,
    pub at_ms: u64,
}

pub struct FlowEntry {
    pub flow: Flow,
    pub armed_ms: u64,
    /// The last decided status, for notifying once per change.
    pub announced: Option<(VerdictStatus, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub name: String,
    /// The command and its arguments; run through `cmd /c` on Windows.
    pub command: Vec<String>,
    #[serde(default = "default_every")]
    pub every_secs: u64,
    #[serde(default)]
    pub format: Format,
    /// How far back the first run reads, in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lookback_minutes: Option<u64>,
    /// Keep the source even when its trial run fails (a CLI not logged in
    /// yet, the human logs in next).
    #[serde(default)]
    pub keep_on_error: bool,
}

fn default_every() -> u64 {
    15
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SourceHealth {
    pub runs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub spans_last: usize,
    pub spans_total: usize,
    pub traces_last: usize,
    /// Where the next run starts reading.
    #[serde(skip)]
    pub since_ms: u64,
}

pub struct SourceEntry {
    pub spec: SourceSpec,
    pub health: SourceHealth,
    pub removed: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub session_id: String,
    pub otlp_http: String,
    pub otlp_grpc: String,
    pub jaeger_ui: String,
    pub started_ms: u64,
}

#[derive(Default)]
pub struct State {
    pub store: TraceStore,
    pub flows: BTreeMap<String, FlowEntry>,
    pub sources: BTreeMap<String, SourceEntry>,
    pub catalog: Vec<CatalogEntry>,
    /// The repository the Spans tab's editor works in (canonical).
    pub code_root: Option<PathBuf>,
    /// Whether a catalog set the root (`flow set` never replaces it then).
    root_from_catalog: bool,
    pub edits: Vec<Edit>,
    meta_version: u64,
    pub poll_error: Option<String>,
}

impl State {
    pub fn version(&self) -> u64 {
        self.store.version() + self.meta_version
    }

    pub fn touch(&mut self) {
        self.meta_version += 1;
    }
}

pub struct Shared {
    state: Mutex<State>,
    pub masker: Masker,
    pub jaeger: Jaeger,
    pub info: Info,
    pub config: Config,
    pub stop: Arc<AtomicBool>,
}

/// What a source produced in one run, without values (DASHR-SOURCE-002).
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub spans: usize,
    pub traces: usize,
    pub services: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl Shared {
    pub fn new(info: Info, config: Config, jaeger: Jaeger, stop: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            masker: Masker::new(&config.masking),
            jaeger,
            info,
            config,
            stop,
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    // ------------------------------------------------------------ flows

    /// Adds or replaces a flow. A changed flow is armed from now;
    /// resending the same flow keeps its arming. `cwd` is where the agent
    /// ran `dashr flow set`: the code root, until a catalog names one.
    pub fn set_flow(&self, flow: Flow, cwd: Option<&str>) -> Value {
        let now = now_ms();
        let mut state = self.lock();
        let unchanged = state.flows.get(&flow.name).is_some_and(|e| e.flow == flow);
        if !unchanged {
            state.flows.insert(
                flow.name.clone(),
                FlowEntry {
                    flow: flow.clone(),
                    armed_ms: now,
                    announced: None,
                },
            );
            state.touch();
        }
        if !state.root_from_catalog
            && let Some(root) = cwd.and_then(|c| canonical_dir(Path::new(c)))
            && state.code_root.as_ref() != Some(&root)
        {
            state.code_root = Some(root);
            state.touch();
        }
        json!({"flow": flow.name, "changed": !unchanged, "armed_ms": state.flows[&flow.name].armed_ms})
    }

    /// Counts only traces from now on.
    pub fn arm(&self, name: &str) -> Result<(), String> {
        let mut state = self.lock();
        let entry = state
            .flows
            .get_mut(name)
            .ok_or_else(|| format!("no flow {name:?}"))?;
        entry.armed_ms = now_ms();
        entry.announced = None;
        state.touch();
        Ok(())
    }

    pub fn remove_flow(&self, name: &str) -> bool {
        let mut state = self.lock();
        let removed = state.flows.remove(name).is_some();
        state.touch();
        removed
    }

    fn verdict_locked(&self, state: &State, entry: &FlowEntry, now: u64) -> Verdict {
        let since_ns = entry.armed_ms.saturating_sub(ARM_SLACK_MS) * 1_000_000;
        let traces: Vec<(Vec<Span>, u64)> = state
            .store
            .all()
            .into_iter()
            .filter(|(spans, updated)| {
                *updated >= entry.armed_ms && spans.first().is_some_and(|s| s.start_ns >= since_ns)
            })
            .collect();
        let candidates: Vec<Candidate> = traces
            .iter()
            .map(|(spans, updated_ms)| Candidate {
                spans,
                updated_ms: *updated_ms,
            })
            .collect();
        flow::evaluate(&entry.flow, &candidates, now, &self.masker)
    }

    pub fn verdict(&self, name: &str) -> Option<Verdict> {
        let state = self.lock();
        let entry = state.flows.get(name)?;
        Some(self.verdict_locked(&state, entry, now_ms()))
    }

    /// A flow with its verdict; `brief` leaves out the steps.
    pub fn flow_view(&self, name: &str, brief: bool) -> Option<Value> {
        let state = self.lock();
        let entry = state.flows.get(name)?;
        let verdict = self.verdict_locked(&state, entry, now_ms());
        Some(if brief {
            json!({"name": name, "armed_ms": entry.armed_ms,
                   "status": verdict.status, "settled": verdict.settled, "summary": verdict.summary})
        } else {
            json!({"flow": entry.flow, "armed_ms": entry.armed_ms, "verdict": verdict})
        })
    }

    pub fn flow_names(&self) -> Vec<String> {
        self.lock().flows.keys().cloned().collect()
    }

    /// Flows whose decided status changed since last asked: `(name,
    /// status, summary)`, for notifying the human once.
    pub fn changed_verdicts(&self) -> Vec<(String, VerdictStatus, String)> {
        let now = now_ms();
        let mut state = self.lock();
        let names: Vec<String> = state.flows.keys().cloned().collect();
        let mut out = Vec::new();
        for name in names {
            let verdict = {
                let entry = &state.flows[&name];
                self.verdict_locked(&state, entry, now)
            };
            let decided = matches!(verdict.status, VerdictStatus::Pass | VerdictStatus::Fail)
                && (verdict.settled || verdict.status == VerdictStatus::Fail);
            if !decided {
                continue;
            }
            let key = (verdict.status, verdict.trace_id.clone());
            let entry = state.flows.get_mut(&name).expect("present");
            if entry.announced.as_ref() != Some(&key) {
                entry.announced = Some(key);
                out.push((name, verdict.status, verdict.summary));
            }
        }
        out
    }

    // ------------------------------------------------- spans and code

    /// Replaces the span catalog. Its `root` (relative to `cwd`), else
    /// `cwd`, becomes the code root.
    pub fn set_catalog(&self, catalog: Catalog, cwd: Option<&str>) -> Result<Value, String> {
        let base = cwd.map(PathBuf::from);
        let root = match (&catalog.root, &base) {
            (Some(root), Some(base)) => Some(base.join(root)),
            (Some(root), None) => Some(PathBuf::from(root)),
            (None, base) => base.clone(),
        };
        let root = match root {
            Some(root) => Some(
                canonical_dir(&root)
                    .ok_or_else(|| format!("code root {}: not a directory", root.display()))?,
            ),
            None => None,
        };
        let mut state = self.lock();
        let count = catalog.spans.len();
        state.catalog = catalog.spans;
        if let Some(root) = root {
            state.code_root = Some(root);
            state.root_from_catalog = true;
        }
        state.touch();
        let root = state.code_root.as_ref().map(|r| r.display().to_string());
        drop(state);
        let located = self
            .inventory()
            .iter()
            .filter(|i| i.planned && i.code.is_some())
            .count();
        Ok(json!({"spans": count, "located": located, "code_root": root}))
    }

    fn inventory(&self) -> Vec<catalog::SpanInfo> {
        let state = self.lock();
        let traces = state.store.all();
        let flows: Vec<&Flow> = state.flows.values().map(|e| &e.flow).collect();
        catalog::inventory(&state.catalog, &flows, &traces)
    }

    /// Every span the code has, raw: the human's view.
    pub fn spans_for_viewer(&self) -> Value {
        let root = self.lock().code_root.clone();
        let mut spans = serde_json::to_value(self.inventory()).unwrap_or_default();
        // Land the editor on the function's line when only its name is known.
        if let (Some(root), Some(items)) = (root.as_deref(), spans.as_array_mut()) {
            let mut cache: BTreeMap<String, Option<String>> = BTreeMap::new();
            for item in items {
                let code = &mut item["code"];
                let (Some(file), None, Some(function)) = (
                    code.get("file").and_then(Value::as_str).map(str::to_owned),
                    code.get("line").and_then(Value::as_u64),
                    code.get("function")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                ) else {
                    continue;
                };
                let content = cache.entry(file.clone()).or_insert_with(|| {
                    crate::code::read(Some(root), &file).ok().map(|f| f.content)
                });
                if let Some(line) = content
                    .as_deref()
                    .and_then(|c| catalog::find_function(c, &function))
                {
                    code["line"] = json!(line);
                }
            }
        }
        spans
    }

    /// Every span the code has, masked: span names and attribute values
    /// may carry data (DASHR-AGENT-004).
    pub fn spans_for_agent(&self) -> Value {
        let mut p = Pseudonyms::default();
        Value::Array(
            self.inventory()
                .into_iter()
                .map(|info| {
                    json!({
                        "service": info.service,
                        "span": self.masker.text(&info.span, &mut p),
                        "kind": info.kind,
                        "code": info.code,
                        "located_by": info.located_by,
                        "planned": info.planned,
                        "seen": info.seen,
                        "errors": info.errors,
                        "last_duration_ms": info.last_duration_ms,
                        "last_trace_id": info.last_trace_id,
                        "attributes": self.masker.attributes(&info.last_attributes, &mut p),
                        "expected_attributes": info.expected_attributes,
                    })
                })
                .collect(),
        )
    }

    pub fn code_root(&self) -> Option<PathBuf> {
        self.lock().code_root.clone()
    }

    pub fn record_edit(&self, file: &str, lines_changed: usize) {
        let mut state = self.lock();
        let at_ms = now_ms();
        state.edits.retain(|e| e.file != file);
        state.edits.push(Edit {
            file: file.to_owned(),
            lines_changed,
            at_ms,
        });
        state.touch();
    }

    // ---------------------------------------------------------- sources

    fn fetch(&self, spec: &SourceSpec, since_ms: u64, until_ms: u64) -> Result<Vec<Span>, String> {
        let env = vec![
            ("DASHR_SOURCE".to_owned(), spec.name.clone()),
            ("DASHR_SINCE".to_owned(), (since_ms / 1000).to_string()),
            ("DASHR_SINCE_MS".to_owned(), since_ms.to_string()),
            ("DASHR_SINCE_ISO".to_owned(), iso(since_ms)),
            (
                "DASHR_UNTIL".to_owned(),
                until_ms.div_ceil(1000).to_string(),
            ),
            ("DASHR_UNTIL_MS".to_owned(), until_ms.to_string()),
            ("DASHR_UNTIL_ISO".to_owned(), iso(until_ms)),
            ("DASHR_OTLP_HTTP".to_owned(), self.info.otlp_http.clone()),
        ];
        let timeout = Duration::from_secs((spec.every_secs * 4).clamp(60, 600));
        let output = command::run(&command::shell_argv(&spec.command), &env, timeout)
            .map_err(|error| self.masker.text(&error, &mut Pseudonyms::default()))?;
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Vec::new());
        }
        ingest::parse(&output.stdout, spec.format, &spec.name)
    }

    /// Sends spans to Jaeger and keeps them; returns what arrived.
    pub fn accept(&self, spans: Vec<Span>) -> RunReport {
        let traces: BTreeSet<String> = spans.iter().map(|s| s.trace_id.clone()).collect();
        let mut services: Vec<String> = Vec::new();
        for span in &spans {
            if !services.contains(&span.service) {
                services.push(span.service.clone());
            }
        }
        let warning = self
            .jaeger
            .send(&spans)
            .err()
            .map(|e| format!("kept, but not sent to Jaeger: {e}"));
        let count = spans.len();
        self.lock().store.insert(spans, now_ms());
        RunReport {
            spans: count,
            traces: traces.len(),
            services,
            error: None,
            warning,
        }
    }

    /// One-shot import of a file or a command's output.
    pub fn ingest(&self, bytes: &[u8], format: Format, source: &str) -> Result<RunReport, String> {
        let spans = ingest::parse(bytes, format, source)?;
        Ok(self.accept(spans))
    }

    /// Tries a source once, then keeps running it every `every_secs`.
    pub fn add_source(self: &Arc<Self>, mut spec: SourceSpec) -> Result<RunReport, String> {
        let name = spec.name.trim().to_owned();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err("source name: letters, digits, '-', '_' or '.'".into());
        }
        if spec.command.is_empty() {
            return Err("a source needs a command".into());
        }
        spec.name = name.clone();
        spec.every_secs = spec.every_secs.max(MIN_SOURCE_EVERY_SECS);
        let now = now_ms();
        let lookback = spec
            .lookback_minutes
            .unwrap_or(self.config.sources.first_lookback_minutes);
        let since = now.saturating_sub(lookback * 60_000);
        let trial = self.fetch(&spec, since, now);
        let (report, ok) = match trial {
            Ok(spans) => {
                let mut report = self.accept(spans);
                if report.spans == 0 && report.warning.is_none() {
                    report.warning = Some("no spans yet: fine if nothing ran in the window; check the command otherwise".into());
                }
                (report, true)
            }
            Err(error) => (
                RunReport {
                    spans: 0,
                    traces: 0,
                    services: Vec::new(),
                    error: Some(error.clone()),
                    warning: None,
                },
                false,
            ),
        };
        if !ok && !spec.keep_on_error {
            return Err(format!(
                "the source's trial run failed: {}",
                report.error.clone().unwrap_or_default()
            ));
        }
        let removed = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.lock();
            if let Some(previous) = state.sources.remove(&name) {
                previous.removed.store(true, Ordering::SeqCst);
            }
            state.sources.insert(
                name.clone(),
                SourceEntry {
                    spec: spec.clone(),
                    health: SourceHealth {
                        runs: 1,
                        last_run_ms: Some(now),
                        last_ok_ms: ok.then_some(now),
                        last_error: report.error.clone(),
                        spans_last: report.spans,
                        spans_total: report.spans,
                        traces_last: report.traces,
                        since_ms: if ok { now } else { since },
                    },
                    removed: Arc::clone(&removed),
                },
            );
            state.touch();
        }
        let shared = Arc::clone(self);
        std::thread::spawn(move || shared.run_source(&name, &removed));
        Ok(report)
    }

    fn run_source(&self, name: &str, removed: &AtomicBool) {
        loop {
            let every = match self.lock().sources.get(name) {
                Some(entry) => Duration::from_secs(entry.spec.every_secs),
                None => return,
            };
            if !sleep_unless(every, &[&self.stop, removed]) {
                return;
            }
            let (spec, since) = match self.lock().sources.get(name) {
                Some(entry) => (entry.spec.clone(), entry.health.since_ms),
                None => return,
            };
            let started = now_ms();
            let from = since.saturating_sub(self.config.sources.overlap_secs * 1000);
            let result = self.fetch(&spec, from, started);
            let report = result.map(|spans| self.accept(spans));
            if removed.load(Ordering::SeqCst) {
                return;
            }
            let mut state = self.lock();
            let Some(entry) = state.sources.get_mut(name) else {
                return;
            };
            let health = &mut entry.health;
            health.runs += 1;
            health.last_run_ms = Some(started);
            match report {
                Ok(report) => {
                    health.last_ok_ms = Some(started);
                    health.last_error = report.warning;
                    health.spans_last = report.spans;
                    health.spans_total += report.spans;
                    health.traces_last = report.traces;
                    health.since_ms = started;
                }
                Err(error) => health.last_error = Some(error),
            }
            state.touch();
        }
    }

    pub fn remove_source(&self, name: &str) -> bool {
        let mut state = self.lock();
        let removed = state.sources.remove(name);
        if let Some(entry) = &removed {
            entry.removed.store(true, Ordering::SeqCst);
        }
        state.touch();
        removed.is_some()
    }

    pub fn sources_view(&self) -> Value {
        let state = self.lock();
        Value::Array(
            state
                .sources
                .values()
                .map(|entry| json!({"spec": entry.spec, "health": entry.health}))
                .collect(),
        )
    }

    // ----------------------------------------------------------- jaeger

    /// Reads recent traces from Jaeger into the store until the session
    /// stops: spans the SDKs exported there over OTLP.
    pub fn poll_jaeger(&self) {
        let lookback_us = self.config.jaeger.lookback_minutes * 60 * 1_000_000;
        while !self.stopped() {
            let now_us = now_ms() * 1000;
            let result = self.jaeger.services().and_then(|services| {
                let mut spans = Vec::new();
                // Jaeger's own spans, should its self-tracing be on.
                for service in services
                    .into_iter()
                    .filter(|s| s != "jaeger" && s != "jaeger-all-in-one")
                {
                    spans.extend(self.jaeger.traces(
                        &service,
                        now_us.saturating_sub(lookback_us),
                        now_us + 60_000_000,
                        500,
                    )?);
                }
                Ok(spans)
            });
            {
                let mut state = self.lock();
                match result {
                    Ok(spans) => {
                        state.store.insert_keep_sources(spans, now_ms());
                        if state.poll_error.take().is_some() {
                            state.touch();
                        }
                    }
                    Err(error) => {
                        if state.poll_error.as_deref() != Some(&error) {
                            state.poll_error = Some(error);
                            state.touch();
                        }
                    }
                }
            }
            sleep_unless(POLL_EVERY, &[&self.stop]);
        }
    }

    // ------------------------------------------------------------ views

    /// A trace for the agent: masked spans and the sequence as text.
    pub fn trace_for_agent(&self, trace_id: &str) -> Option<Value> {
        let state = self.lock();
        let spans = state.store.trace(trace_id)?;
        let summary = state.store.summary(trace_id)?;
        drop(state);
        let mut p = Pseudonyms::default();
        let masked: Vec<Value> = spans
            .iter()
            .map(|s| {
                json!({
                    "span_id": s.span_id, "parent_id": s.parent_id,
                    "service": s.service, "name": self.masker.text(&s.name, &mut p),
                    "kind": s.kind, "start_offset_ms": s.start_ns.saturating_sub(summary.start_ns) as f64 / 1e6,
                    "duration_ms": s.duration_ms(), "status": s.status,
                    "status_message": s.status_message.as_deref().map(|m| self.masker.text(m, &mut p)),
                    "attributes": self.masker.attributes(&s.attributes, &mut p),
                    "events": s.events.iter().map(|e| json!({"name": self.masker.text(&e.name, &mut p), "attributes": self.masker.attributes(&e.attributes, &mut p)})).collect::<Vec<_>>(),
                    "source": s.source,
                })
            })
            .collect();
        Some(json!({
            "trace_id": trace_id,
            "summary": self.mask_summary(&summary),
            "sequence": sequence::text(&spans, &self.masker),
            "spans": masked,
        }))
    }

    pub fn mask_summary(&self, summary: &dashr_core::store::Summary) -> Value {
        let mut value = serde_json::to_value(summary).unwrap_or_default();
        value["root"] = Value::String(self.masker.text(&summary.root, &mut Pseudonyms::default()));
        value
    }

    /// Everything the browser shows, raw: the human's own data.
    pub fn viewer_state(&self) -> Value {
        let names = self.flow_names();
        let flows: Vec<Value> = names
            .iter()
            .filter_map(|n| self.flow_view(n, false))
            .collect();
        let state = self.lock();
        let traces = state.store.summaries(&dashr_core::store::Filter {
            limit: Some(300),
            ..Default::default()
        });
        json!({
            "version": state.version(),
            "info": self.info,
            "traces": traces,
            "spans_total": state.store.spans_total(),
            "flows": flows,
            "sources": state.sources.values().map(|e| json!({"spec": e.spec, "health": e.health})).collect::<Vec<_>>(),
            "poll_error": state.poll_error,
            "code_root": state.code_root,
            "catalog_spans": state.catalog.len(),
            "edits": state.edits,
        })
    }

    pub fn viewer_trace(&self, trace_id: &str) -> Option<Value> {
        let state = self.lock();
        let spans = state.store.trace(trace_id)?;
        let summary = state.store.summary(trace_id)?;
        drop(state);
        Some(json!({"summary": summary, "spans": spans, "sequence": sequence::build(&spans)}))
    }

    /// A report to attach to a pull request: a flow's verdict with the
    /// trace it judged, or a trace alone. `masked` is always true for the
    /// agent (DASHR-EXPORT-001).
    pub fn report(
        &self,
        flow: Option<&str>,
        trace: Option<&str>,
        masked: bool,
    ) -> Result<dashr_core::report::Report, String> {
        use dashr_core::report::{Mask, Report, mermaid};
        let (flow_entry, verdict) = {
            let state = self.lock();
            let name = match flow {
                Some(name) => Some(name.to_owned()),
                // Without a flow or a trace: the first flow, if any.
                None if trace.is_none() => state.flows.keys().next().cloned(),
                None => None,
            };
            match name {
                Some(name) => {
                    let entry = state
                        .flows
                        .get(&name)
                        .ok_or_else(|| format!("no flow {name:?}"))?;
                    let verdict = self.verdict_locked(&state, entry, now_ms());
                    (Some(entry.flow.clone()), Some(verdict))
                }
                None => (None, None),
            }
        };
        let trace_id = trace
            .map(str::to_owned)
            .or_else(|| verdict.as_ref().and_then(|v| v.trace_id.clone()))
            .or_else(|| {
                let state = self.lock();
                let newest = state.store.summaries(&dashr_core::store::Filter {
                    limit: Some(1),
                    ..Default::default()
                });
                newest.first().map(|s| s.trace_id.clone())
            });
        let state = self.lock();
        let spans = trace_id.as_deref().and_then(|id| state.store.trace(id));
        let summary = trace_id.as_deref().and_then(|id| state.store.summary(id));
        drop(state);
        if trace.is_some() && spans.is_none() {
            return Err(format!("no trace {}", trace.unwrap_or_default()));
        }
        if flow_entry.is_none() && spans.is_none() {
            return Err("nothing to export yet: no flow and no trace".into());
        }
        let masker = masked.then_some(&self.masker);
        let mut mask = Mask::new(masker);
        let mut summary = summary;
        if let Some(summary) = summary.as_mut() {
            summary.root = mask.text(&summary.root);
        }
        let title = match (&flow_entry, &summary) {
            (Some(flow), _) => flow.name.clone(),
            (None, Some(summary)) => summary.root.clone(),
            _ => "trace".into(),
        };
        let raw_masker;
        let sequence_masker = match masker {
            Some(masker) => masker,
            None => {
                let mut config = self.config.masking.clone();
                config.enabled = false;
                raw_masker = Masker::new(&config);
                &raw_masker
            }
        };
        let spans_json = spans.as_ref().map(|spans| {
            if masked {
                trace_id
                    .as_deref()
                    .and_then(|id| self.trace_for_agent(id))
                    .map(|t| t["spans"].clone())
                    .unwrap_or_default()
            } else {
                serde_json::to_value(spans).unwrap_or_default()
            }
        });
        Ok(Report {
            title,
            generated: iso(now_ms()),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            masked,
            mermaid: spans.as_deref().map(|s| mermaid(s, &mut mask)),
            sequence: spans.as_deref().map(|s| sequence::text(s, sequence_masker)),
            summary,
            flow: flow_entry,
            verdict,
            spans: spans_json,
        })
    }

    pub fn status(&self) -> Value {
        let names = self.flow_names();
        let flows: Vec<Value> = names
            .iter()
            .filter_map(|n| self.flow_view(n, true))
            .collect();
        let sources = self.sources_view();
        let state = self.lock();
        json!({
            "session": self.info.session_id,
            "otlp": {"http": self.info.otlp_http, "grpc": self.info.otlp_grpc},
            "jaeger_ui": self.info.jaeger_ui,
            "traces": state.store.trace_count(),
            "spans": state.store.spans_total(),
            "flows": flows,
            "sources": sources,
            "jaeger_error": state.poll_error,
            "code_root": state.code_root,
            "catalog_spans": state.catalog.len(),
            "human_edits": state.edits,
        })
    }
}

/// A directory, canonical; `None` when it is not one.
fn canonical_dir(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok().filter(|p| p.is_dir())
}

/// Sleeps `duration` unless a flag is raised first; `false` when one was.
pub fn sleep_unless(duration: Duration, flags: &[&AtomicBool]) -> bool {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if flags.iter().any(|f| f.load(Ordering::SeqCst)) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !flags.iter().any(|f| f.load(Ordering::SeqCst))
}

/// Unix milliseconds as RFC 3339 UTC (`2026-09-28T08:00:00Z`).
pub fn iso(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_times() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(1_790_582_953_928), "2026-09-28T08:09:13Z");
        assert_eq!(
            dashr_core::ingest::rfc3339_ns(&iso(1_790_582_953_000)),
            Some(1_790_582_953_000_000_000)
        );
    }
}
