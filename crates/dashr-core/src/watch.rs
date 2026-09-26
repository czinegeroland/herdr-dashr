//! Watches: thresholds the agent sets and the pane evaluates locally.
//!
//! The agent says "tell the human when the DLQ depth panel goes above 0"
//! without ever seeing the depth. The dashboard pane evaluates the rule on
//! its own schedule against real values; all that leaves it is a breached or
//! clear state and a Herdr notification (requirement DASHR-ALERT-001).

use serde::{Deserialize, Serialize};

use crate::frames::QueryResult;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    Last,
    Max,
    Min,
    Mean,
    Sum,
    /// Number of rows across all frames; works for logs and tables too.
    Count,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Comparison {
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
}

impl Comparison {
    pub fn holds(self, left: f64, right: f64) -> bool {
        match self {
            Comparison::Gt => left > right,
            Comparison::Ge => left >= right,
            Comparison::Lt => left < right,
            Comparison::Le => left <= right,
            Comparison::Eq => (left - right).abs() < f64::EPSILON,
            Comparison::Ne => (left - right).abs() >= f64::EPSILON,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Comparison::Gt => ">",
            Comparison::Ge => ">=",
            Comparison::Lt => "<",
            Comparison::Le => "<=",
            Comparison::Eq => "==",
            Comparison::Ne => "!=",
        }
    }
}

/// How a breach is announced. An alert marks the pane blocked until it
/// clears; info only notifies (a message that was expected has arrived).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Alert,
    Info,
}

/// One watch rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchRule {
    pub id: String,
    pub panel_id: i64,
    pub reducer: Reducer,
    pub op: Comparison,
    pub threshold: f64,
    /// Short human wording for the notification, e.g. "DLQ not empty".
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub severity: Severity,
}

impl WatchRule {
    /// The notification wording. Contains the rule, never the value.
    pub fn describe(&self, panel_title: &str) -> String {
        match &self.label {
            Some(label) if !label.trim().is_empty() => label.trim().to_owned(),
            _ => format!(
                "{panel_title}: {:?} {} {}",
                self.reducer,
                self.op.symbol(),
                self.threshold
            )
            .to_lowercase(),
        }
    }
}

/// The outcome of evaluating one rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Evaluation {
    Breached,
    Clear,
    /// No numeric data to judge by. Not a breach: an empty panel is what
    /// `panel_status` is for.
    NoData,
}

/// Reduces every numeric series of every result and checks the rule against
/// each; any series breaching breaches the rule.
pub fn evaluate(rule: &WatchRule, results: &[QueryResult]) -> Evaluation {
    if rule.reducer == Reducer::Count {
        let rows: usize = results.iter().map(QueryResult::row_count).sum();
        return if rule.op.holds(rows as f64, rule.threshold) {
            Evaluation::Breached
        } else {
            Evaluation::Clear
        };
    }
    let mut saw_data = false;
    for result in results {
        for frame in &result.frames {
            for series in frame.numeric_series() {
                let Some(value) = reduce(rule.reducer, &series) else {
                    continue;
                };
                saw_data = true;
                if rule.op.holds(value, rule.threshold) {
                    return Evaluation::Breached;
                }
            }
        }
    }
    if saw_data {
        Evaluation::Clear
    } else {
        Evaluation::NoData
    }
}

fn reduce(reducer: Reducer, series: &[f64]) -> Option<f64> {
    if series.is_empty() {
        return None;
    }
    Some(match reducer {
        Reducer::Last => *series.last()?,
        Reducer::Max => series.iter().copied().fold(f64::MIN, f64::max),
        Reducer::Min => series.iter().copied().fold(f64::MAX, f64::min),
        Reducer::Mean => series.iter().sum::<f64>() / series.len() as f64,
        Reducer::Sum => series.iter().sum(),
        Reducer::Count => series.len() as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{Frame, QueryResult};
    use crate::masking::FieldInput;
    use serde_json::json;

    fn result(values: &[f64]) -> QueryResult {
        QueryResult {
            ref_id: "A".into(),
            status: Some(200),
            error: None,
            frames: vec![Frame {
                name: None,
                fields: vec![
                    FieldInput {
                        name: "time".into(),
                        field_type: "time".into(),
                        labels: Default::default(),
                    },
                    FieldInput {
                        name: "v".into(),
                        field_type: "number".into(),
                        labels: Default::default(),
                    },
                ],
                columns: vec![
                    values.iter().map(|_| json!(0)).collect(),
                    values.iter().map(|v| json!(v)).collect(),
                ],
            }],
        }
    }

    fn rule(reducer: Reducer, op: Comparison, threshold: f64) -> WatchRule {
        WatchRule {
            id: "w1".into(),
            panel_id: 1,
            reducer,
            op,
            threshold,
            label: None,
            severity: Default::default(),
        }
    }

    #[test]
    fn reducers_and_comparisons() {
        let results = [result(&[1.0, 5.0, 2.0])];
        assert_eq!(
            evaluate(&rule(Reducer::Last, Comparison::Gt, 1.5), &results),
            Evaluation::Breached
        );
        assert_eq!(
            evaluate(&rule(Reducer::Last, Comparison::Gt, 2.0), &results),
            Evaluation::Clear
        );
        assert_eq!(
            evaluate(&rule(Reducer::Max, Comparison::Ge, 5.0), &results),
            Evaluation::Breached
        );
        assert_eq!(
            evaluate(&rule(Reducer::Min, Comparison::Lt, 1.0), &results),
            Evaluation::Clear
        );
        assert_eq!(
            evaluate(&rule(Reducer::Mean, Comparison::Eq, 8.0 / 3.0), &results),
            Evaluation::Breached
        );
        assert_eq!(
            evaluate(&rule(Reducer::Sum, Comparison::Ne, 8.0), &results),
            Evaluation::Clear
        );
        assert_eq!(
            evaluate(&rule(Reducer::Count, Comparison::Ge, 3.0), &results),
            Evaluation::Breached
        );
    }

    #[test]
    fn no_numbers_is_no_data_not_a_breach() {
        assert_eq!(
            evaluate(&rule(Reducer::Max, Comparison::Gt, 0.0), &[result(&[])]),
            Evaluation::NoData
        );
        assert_eq!(
            evaluate(&rule(Reducer::Max, Comparison::Gt, 0.0), &[]),
            Evaluation::NoData
        );
    }

    #[test]
    fn rules_round_trip_with_symbolic_operators() {
        let text = r#"{"id":"dlq","panel_id":3,"reducer":"max","op":">","threshold":0,"label":"DLQ not empty"}"#;
        let parsed: WatchRule = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.op, Comparison::Gt);
        assert_eq!(parsed.describe("DLQ depth"), "DLQ not empty");
        let unlabeled = rule(Reducer::Max, Comparison::Gt, 0.0);
        assert_eq!(unlabeled.describe("Errors"), "errors: max > 0");
    }
}
