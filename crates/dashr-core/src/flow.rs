//! Flows: the trace a feature is expected to produce (DASHR-FLOW-001).
//!
//! The agent writes a flow from the code it just instrumented: the steps in
//! order — which service, which span, which attributes with which values,
//! and where in the code each span is made (the viewer opens that code,
//! DASHR-VIEW-005). When the feature runs, every
//! trace that arrived since the flow was armed is matched against it, and
//! the best one becomes the verdict (DASHR-FLOW-003): each step ok,
//! missing, out of order, wrong, failed or slow, with the actual values
//! masked. Expected values are compared against raw values here, inside
//! dashr; the agent never needs to see them.

use std::collections::BTreeSet;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Span, SpanKind};
use crate::privacy::{Masker, Pseudonyms};

/// `*` matches any run of characters, `?` one; everything else literally.
pub fn glob(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether an attribute's actual value meets an expectation:
///
/// * `"*"` — present, any value;
/// * `"!"` — absent (a value that must not be recorded, such as a card
///   number);
/// * `"re:<regex>"` — a string (or number) matching the regular expression;
/// * anything else — equal, comparing `3` and `"3"` as equal.
pub fn value_matches(expected: &Value, actual: Option<&Value>) -> bool {
    match (expected, actual) {
        (Value::String(e), actual) if e == "!" => actual.is_none_or(Value::is_null),
        (_, None | Some(Value::Null)) => false,
        (Value::String(e), Some(_)) if e == "*" => true,
        (Value::String(e), Some(actual)) if e.starts_with("re:") => {
            Regex::new(&e[3..]).is_ok_and(|re| re.is_match(&plain(actual)))
        }
        (expected, Some(actual)) => expected == actual || plain(expected) == plain(actual),
    }
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    /// Steps start in the order listed.
    #[default]
    Sequence,
    /// Steps may appear in any order (parallel work).
    Any,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Count {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<usize>,
}

/// One expected span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// A short id for the step; defaults to its position.
    #[serde(default)]
    pub id: String,
    /// The service (glob).
    pub service: String,
    /// The span name (glob).
    pub span: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<SpanKind>,
    /// Expected attributes: key to expectation (see [`value_matches`]).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, Value>,
    /// The step is expected to fail (an error path under test).
    #[serde(default)]
    pub error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<f64>,
    /// A missing optional step is not a failure.
    #[serde(default)]
    pub optional: bool,
    /// How many spans may match: exactly one call, no retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<Count>,
    /// Where the span is created (`path/file.py:function`): the viewer opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Why the span is there, shown beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// Spans that must not appear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forbid {
    #[serde(default = "star")]
    pub service: String,
    #[serde(default = "star")]
    pub span: String,
}

fn star() -> String {
    "*".into()
}

fn yes() -> bool {
    true
}

fn default_settle() -> u64 {
    10
}

/// Which traces the flow is about: by attributes any span carries (a test
/// run id), and/or by the root span's name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
    #[serde(default)]
    pub order: Order,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbid: Vec<Forbid>,
    /// Any error span not expected by a step fails the flow.
    #[serde(default = "yes")]
    pub no_errors: bool,
    /// The whole trace's budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<f64>,
    /// Seconds without new spans before a trace counts as complete. Pulled
    /// sources deliver late: raise it for them.
    #[serde(default = "default_settle")]
    pub settle_secs: u64,
}

impl Flow {
    /// Parses and checks a flow; fills in step ids.
    pub fn parse(json: &str) -> Result<Self, String> {
        let mut flow: Flow =
            serde_json::from_str(json).map_err(|error| format!("invalid flow: {error}"))?;
        flow.validate()?;
        Ok(flow)
    }

    pub fn validate(&mut self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err("flow name: 1-64 letters, digits, '-', '_' or '.'".into());
        }
        if self.steps.is_empty() {
            return Err("a flow needs at least one step".into());
        }
        let mut seen = BTreeSet::new();
        for (index, step) in self.steps.iter_mut().enumerate() {
            if step.id.trim().is_empty() {
                step.id = (index + 1).to_string();
            }
            if !seen.insert(step.id.clone()) {
                return Err(format!("step id {:?} is used twice", step.id));
            }
            if step.service.is_empty() || step.span.is_empty() {
                return Err(format!(
                    "step {}: service and span are required (globs allowed)",
                    step.id
                ));
            }
            check_patterns(&step.attributes, &format!("step {}", step.id))?;
            if let Some(count) = &step.count
                && let (Some(min), Some(max)) = (count.min, count.max)
                && min > max
            {
                return Err(format!("step {}: count.min is above count.max", step.id));
            }
        }
        if let Some(selector) = &self.selector {
            check_patterns(&selector.attributes, "match")?;
        }
        Ok(())
    }
}

fn check_patterns(attributes: &serde_json::Map<String, Value>, at: &str) -> Result<(), String> {
    for (key, expected) in attributes {
        if let Value::String(text) = expected
            && let Some(pattern) = text.strip_prefix("re:")
        {
            Regex::new(pattern)
                .map_err(|error| format!("{at}: attribute {key}: bad regex: {error}"))?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ verdicts

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Ok,
    /// Optional and absent.
    Skipped,
    Missing,
    OutOfOrder,
    /// Matched, but attributes, count or the expected error differ.
    Mismatch,
    /// Failed without being expected to.
    Error,
    Slow,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepResult {
    pub id: String,
    pub status: StepStatus,
    pub service: String,
    pub span: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    /// How many spans matched the step's service and name.
    pub matches: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub service: String,
    pub span: String,
    pub span_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    /// No trace since the flow was armed touches it.
    Waiting,
    /// A trace is arriving; not decided yet.
    Running,
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub flow: String,
    pub status: VerdictStatus,
    /// No new spans for `settle_secs`: the status will not change unless a
    /// new trace arrives.
    pub settled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    pub traces_considered: usize,
    pub steps: Vec<StepResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unexpected_errors: Vec<Problem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub forbidden: Vec<Problem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    pub summary: String,
}

/// A trace offered to [`evaluate`].
pub struct Candidate<'a> {
    /// The trace's spans by start time.
    pub spans: &'a [Span],
    pub updated_ms: u64,
}

fn step_matches(step: &Step, span: &Span) -> bool {
    glob(&step.service, &span.service)
        && glob(&step.span, &span.name)
        && step.kind.is_none_or(|k| k == span.kind)
}

fn attribute_problems(
    step: &Step,
    span: &Span,
    masker: &Masker,
    pseudonyms: &mut Pseudonyms,
) -> Vec<String> {
    step.attributes
        .iter()
        .filter(|(key, expected)| !value_matches(expected, span.attribute(key)))
        .map(|(key, expected)| {
            let actual = span.attribute(key);
            let shown = actual.map(|value| masker.value(key, value, pseudonyms));
            match (expected.as_str(), shown) {
                (Some("!"), Some(shown)) => {
                    format!("{key}: must not be recorded, but is ({shown})")
                }
                (_, None) => format!("{key}: missing (expected {expected})"),
                (_, Some(shown)) => format!("{key}: expected {expected}, got {shown}"),
            }
        })
        .collect()
}

fn selected(flow: &Flow, spans: &[Span]) -> bool {
    let Some(selector) = &flow.selector else {
        return true;
    };
    if !selector.attributes.is_empty()
        && !selector
            .attributes
            .iter()
            .all(|(k, v)| spans.iter().any(|s| value_matches(v, s.attribute(k))))
    {
        return false;
    }
    if let Some(root) = &selector.root {
        let ids: BTreeSet<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
        let is_root = |s: &&Span| s.parent_id.as_deref().is_none_or(|p| !ids.contains(p));
        if !spans.iter().filter(is_root).any(|s| glob(root, &s.name)) {
            return false;
        }
    }
    true
}

struct Scored {
    verdict: Verdict,
    score: (usize, usize, u64),
}

fn judge(flow: &Flow, spans: &[Span], updated_ms: u64, now_ms: u64, masker: &Masker) -> Scored {
    let mut pseudonyms = Pseudonyms::default();
    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut previous_start = 0u64;
    let mut steps = Vec::new();
    for step in &flow.steps {
        let candidates: Vec<&Span> = spans.iter().filter(|s| step_matches(step, s)).collect();
        let free: Vec<&Span> = candidates
            .iter()
            .copied()
            .filter(|s| !used.contains(s.span_id.as_str()))
            .collect();
        let in_order: Vec<&Span> = match flow.order {
            Order::Sequence => free
                .iter()
                .copied()
                .filter(|s| s.start_ns >= previous_start)
                .collect(),
            Order::Any => free.clone(),
        };
        // Prefer a span whose attributes are right; else the first in order.
        let best = |pool: &[&'_ Span]| -> Option<Span> {
            pool.iter()
                .find(|s| {
                    attribute_problems(step, s, masker, &mut Pseudonyms::default()).is_empty()
                })
                .or_else(|| pool.first())
                .map(|s| (*s).clone())
        };
        let (chosen, out_of_order) = match best(&in_order) {
            Some(span) => (Some(span), false),
            None => (best(&free), !free.is_empty()),
        };
        let mut result = StepResult {
            id: step.id.clone(),
            status: StepStatus::Ok,
            service: step.service.clone(),
            span: step.span.clone(),
            span_id: None,
            duration_ms: None,
            matches: candidates.len(),
            problems: Vec::new(),
        };
        let Some(span) = chosen else {
            result.status = if step.optional {
                StepStatus::Skipped
            } else {
                StepStatus::Missing
            };
            if !step.optional {
                result.problems.push(format!(
                    "no span {:?} in service {:?}",
                    step.span, step.service
                ));
            }
            steps.push(result);
            continue;
        };
        used.insert(
            spans
                .iter()
                .find(|s| s.span_id == span.span_id)
                .map_or("", |s| s.span_id.as_str()),
        );
        result.span_id = Some(span.span_id.clone());
        result.duration_ms = Some(span.duration_ms());
        let mut status = StepStatus::Ok;
        let mut problems = attribute_problems(step, &span, masker, &mut pseudonyms);
        if !problems.is_empty() {
            status = StepStatus::Mismatch;
        }
        if let Some(count) = &step.count {
            let n = candidates.len();
            if count.min.is_some_and(|min| n < min) || count.max.is_some_and(|max| n > max) {
                problems.push(format!(
                    "{n} matching spans; expected {}",
                    match (count.min, count.max) {
                        (Some(a), Some(b)) if a == b => format!("exactly {a}"),
                        (Some(a), Some(b)) => format!("{a} to {b}"),
                        (Some(a), None) => format!("at least {a}"),
                        (None, Some(b)) => format!("at most {b}"),
                        (None, None) => "any".into(),
                    }
                ));
                status = StepStatus::Mismatch;
            }
        }
        match (step.error, span.is_error()) {
            (false, true) => {
                let message = span
                    .status_message
                    .as_deref()
                    .map(|m| masker.text(m, &mut pseudonyms));
                problems.push(format!(
                    "failed: {}",
                    message.unwrap_or_else(|| "error status".into())
                ));
                status = StepStatus::Error;
            }
            (true, false) => {
                problems.push("expected to fail, but succeeded".into());
                status = StepStatus::Mismatch;
            }
            _ => {}
        }
        if let Some(max) = step.max_ms
            && span.duration_ms() > max
        {
            problems.push(format!(
                "took {:.1} ms; budget {max} ms",
                span.duration_ms()
            ));
            if status == StepStatus::Ok {
                status = StepStatus::Slow;
            }
        }
        if out_of_order {
            problems.push("started before the previous step".into());
            if status == StepStatus::Ok {
                status = StepStatus::OutOfOrder;
            }
        }
        if flow.order == Order::Sequence && !out_of_order {
            previous_start = span.start_ns;
        }
        result.status = status;
        result.problems = problems;
        steps.push(result);
    }

    let problem = |span: &Span, message: String| Problem {
        service: span.service.clone(),
        span: span.name.clone(),
        span_id: span.span_id.clone(),
        message,
    };
    let expected_errors: BTreeSet<String> = steps
        .iter()
        .zip(&flow.steps)
        .filter(|(_, step)| step.error)
        .filter_map(|(result, _)| result.span_id.clone())
        .collect();
    let unexpected_errors: Vec<Problem> = if flow.no_errors {
        spans
            .iter()
            .filter(|s| s.is_error() && !expected_errors.contains(&s.span_id))
            .map(|s| {
                let message = s.status_message.as_deref().map_or_else(
                    || "error status".into(),
                    |m| masker.text(m, &mut pseudonyms),
                );
                problem(s, message)
            })
            .collect()
    } else {
        Vec::new()
    };
    let forbidden: Vec<Problem> = spans
        .iter()
        .filter(|s| {
            flow.forbid
                .iter()
                .any(|f| glob(&f.service, &s.service) && glob(&f.span, &s.name))
        })
        .map(|s| problem(s, "forbidden span".into()))
        .collect();
    let start = spans.iter().map(|s| s.start_ns).min().unwrap_or(0);
    let end = spans.iter().map(|s| s.end_ns).max().unwrap_or(0);
    let duration_ms = end.saturating_sub(start) as f64 / 1e6;
    let over_budget = flow.max_ms.is_some_and(|max| duration_ms > max);

    let settled = now_ms.saturating_sub(updated_ms) >= flow.settle_secs * 1000;
    let ok_steps = steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Ok | StepStatus::Skipped))
        .count();
    let complete = ok_steps == steps.len();
    let hard = !unexpected_errors.is_empty()
        || !forbidden.is_empty()
        || steps.iter().any(|s| s.status == StepStatus::Error);
    let status = if hard {
        VerdictStatus::Fail
    } else if complete && !over_budget {
        VerdictStatus::Pass
    } else if settled {
        VerdictStatus::Fail
    } else {
        VerdictStatus::Running
    };
    let mut parts = vec![format!("{ok_steps}/{} steps ok", steps.len())];
    for step in steps
        .iter()
        .filter(|s| !matches!(s.status, StepStatus::Ok | StepStatus::Skipped))
    {
        parts.push(format!("{} {:?}", step.id, step.status).to_lowercase());
    }
    if !unexpected_errors.is_empty() {
        parts.push(format!(
            "{} unexpected error span(s)",
            unexpected_errors.len()
        ));
    }
    if !forbidden.is_empty() {
        parts.push(format!("{} forbidden span(s)", forbidden.len()));
    }
    if over_budget {
        parts.push(format!(
            "trace took {duration_ms:.0} ms, over {} ms",
            flow.max_ms.unwrap_or(0.0)
        ));
    }
    let score = (
        ok_steps,
        steps.iter().filter(|s| s.span_id.is_some()).count(),
        start,
    );
    Scored {
        verdict: Verdict {
            flow: flow.name.clone(),
            status,
            settled,
            trace_id: spans.first().map(|s| s.trace_id.clone()),
            traces_considered: 0,
            steps,
            unexpected_errors,
            forbidden,
            duration_ms: Some(duration_ms),
            summary: parts.join("; "),
        },
        score,
    }
}

/// The verdict on the best of `candidates` (traces since the flow was
/// armed): the one meeting the most steps, then the newest.
pub fn evaluate(
    flow: &Flow,
    candidates: &[Candidate<'_>],
    now_ms: u64,
    masker: &Masker,
) -> Verdict {
    let relevant: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| selected(flow, c.spans))
        .filter(|c| {
            c.spans
                .iter()
                .any(|s| flow.steps.iter().any(|step| step_matches(step, s)))
        })
        .collect();
    let best = relevant
        .iter()
        .map(|c| judge(flow, c.spans, c.updated_ms, now_ms, masker))
        .max_by(|a, b| a.score.cmp(&b.score));
    match best {
        Some(scored) => Verdict {
            traces_considered: relevant.len(),
            ..scored.verdict
        },
        None => Verdict {
            flow: flow.name.clone(),
            status: VerdictStatus::Waiting,
            settled: false,
            trace_id: None,
            traces_considered: 0,
            steps: flow
                .steps
                .iter()
                .map(|step| StepResult {
                    id: step.id.clone(),
                    status: if step.optional {
                        StepStatus::Skipped
                    } else {
                        StepStatus::Missing
                    },
                    service: step.service.clone(),
                    span: step.span.clone(),
                    span_id: None,
                    duration_ms: None,
                    matches: 0,
                    problems: Vec::new(),
                })
                .collect(),
            unexpected_errors: Vec::new(),
            forbidden: Vec::new(),
            duration_ms: None,
            summary: "no trace yet: trigger the feature".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;
    use crate::store::tests::span;
    use serde_json::json;

    fn checkout() -> Flow {
        Flow::parse(
            &json!({
                "name": "checkout",
                "match": {"attributes": {"test.run": "r-1"}},
                "steps": [
                    {"id": "api", "service": "orders-api", "span": "POST /orders", "attributes": {"order.items": 3, "test.run": "*"}},
                    {"id": "validate", "service": "orders-api", "span": "validate order", "attributes": {"customer.email": "re:@example\\.com$", "card.number": "!"}},
                    {"id": "reserve", "service": "stock*", "span": "reserve", "count": {"min": 1, "max": 1}, "max_ms": 50},
                    {"id": "notify", "service": "mailer", "span": "send", "optional": true}
                ],
                "forbid": [{"span": "retry*"}],
                "max_ms": 1000
            })
            .to_string(),
        )
        .unwrap()
    }

    fn trace(items: u64, email: &str) -> Vec<Span> {
        let mut root = span("a", "1", None, "orders-api", "POST /orders", 0, 100_000_000);
        root.attributes.insert("order.items".into(), json!(items));
        root.attributes.insert("test.run".into(), json!("r-1"));
        let mut validate = span(
            "a",
            "2",
            Some("1"),
            "orders-api",
            "validate order",
            1_000_000,
            2_000_000,
        );
        validate
            .attributes
            .insert("customer.email".into(), json!(email));
        let reserve = span(
            "a",
            "3",
            Some("1"),
            "stock-api",
            "reserve",
            3_000_000,
            30_000_000,
        );
        vec![root, validate, reserve]
    }

    fn verdict(spans: &[Span], updated_ms: u64, now_ms: u64) -> Verdict {
        evaluate(
            &checkout(),
            &[Candidate { spans, updated_ms }],
            now_ms,
            &Masker::default(),
        )
    }

    #[test]
    fn globs() {
        assert!(glob("stock*", "stock-api"));
        assert!(glob("*orders*", "POST /orders/42"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("stock*", "orders"));
        assert!(glob("*", ""));
    }

    #[test]
    fn expectations() {
        assert!(value_matches(&json!(3), Some(&json!("3"))));
        assert!(value_matches(&json!("*"), Some(&json!(false))));
        assert!(!value_matches(&json!("*"), None));
        assert!(value_matches(&json!("!"), None));
        assert!(!value_matches(&json!("!"), Some(&json!("4111"))));
        assert!(value_matches(&json!("re:^o-\\d+$"), Some(&json!("o-42"))));
    }

    #[test]
    fn a_trace_that_does_what_the_flow_says_passes() {
        let spans = trace(3, "ann@example.com");
        let v = verdict(&spans, 0, 100);
        assert_eq!(v.status, VerdictStatus::Pass, "{v:#?}");
        assert!(!v.settled);
        assert_eq!(v.steps[3].status, StepStatus::Skipped);
        assert_eq!(v.summary, "4/4 steps ok");
        assert!(verdict(&spans, 0, 20_000).settled);
    }

    #[test]
    fn wrong_values_are_reported_masked() {
        let v = verdict(&trace(2, "bob@evil.org"), 0, 100);
        assert_eq!(
            v.status,
            VerdictStatus::Running,
            "may still change until settled"
        );
        assert_eq!(v.steps[0].status, StepStatus::Mismatch);
        assert_eq!(v.steps[0].problems, ["order.items: expected 3, got 2"]);
        assert_eq!(
            v.steps[1].problems,
            ["customer.email: expected \"re:@example\\\\.com$\", got \"<customer_email#1>\""]
        );
        let text = serde_json::to_string(&v).unwrap();
        assert!(
            !text.contains("bob@evil.org"),
            "the actual value never reaches the agent"
        );
        assert_eq!(
            verdict(&trace(2, "bob@evil.org"), 0, 20_000).status,
            VerdictStatus::Fail
        );
    }

    #[test]
    fn errors_retries_forbidden_spans_and_order_fail() {
        let mut spans = trace(3, "ann@example.com");
        spans[2].status = Status::Error;
        spans[2].status_message = Some("out of stock for ann@example.com".into());
        let v = verdict(&spans, 0, 100);
        assert_eq!(v.status, VerdictStatus::Fail, "an error is final");
        assert_eq!(v.steps[2].status, StepStatus::Error);
        assert_eq!(v.steps[2].problems, ["failed: out of stock for <email#1>"]);
        assert_eq!(v.unexpected_errors.len(), 1);

        let mut spans = trace(3, "ann@example.com");
        spans.push(span(
            "a",
            "4",
            Some("1"),
            "stock-api",
            "reserve",
            31_000_000,
            40_000_000,
        ));
        spans.push(span(
            "a",
            "5",
            Some("1"),
            "stock-api",
            "retry reserve",
            30_500_000,
            30_600_000,
        ));
        let v = verdict(&spans, 0, 100);
        assert_eq!(v.steps[2].status, StepStatus::Mismatch);
        assert_eq!(
            v.steps[2].problems,
            ["2 matching spans; expected exactly 1"]
        );
        assert_eq!(v.forbidden.len(), 1);
        assert_eq!(v.status, VerdictStatus::Fail);

        let mut spans = trace(3, "ann@example.com");
        spans[1].start_ns = 50_000_000; // validate after reserve
        spans[1].end_ns = 51_000_000;
        let v = verdict(&spans, 0, 20_000);
        assert_eq!(v.steps[2].status, StepStatus::OutOfOrder, "{v:#?}");
    }

    #[test]
    fn budgets_and_selection() {
        let mut spans = trace(3, "ann@example.com");
        spans[2].end_ns = 80_000_000;
        let v = verdict(&spans, 0, 100);
        assert_eq!(v.steps[2].status, StepStatus::Slow);
        let mut other = trace(3, "ann@example.com");
        other[0].attributes.insert("test.run".into(), json!("r-2"));
        let v = verdict(&other, 0, 100);
        assert_eq!(
            v.status,
            VerdictStatus::Waiting,
            "another run's trace is not considered"
        );
    }

    #[test]
    fn the_best_trace_wins() {
        let good = trace(3, "ann@example.com");
        let partial: Vec<Span> = trace(3, "ann@example.com")
            .into_iter()
            .take(1)
            .map(|mut s| {
                s.trace_id = "b".repeat(32);
                s
            })
            .collect();
        let v = evaluate(
            &checkout(),
            &[
                Candidate {
                    spans: &partial,
                    updated_ms: 0,
                },
                Candidate {
                    spans: &good,
                    updated_ms: 0,
                },
            ],
            100,
            &Masker::default(),
        );
        assert_eq!(v.trace_id.as_deref(), Some(good[0].trace_id.as_str()));
        assert_eq!(v.traces_considered, 2);
    }

    #[test]
    fn invalid_flows_are_refused_with_a_reason() {
        assert!(
            Flow::parse(r#"{"name":"x","steps":[]}"#)
                .unwrap_err()
                .contains("at least one step")
        );
        assert!(
            Flow::parse(r#"{"name":"bad name","steps":[{"service":"a","span":"b"}]}"#).is_err()
        );
        assert!(
            Flow::parse(
                r#"{"name":"x","steps":[{"service":"a","span":"b","attributes":{"k":"re:("}}]}"#
            )
            .unwrap_err()
            .contains("bad regex")
        );
        assert!(
            Flow::parse(r#"{"name":"x","steps":[{"service":"a","span":"b","colour":1}]}"#)
                .unwrap_err()
                .contains("unknown field")
        );
        let flow = Flow::parse(
            r#"{"name":"x","steps":[{"service":"a","span":"b"},{"service":"a","span":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(
            (flow.steps[0].id.as_str(), flow.steps[1].id.as_str()),
            ("1", "2")
        );
    }
}
