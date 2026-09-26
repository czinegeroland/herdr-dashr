//! One tick of the dashboard pane's background loop.
//!
//! Each tick reads the current dashboard, classifies every panel, evaluates
//! the watch rules against real values, and reports to Herdr: a sidebar
//! token with the panel summary (DASHR-HERDR-007) and, when a watch
//! breaches, a blocked state plus a notification (DASHR-ALERT-002/003).
//! Values stay in this process; only counts and rule wording leave it.

use dashr_core::masking::Masker;
use dashr_core::session::{SessionRecord, SessionStore};
use dashr_core::watch::{self, Evaluation};
use dashr_grafana::Client;

use crate::status::{self, PanelStatus, Summary};

/// Where a tick's outcome goes. Implemented over the Herdr CLI by the pane,
/// and by a recorder in tests.
pub trait Reporter {
    fn token(&mut self, text: &str);
    fn blocked(&mut self, message: &str);
    fn clear(&mut self);
    fn notify(&mut self, title: &str, body: &str);
}

/// What one tick found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tick {
    pub summary: Summary,
    /// Every panel's status, for the text view. Carries no values.
    pub panels: Vec<PanelStatus>,
    /// Descriptions of rules that breached since the last tick.
    pub newly_breached: Vec<String>,
    /// Rule ids that cleared since the last tick.
    pub cleared: Vec<String>,
    /// Descriptions of every rule breached now.
    pub breached: Vec<String>,
    pub error: Option<String>,
}

/// Splits breach state into what is new and what cleared.
pub fn transitions(previous: &[String], current: &[String]) -> (Vec<String>, Vec<String>) {
    let new = current
        .iter()
        .filter(|id| !previous.contains(id))
        .cloned()
        .collect();
    let cleared = previous
        .iter()
        .filter(|id| !current.contains(id))
        .cloned()
        .collect();
    (new, cleared)
}

/// Runs one tick against Grafana and records breach state.
pub fn tick(
    record: &SessionRecord,
    client: &Client,
    masker: &Masker,
    store: &SessionStore,
) -> Tick {
    let model = match client.dashboard(&record.dashboard_uid) {
        Ok(model) => model,
        Err(error) => {
            return Tick {
                error: Some(error.to_string()),
                ..Tick::default()
            };
        }
    };
    let statuses = status::dashboard_status(client, &model, masker);
    let summary = Summary::of(&statuses);

    let mut state = store.load_watches(&record.session_id);
    let mut breached_ids = Vec::new();
    let mut descriptions = std::collections::BTreeMap::new();
    for rule in &state.rules {
        let Some(panel) = status::find_panel(&model, rule.panel_id) else {
            continue;
        };
        let title = panel
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("panel");
        let results = status::panel_results(client, &model, panel);
        if watch::evaluate(rule, &results) == Evaluation::Breached {
            breached_ids.push(rule.id.clone());
            descriptions.insert(rule.id.clone(), rule.describe(title));
        }
    }
    let (new, cleared) = transitions(&state.breached, &breached_ids);
    if new.len() + cleared.len() > 0 {
        state.breached = breached_ids.clone();
        let _ = store.save_watches(&record.session_id, &state);
    }
    Tick {
        summary,
        panels: statuses,
        newly_breached: new
            .iter()
            .filter_map(|id| descriptions.get(id).cloned())
            .collect(),
        cleared,
        breached: breached_ids
            .iter()
            .filter_map(|id| descriptions.get(id).cloned())
            .collect(),
        error: None,
    }
}

/// Sends a tick's outcome to a reporter.
pub fn report(tick: &Tick, reporter: &mut dyn Reporter, notify: bool) {
    if let Some(error) = &tick.error {
        reporter.token(&format!("grafana: {}", truncate(error, 40)));
        return;
    }
    let mut token = tick.summary.token();
    if !tick.breached.is_empty() {
        token.push_str(&format!(" · {} alert", tick.breached.len()));
    }
    reporter.token(&token);
    if !tick.newly_breached.is_empty() {
        let message = tick.newly_breached.join("; ");
        reporter.blocked(&message);
        if notify {
            reporter.notify("dashr alert", &message);
        }
    } else if tick.breached.is_empty() && !tick.cleared.is_empty() {
        reporter.clear();
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Vec<String>);

    impl Reporter for Recorder {
        fn token(&mut self, text: &str) {
            self.0.push(format!("token:{text}"));
        }
        fn blocked(&mut self, message: &str) {
            self.0.push(format!("blocked:{message}"));
        }
        fn clear(&mut self) {
            self.0.push("clear".into());
        }
        fn notify(&mut self, title: &str, body: &str) {
            self.0.push(format!("notify:{title}:{body}"));
        }
    }

    #[test]
    fn transitions_split_new_and_cleared() {
        let (new, cleared) = transitions(&["a".into(), "b".into()], &["b".into(), "c".into()]);
        assert_eq!(new, vec!["c"]);
        assert_eq!(cleared, vec!["a"]);
    }

    #[test]
    fn a_new_breach_blocks_and_notifies_once() {
        let tick = Tick {
            summary: Summary {
                ok: 3,
                empty: 0,
                error: 0,
            },
            newly_breached: vec!["DLQ not empty".into()],
            breached: vec!["DLQ not empty".into()],
            ..Tick::default()
        };
        let mut recorder = Recorder::default();
        report(&tick, &mut recorder, true);
        assert_eq!(
            recorder.0,
            vec![
                "token:3 ok · 1 alert",
                "blocked:DLQ not empty",
                "notify:dashr alert:DLQ not empty"
            ]
        );
        // The same breach on the next tick is not new: token only.
        let steady = Tick {
            newly_breached: vec![],
            ..tick
        };
        let mut recorder = Recorder::default();
        report(&steady, &mut recorder, true);
        assert_eq!(recorder.0, vec!["token:3 ok · 1 alert"]);
    }

    #[test]
    fn clearing_the_last_breach_returns_to_idle() {
        let tick = Tick {
            summary: Summary {
                ok: 1,
                empty: 1,
                error: 0,
            },
            cleared: vec!["dlq".into()],
            ..Tick::default()
        };
        let mut recorder = Recorder::default();
        report(&tick, &mut recorder, false);
        assert_eq!(recorder.0, vec!["token:1 ok · 1 empty", "clear"]);
    }

    #[test]
    fn grafana_errors_show_in_the_token() {
        let tick = Tick {
            error: Some("Grafana at http://127.0.0.1:1 is unreachable: connection refused".into()),
            ..Tick::default()
        };
        let mut recorder = Recorder::default();
        report(&tick, &mut recorder, true);
        assert_eq!(recorder.0.len(), 1);
        assert!(recorder.0[0].starts_with("token:grafana: "));
    }
}
