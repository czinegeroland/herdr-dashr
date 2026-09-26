//! Log expectations: "did the log messages I am looking for fire?"
//!
//! The human (or the agent) arms a set of expectations — messages that
//! should appear, and messages that must not. The dashboard gets a section
//! at the top (requirements DASHR-LOGX-001..003):
//!
//! * one tile per expectation, counting matching lines since the moment the
//!   expectations were armed: grey "waiting" then green for a message that
//!   should appear, green "none" then red for one that must not;
//! * a live trail of every line, newest first, with matching lines
//!   highlighted in the same colours.
//!
//! Matching runs in Loki (LogQL `|~`, case-insensitive) for the counts and in
//! the browser (a table value mapping) for the highlight, so the patterns
//! have to mean the same thing to RE2 and to JavaScript. [`validate`] keeps
//! them to the common subset, and [`js_case_insensitive`] spells the `(?i)`
//! flag, which JavaScript lacks inline, as character classes.
//!
//! The agent never needs line content for any of this: the tiles are counts.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::watch::{Comparison, Reducer, Severity, WatchRule};

/// Panel ids reserved for the expectation section.
pub const ID_FIRST: i64 = 9000;
pub const ID_LAST: i64 = 9199;
/// The live trail's panel id.
pub const TRAIL_ID: i64 = 9100;
/// The most expectations one section holds.
pub const MAX_EXPECTATIONS: usize = 12;
/// Every stream Loki holds, when no selector is given.
pub const DEFAULT_SELECTOR: &str = r#"{service_name=~".+"}"#;
/// Watch ids for expectation watches start with this.
pub const WATCH_PREFIX: &str = "logx-";

const TILE_HEIGHT: i64 = 4;
const TRAIL_HEIGHT: i64 = 14;
const GREY: &str = "#8e8e8e";

/// Whether a message should appear or must not.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    #[default]
    Present,
    Absent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Expectation {
    pub name: String,
    pub pattern: String,
    #[serde(default)]
    pub expect: Presence,
}

/// An armed set of expectations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogxSpec {
    pub expectations: Vec<Expectation>,
    /// LogQL stream selector, e.g. `{service_name="checkout"}`.
    pub selector: String,
    /// The Loki datasource to query.
    pub datasource_uid: String,
    /// Only lines after this moment count (epoch milliseconds).
    pub armed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogxError {
    #[error("give between 1 and {MAX_EXPECTATIONS} expectations")]
    Count,
    #[error("expectation names must be 1-60 characters and unique: {0:?}")]
    Name(String),
    #[error("pattern {0:?}: {1}")]
    Pattern(String, String),
    #[error("selector {0:?} must be a LogQL stream selector such as {{service_name=\"checkout\"}}")]
    Selector(String),
}

/// Checks a spec; patterns must mean the same to Loki and to the browser.
pub fn validate(spec: &LogxSpec) -> Result<(), LogxError> {
    if spec.expectations.is_empty() || spec.expectations.len() > MAX_EXPECTATIONS {
        return Err(LogxError::Count);
    }
    let mut names = std::collections::BTreeSet::new();
    for expectation in &spec.expectations {
        let name = expectation.name.trim();
        if name.is_empty() || name.chars().count() > 60 || !names.insert(name.to_lowercase()) {
            return Err(LogxError::Name(expectation.name.clone()));
        }
        let pattern = &expectation.pattern;
        let refuse = |why: &str| Err(LogxError::Pattern(pattern.clone(), why.to_owned()));
        if pattern.trim().is_empty() || pattern.chars().count() > 200 {
            return refuse("must be 1-200 characters");
        }
        if pattern.contains('`') {
            return refuse("backticks are not allowed");
        }
        // Inline flags, named groups and look-arounds differ between RE2
        // (Loki) and JavaScript (the highlight); keep to the common subset.
        if pattern.contains("(?") {
            return refuse("(? constructs are not supported; matching is already case-insensitive");
        }
        if let Err(error) = regex::Regex::new(&format!("(?i){pattern}")) {
            return Err(LogxError::Pattern(pattern.clone(), error.to_string()));
        }
    }
    let selector = spec.selector.trim();
    if !(selector.starts_with('{') && selector.ends_with('}')) || selector.contains('`') {
        return Err(LogxError::Selector(spec.selector.clone()));
    }
    Ok(())
}

/// Spells a case-insensitive match for JavaScript, which has no inline
/// `(?i)`: each ASCII letter outside a character class becomes `[xX]`.
pub fn js_case_insensitive(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() * 3);
    let mut chars = pattern.chars();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '[' if !in_class => {
                in_class = true;
                out.push(c);
            }
            ']' if in_class => {
                in_class = false;
                out.push(c);
            }
            c if !in_class && c.is_ascii_alphabetic() => {
                out.push('[');
                out.push(c.to_ascii_lowercase());
                out.push(c.to_ascii_uppercase());
                out.push(']');
            }
            c => out.push(c),
        }
    }
    out
}

/// The LogQL count of lines matching `pattern` over the dashboard range.
/// `or vector(0)` makes a message that has not appeared read as 0, not as
/// "no data".
pub fn count_query(selector: &str, pattern: &str) -> String {
    format!("sum(count_over_time({selector} |~ `(?i){pattern}` [$__range])) or vector(0)")
}

fn datasource(uid: &str) -> Value {
    json!({"type": "loki", "uid": uid})
}

fn tile(spec: &LogxSpec, index: usize, grid: Value) -> Value {
    let expectation = &spec.expectations[index];
    let (zero_text, zero_color, hit_color) = match expectation.expect {
        Presence::Present => ("waiting", GREY, "green"),
        Presence::Absent => ("none", "green", "red"),
    };
    json!({
        "id": ID_FIRST + index as i64,
        "type": "stat",
        "title": expectation.name.trim(),
        "description": format!(
            "{} /{}/ since the expectations were armed",
            match expectation.expect { Presence::Present => "Should appear:", Presence::Absent => "Must not appear:" },
            expectation.pattern
        ),
        "gridPos": grid,
        "datasource": datasource(&spec.datasource_uid),
        "targets": [{
            "refId": "A",
            "datasource": datasource(&spec.datasource_uid),
            "expr": count_query(&spec.selector, &expectation.pattern),
            "queryType": "instant"
        }],
        "fieldConfig": {"defaults": {
            "mappings": [{"type": "value", "options": {"0": {"text": zero_text, "color": zero_color}}}],
            "thresholds": {"mode": "absolute", "steps": [
                {"color": zero_color, "value": null},
                {"color": hit_color, "value": 1}
            ]},
            "color": {"mode": "thresholds"}
        }},
        "options": {"colorMode": "background", "graphMode": "none", "textMode": "value",
                    "reduceOptions": {"calcs": ["lastNotNull"]}}
    })
}

/// The live trail: every line, newest first, matches highlighted.
pub fn trail(spec: &LogxSpec, y: i64) -> Value {
    // Must-not-appear first: Grafana applies the first matching mapping, so
    // a line matching both reads as the violation it is.
    let mut ordered: Vec<&Expectation> = spec
        .expectations
        .iter()
        .filter(|e| e.expect == Presence::Absent)
        .collect();
    ordered.extend(
        spec.expectations
            .iter()
            .filter(|e| e.expect == Presence::Present),
    );
    let mappings: Vec<Value> = ordered
        .iter()
        .enumerate()
        .map(|(index, expectation)| {
            json!({"type": "regex", "options": {
                "pattern": format!(".*(?:{}).*", js_case_insensitive(&expectation.pattern)),
                "result": {
                    "color": if expectation.expect == Presence::Absent { "red" } else { "green" },
                    "index": index
                }
            }})
        })
        .collect();
    json!({
        "id": TRAIL_ID,
        "type": "table",
        "title": "Live log trail",
        "description": "Every line since the expectations were armed, newest first. Green: expected message. Red: must-not-appear message.",
        "gridPos": {"x": 0, "y": y, "w": 24, "h": TRAIL_HEIGHT},
        "datasource": datasource(&spec.datasource_uid),
        "targets": [{
            "refId": "A",
            "datasource": datasource(&spec.datasource_uid),
            "expr": spec.selector,
            "queryType": "range",
            "maxLines": 1000
        }],
        "transformations": [{"id": "organize", "options": {
            "excludeByName": {"labels": true, "tsNs": true, "id": true, "labelTypes": true,
                              "trace_id": true, "span_id": true},
            "renameByName": {"Line": "Message"}
        }}],
        "fieldConfig": {"defaults": {}, "overrides": [
            {"matcher": {"id": "byName", "options": "Time"},
             "properties": [{"id": "custom.width", "value": 200}]},
            {"matcher": {"id": "byName", "options": "Line"},
             "properties": [
                {"id": "custom.cellOptions", "value": {"type": "color-background", "mode": "basic"}},
                {"id": "mappings", "value": mappings},
                {"id": "color", "value": {"mode": "fixed", "fixedColor": "transparent"}}
             ]}
        ]},
        "options": {"showHeader": true, "cellHeight": "sm",
                    "sortBy": [{"displayName": "Time", "desc": true}]}
    })
}

/// The whole section and its height in grid rows.
pub fn section(spec: &LogxSpec) -> (Vec<Value>, i64) {
    let count = spec.expectations.len() as i64;
    let per_row = count.min(6);
    let width = 24 / per_row;
    let mut panels = Vec::new();
    for index in 0..spec.expectations.len() {
        let i = index as i64;
        let grid = json!({
            "x": (i % per_row) * width,
            "y": (i / per_row) * TILE_HEIGHT,
            "w": width,
            "h": TILE_HEIGHT
        });
        panels.push(tile(spec, index, grid));
    }
    let tiles_height = (count + per_row - 1) / per_row * TILE_HEIGHT;
    panels.push(trail(spec, tiles_height));
    (panels, tiles_height + TRAIL_HEIGHT)
}

fn is_section_panel(panel: &Value) -> bool {
    panel
        .get("id")
        .and_then(Value::as_i64)
        .is_some_and(|id| (ID_FIRST..=ID_LAST).contains(&id))
}

fn shift(panel: &mut Value, by: i64) {
    if let Some(y) = panel.pointer("/gridPos/y").and_then(Value::as_i64) {
        panel["gridPos"]["y"] = json!((y + by).max(0));
    }
    if let Some(children) = panel.get_mut("panels").and_then(Value::as_array_mut) {
        for child in children {
            shift(child, by);
        }
    }
}

/// The dashboard with the section at the top, replacing any earlier one,
/// and its time range starting when the expectations were armed.
pub fn merge(dashboard: &Value, spec: &LogxSpec) -> Value {
    let mut out = remove(dashboard);
    let (section, height) = section(spec);
    let mut rest: Vec<Value> = out
        .get("panels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for panel in &mut rest {
        shift(panel, height);
    }
    let mut panels = section;
    panels.extend(rest);
    out["panels"] = Value::Array(panels);
    out["time"] = json!({"from": window_start(spec), "to": "now"});
    out
}

/// Grafana renders `$__range` in whole seconds, so a window starting exactly
/// at arming would start up to a second late and a line logged just after
/// arming would flicker in and out of the count. Starting one second early
/// keeps every line after arming, at the cost of at most a second before it.
pub const WINDOW_SLACK_MS: i64 = 1000;

/// The dashboard's `time.from` for a spec.
pub fn window_start(spec: &LogxSpec) -> String {
    rfc3339(spec.armed_at_ms - WINDOW_SLACK_MS)
}

/// Epoch milliseconds as `2026-09-26T13:30:57.181Z`. Grafana's time picker
/// reads this form; it shows a bare epoch-millisecond string as "Invalid
/// date".
pub fn rfc3339(epoch_ms: i64) -> String {
    let seconds = epoch_ms.div_euclid(1000);
    let millis = epoch_ms.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

/// The dashboard without the section, the remaining panels moved back up.
pub fn remove(dashboard: &Value) -> Value {
    let mut out = dashboard.clone();
    let kept: Vec<Value> = dashboard
        .get("panels")
        .and_then(Value::as_array)
        .map(|panels| {
            panels
                .iter()
                .filter(|p| !is_section_panel(p))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let had_section = dashboard
        .get("panels")
        .and_then(Value::as_array)
        .is_some_and(|panels| panels.iter().any(is_section_panel));
    let top = kept
        .iter()
        .filter_map(|p| p.pointer("/gridPos/y").and_then(Value::as_i64))
        .min()
        .unwrap_or(0);
    let mut kept = kept;
    if had_section && top > 0 {
        for panel in &mut kept {
            shift(panel, -top);
        }
    }
    out["panels"] = Value::Array(kept);
    out
}

/// One watch per expectation: a message that should appear notifies when it
/// does; one that must not raises an alert.
pub fn watch_rules(spec: &LogxSpec) -> Vec<WatchRule> {
    spec.expectations
        .iter()
        .enumerate()
        .map(|(index, expectation)| WatchRule {
            id: format!("{WATCH_PREFIX}{index}"),
            panel_id: ID_FIRST + index as i64,
            reducer: Reducer::Last,
            op: Comparison::Gt,
            threshold: 0.0,
            label: Some(match expectation.expect {
                Presence::Present => format!("✓ {} logged", expectation.name.trim()),
                Presence::Absent => format!("✗ {} logged", expectation.name.trim()),
            }),
            severity: match expectation.expect {
                Presence::Present => Severity::Info,
                Presence::Absent => Severity::Alert,
            },
        })
        .collect()
}

/// What one expectation's count says.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Should appear, has not yet.
    Waiting,
    /// Should appear, has.
    Seen,
    /// Must not appear, has not.
    Clear,
    /// Must not appear, has.
    Violated,
}

pub fn outcome(expect: Presence, count: u64) -> Outcome {
    match (expect, count) {
        (Presence::Present, 0) => Outcome::Waiting,
        (Presence::Present, _) => Outcome::Seen,
        (Presence::Absent, 0) => Outcome::Clear,
        (Presence::Absent, _) => Outcome::Violated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(expectations: &[(&str, &str, Presence)]) -> LogxSpec {
        LogxSpec {
            expectations: expectations
                .iter()
                .map(|(name, pattern, expect)| Expectation {
                    name: (*name).into(),
                    pattern: (*pattern).into(),
                    expect: *expect,
                })
                .collect(),
            selector: DEFAULT_SELECTOR.into(),
            datasource_uid: "loki".into(),
            armed_at_ms: 1_790_000_000_000,
        }
    }

    #[test]
    fn validation_keeps_patterns_portable() {
        let ok = spec(&[("order", "order created", Presence::Present)]);
        assert!(validate(&ok).is_ok());
        let bad =
            |pattern: &str| validate(&spec(&[("x", pattern, Presence::Present)])).unwrap_err();
        assert!(matches!(bad("a`b"), LogxError::Pattern(..)));
        assert!(matches!(bad("(?i)order"), LogxError::Pattern(..)));
        assert!(matches!(bad("(?P<n>x)"), LogxError::Pattern(..)));
        assert!(matches!(bad("unclosed("), LogxError::Pattern(..)));
        assert!(matches!(bad(" "), LogxError::Pattern(..)));
        let dup = spec(&[("A", "x", Presence::Present), ("a", "y", Presence::Present)]);
        assert!(matches!(validate(&dup), Err(LogxError::Name(_))));
        let mut selector = ok.clone();
        selector.selector = "service_name=checkout".into();
        assert!(matches!(validate(&selector), Err(LogxError::Selector(_))));
        let many: Vec<(&str, &str, Presence)> = Vec::new();
        assert_eq!(validate(&spec(&many)), Err(LogxError::Count));
    }

    #[test]
    fn js_folding_matches_case_insensitively_and_keeps_classes_and_escapes() {
        assert_eq!(js_case_insensitive("Ok 1"), "[oO][kK] 1");
        assert_eq!(js_case_insensitive(r"a\.b[a-z]\d"), r"[aA]\.[bB][a-z]\d");
        // The folded pattern must match what (?i) matches in RE2.
        for (pattern, line) in [
            ("order created", "INFO ORDER Created id=1"),
            ("payment .* captured", "Payment for 42 was CAPTURED"),
            (r"id=\d+", "ID=42"),
        ] {
            let folded = regex::Regex::new(&js_case_insensitive(pattern)).unwrap();
            let insensitive = regex::Regex::new(&format!("(?i){pattern}")).unwrap();
            assert_eq!(
                folded.is_match(line),
                insensitive.is_match(line),
                "{pattern} vs {line}"
            );
            assert!(folded.is_match(line), "{pattern} vs {line}");
        }
    }

    #[test]
    fn count_query_is_case_insensitive_and_zero_filled() {
        assert_eq!(
            count_query(DEFAULT_SELECTOR, "order created"),
            r#"sum(count_over_time({service_name=~".+"} |~ `(?i)order created` [$__range])) or vector(0)"#
        );
    }

    #[test]
    fn section_has_tiles_then_a_highlighted_trail() {
        let spec = spec(&[
            ("order created", "order created", Presence::Present),
            ("payment", "payment .* captured", Presence::Present),
            ("no exceptions", "exception", Presence::Absent),
        ]);
        let (panels, height) = section(&spec);
        assert_eq!(panels.len(), 4);
        assert_eq!(height, 4 + 14);
        assert_eq!(
            panels[0]["gridPos"],
            json!({"x": 0, "y": 0, "w": 8, "h": 4})
        );
        assert_eq!(
            panels[2]["fieldConfig"]["defaults"]["thresholds"]["steps"][1]["color"],
            "red"
        );
        assert_eq!(
            panels[0]["fieldConfig"]["defaults"]["mappings"][0]["options"]["0"]["text"],
            "waiting"
        );
        let trail = &panels[3];
        assert_eq!(trail["id"], TRAIL_ID);
        assert_eq!(trail["gridPos"]["y"], 4);
        let mappings = &trail["fieldConfig"]["overrides"][1]["properties"][1]["value"];
        assert_eq!(
            mappings[0]["options"]["result"]["color"], "red",
            "violations first"
        );
        assert_eq!(
            mappings[1]["options"]["pattern"],
            ".*(?:[oO][rR][dD][eE][rR] [cC][rR][eE][aA][tT][eE][dD]).*"
        );
        // Everything validates as a dashboard.
        let known = vec!["loki".to_owned()];
        let dashboard = json!({"title": "t", "panels": panels});
        let normalized = crate::dashboard::normalize(
            &dashboard,
            &crate::dashboard::Pins {
                uid: "u",
                refresh: "2s",
                time_from: "now-1h",
                known_datasources: &known,
            },
        )
        .unwrap();
        assert!(normalized.warnings.is_empty(), "{:?}", normalized.warnings);
    }

    #[test]
    fn merge_puts_the_section_on_top_and_replaces_an_earlier_one() {
        let existing = json!({"title": "work", "time": {"from": "now-6h", "to": "now"}, "panels": [
            {"id": 1, "type": "timeseries", "gridPos": {"x": 0, "y": 0, "w": 24, "h": 8}},
            {"id": 2, "type": "row", "gridPos": {"x": 0, "y": 8, "w": 24, "h": 1},
             "panels": [{"id": 3, "type": "stat", "gridPos": {"x": 0, "y": 9, "w": 6, "h": 4}}]}
        ]});
        let one = spec(&[("a", "alpha", Presence::Present)]);
        let merged = merge(&existing, &one);
        let panels = merged["panels"].as_array().unwrap();
        assert_eq!(panels[0]["id"], ID_FIRST);
        assert_eq!(panels[1]["id"], TRAIL_ID);
        assert_eq!(panels[2]["id"], 1);
        assert_eq!(panels[2]["gridPos"]["y"], 18);
        assert_eq!(panels[3]["panels"][0]["gridPos"]["y"], 27);
        assert_eq!(merged["time"]["from"], "2026-09-21T14:13:19.000Z");
        assert_eq!(merged["title"], "work");
        // Re-arming replaces, it does not stack.
        let two = spec(&[
            ("a", "alpha", Presence::Present),
            ("b", "beta", Presence::Present),
        ]);
        let again = merge(&merged, &two);
        let ids: Vec<i64> = again["panels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![ID_FIRST, ID_FIRST + 1, TRAIL_ID, 1, 2]);
        assert_eq!(again["panels"][3]["gridPos"]["y"], 18);
        // Clearing restores the original layout.
        let cleared = remove(&again);
        assert_eq!(cleared["panels"][0]["gridPos"]["y"], 0);
        assert_eq!(cleared["panels"][1]["panels"][0]["gridPos"]["y"], 9);
        assert_eq!(remove(&existing), existing, "no section: nothing moves");
    }

    #[test]
    fn watches_notify_for_expected_and_alert_for_forbidden() {
        let rules = watch_rules(&spec(&[
            ("order created", "order created", Presence::Present),
            ("no exceptions", "exception", Presence::Absent),
        ]));
        assert_eq!(rules[0].id, "logx-0");
        assert_eq!(rules[0].panel_id, ID_FIRST);
        assert_eq!(rules[0].severity, Severity::Info);
        assert_eq!(rules[0].label.as_deref(), Some("✓ order created logged"));
        assert_eq!(rules[1].severity, Severity::Alert);
        assert_eq!(rules[1].label.as_deref(), Some("✗ no exceptions logged"));
    }

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_790_429_457_181), "2026-09-26T13:30:57.181Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(rfc3339(1_735_689_599_999), "2024-12-31T23:59:59.999Z");
    }

    #[test]
    fn outcomes() {
        assert_eq!(outcome(Presence::Present, 0), Outcome::Waiting);
        assert_eq!(outcome(Presence::Present, 3), Outcome::Seen);
        assert_eq!(outcome(Presence::Absent, 0), Outcome::Clear);
        assert_eq!(outcome(Presence::Absent, 1), Outcome::Violated);
    }
}
