//! A first dashboard from an inventory, with no model involved.
//!
//! Queries follow the CloudWatch datasource's JSON model: metric queries
//! (`queryMode: "Metrics"`, namespace, metric, dimensions, statistic) and
//! Logs Insights queries (`queryMode: "Logs"`, log group names, expression).
//! The layout puts what most often explains a failed deploy first: pipeline
//! stage state, then errors in logs, then queues backing up, then
//! executions failing (DASHR-AWS-004).

use serde_json::{Value, json};

use crate::inventory::Inventory;

/// Log groups per Logs Insights panel. CloudWatch accepts up to 50; a panel
/// over many groups is slow and hard to read.
const LOG_GROUPS_PER_PANEL: usize = 10;
/// Cap on per-resource metric panels, so a stack of 40 Lambdas stays usable.
const MAX_RESOURCE_PANELS: usize = 12;

fn datasource(uid: &str) -> Value {
    json!({"type": "cloudwatch", "uid": uid})
}

fn metric_query(
    ref_id: &str,
    uid: &str,
    region: &str,
    namespace: &str,
    metric: &str,
    dimension: (&str, &str),
    statistic: &str,
) -> Value {
    json!({
        "refId": ref_id,
        "datasource": datasource(uid),
        "queryMode": "Metrics",
        "metricQueryType": 0,
        "metricEditorMode": 0,
        "region": region,
        "namespace": namespace,
        "metricName": metric,
        "dimensions": {dimension.0: [dimension.1]},
        "statistic": statistic,
        "period": "60",
        "matchExact": true,
        "id": "",
        "expression": "",
        "label": format!("{} {}", dimension.1, metric)
    })
}

struct Layout {
    panels: Vec<Value>,
    next_id: i64,
    y: i64,
    x: i64,
}

impl Layout {
    fn row(&mut self, title: &str) {
        if self.x != 0 {
            self.y += 8;
            self.x = 0;
        }
        self.panels.push(json!({
            "id": self.next_id, "type": "row", "title": title, "collapsed": false,
            "gridPos": {"x": 0, "y": self.y, "w": 24, "h": 1}, "panels": []
        }));
        self.next_id += 1;
        self.y += 1;
    }

    fn add(&mut self, mut panel: Value, width: i64, height: i64) {
        if self.x + width > 24 {
            self.x = 0;
            self.y += height;
        }
        panel["id"] = json!(self.next_id);
        panel["gridPos"] = json!({"x": self.x, "y": self.y, "w": width, "h": height});
        self.panels.push(panel);
        self.next_id += 1;
        self.x += width;
        if self.x >= 24 {
            self.x = 0;
            self.y += height;
        }
    }
}

fn stage_markdown(inventory: &Inventory) -> String {
    let mut text = format!(
        "**Pipeline** `{}` in `{}`\n\n",
        inventory.pipeline.name, inventory.pipeline.region
    );
    if inventory.stages.is_empty() {
        text.push_str("_Stage state unavailable._\n");
    } else {
        text.push_str("| Stage | Status |\n|---|---|\n");
        for stage in &inventory.stages {
            let mark = match stage.status.as_str() {
                "Succeeded" => "✅",
                "Failed" => "❌",
                "InProgress" => "⏳",
                _ => "·",
            };
            text.push_str(&format!("| {} | {mark} {} |\n", stage.name, stage.status));
        }
    }
    if !inventory.stacks.is_empty() {
        let stacks: Vec<String> = inventory
            .stacks
            .iter()
            .map(|stack| format!("`{}`", stack.stack_name))
            .collect();
        text.push_str(&format!("\nStacks: {}\n", stacks.join(", ")));
    }
    for warning in &inventory.warnings {
        text.push_str(&format!("\n> {warning}\n"));
    }
    text.push_str("\n_Stage state is a snapshot from when the pane opened._");
    text
}

/// Builds the first dashboard. `uid` is the session's CloudWatch datasource.
pub fn propose(inventory: &Inventory, datasource_uid: &str) -> Value {
    let region = inventory.pipeline.region.as_str();
    let resources = &inventory.resources;
    let mut layout = Layout {
        panels: Vec::new(),
        next_id: 1,
        y: 0,
        x: 0,
    };

    layout.add(
        json!({"type": "text", "title": "Pipeline", "options": {"mode": "markdown", "content": stage_markdown(inventory)}}),
        24,
        7,
    );

    let log_groups = resources.all_log_groups();
    if !log_groups.is_empty() {
        layout.row("Logs");
        for (index, chunk) in log_groups.chunks(LOG_GROUPS_PER_PANEL).enumerate() {
            let suffix = if log_groups.len() > LOG_GROUPS_PER_PANEL {
                format!(" ({})", index + 1)
            } else {
                String::new()
            };
            layout.add(
                json!({
                    "type": "timeseries",
                    "title": format!("Error lines per minute{suffix}"),
                    "datasource": datasource(datasource_uid),
                    "targets": [{
                        "refId": "A",
                        "datasource": datasource(datasource_uid),
                        "queryMode": "Logs",
                        "region": region,
                        "logGroupNames": chunk,
                        "expression": "filter @message like /(?i)(error|exception|fail|timeout)/ | stats count(*) as errors by bin(1m)",
                        "statsGroups": ["bin(1m)"]
                    }]
                }),
                12,
                8,
            );
            layout.add(
                json!({
                    "type": "logs",
                    "title": format!("Recent errors{suffix}"),
                    "datasource": datasource(datasource_uid),
                    "options": {"showTime": true, "wrapLogMessage": true, "sortOrder": "Descending"},
                    "targets": [{
                        "refId": "A",
                        "datasource": datasource(datasource_uid),
                        "queryMode": "Logs",
                        "region": region,
                        "logGroupNames": chunk,
                        "expression": "fields @timestamp, @message, @logStream | filter @message like /(?i)(error|exception|fail|timeout)/ | sort @timestamp desc | limit 100"
                    }]
                }),
                12,
                8,
            );
        }
    }

    if !resources.queues.is_empty() {
        layout.row("Queues");
        for queue in resources.queues.iter().take(MAX_RESOURCE_PANELS) {
            let dimension = ("QueueName", queue.name.as_str());
            let (title, targets) = if queue.dead_letter {
                (
                    format!("DLQ {} — messages", queue.name),
                    vec![metric_query(
                        "A",
                        datasource_uid,
                        region,
                        "AWS/SQS",
                        "ApproximateNumberOfMessagesVisible",
                        dimension,
                        "Maximum",
                    )],
                )
            } else {
                (
                    format!("{} — depth and age", queue.name),
                    vec![
                        metric_query(
                            "A",
                            datasource_uid,
                            region,
                            "AWS/SQS",
                            "ApproximateNumberOfMessagesVisible",
                            dimension,
                            "Maximum",
                        ),
                        metric_query(
                            "B",
                            datasource_uid,
                            region,
                            "AWS/SQS",
                            "ApproximateAgeOfOldestMessage",
                            dimension,
                            "Maximum",
                        ),
                    ],
                )
            };
            let panel_type = if queue.dead_letter {
                "stat"
            } else {
                "timeseries"
            };
            let mut panel = json!({
                "type": panel_type,
                "title": title,
                "datasource": datasource(datasource_uid),
                "targets": targets
            });
            if queue.dead_letter {
                panel["fieldConfig"] = json!({"defaults": {"thresholds": {"mode": "absolute", "steps": [
                    {"color": "green", "value": null}, {"color": "red", "value": 1}
                ]}}});
                panel["options"] =
                    json!({"colorMode": "background", "reduceOptions": {"calcs": ["lastNotNull"]}});
            }
            layout.add(panel, if queue.dead_letter { 6 } else { 12 }, 8);
        }
    }

    if !resources.state_machines.is_empty() {
        layout.row("Step Functions");
        for machine in resources.state_machines.iter().take(MAX_RESOURCE_PANELS) {
            let dimension = ("StateMachineArn", machine.arn.as_str());
            layout.add(
                json!({
                    "type": "timeseries",
                    "title": format!("{} — executions", machine.name),
                    "datasource": datasource(datasource_uid),
                    "targets": [
                        metric_query("A", datasource_uid, region, "AWS/States", "ExecutionsStarted", dimension, "Sum"),
                        metric_query("B", datasource_uid, region, "AWS/States", "ExecutionsFailed", dimension, "Sum"),
                        metric_query("C", datasource_uid, region, "AWS/States", "ExecutionsTimedOut", dimension, "Sum"),
                    ]
                }),
                12,
                8,
            );
        }
    }

    if !resources.lambdas.is_empty() {
        layout.row("Lambda");
        for function in resources.lambdas.iter().take(MAX_RESOURCE_PANELS) {
            let dimension = ("FunctionName", function.as_str());
            layout.add(
                json!({
                    "type": "timeseries",
                    "title": format!("{function} — errors and throttles"),
                    "datasource": datasource(datasource_uid),
                    "targets": [
                        metric_query("A", datasource_uid, region, "AWS/Lambda", "Errors", dimension, "Sum"),
                        metric_query("B", datasource_uid, region, "AWS/Lambda", "Throttles", dimension, "Sum"),
                        metric_query("C", datasource_uid, region, "AWS/Lambda", "Invocations", dimension, "Sum"),
                    ]
                }),
                12,
                8,
            );
        }
    }

    json!({
        "title": format!("{} ({})", inventory.pipeline.name, region),
        "tags": ["dashr", "codepipeline"],
        "time": {"from": "now-3h", "to": "now"},
        "panels": layout.panels
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Queue, Resources, StackTarget, StageState, StateMachine};
    use crate::url::PipelineRef;

    fn inventory(resources: Resources) -> Inventory {
        Inventory {
            pipeline: PipelineRef {
                region: "eu-west-1".into(),
                name: "api".into(),
                execution_id: None,
            },
            stages: vec![StageState {
                name: "Deploy".into(),
                status: "Failed".into(),
                execution_id: Some("e1".into()),
            }],
            stacks: vec![StackTarget {
                stage: "Deploy".into(),
                action: "Cfn".into(),
                stack_name: "api-dev".into(),
                region: "eu-west-1".into(),
            }],
            resources,
            warnings: vec!["deploy provider ECS is not inspected".into()],
        }
    }

    #[test]
    fn empty_inventory_still_yields_the_pipeline_panel() {
        let dashboard = propose(&inventory(Resources::default()), "cw");
        let panels = dashboard["panels"].as_array().unwrap();
        assert_eq!(panels.len(), 1);
        let content = panels[0]["options"]["content"].as_str().unwrap();
        assert!(content.contains("❌ Failed"));
        assert!(content.contains("api-dev"));
        assert!(content.contains("ECS"));
    }

    #[test]
    fn every_resource_kind_gets_panels_with_valid_queries() {
        let resources = Resources {
            log_groups: vec!["/app/api".into()],
            queues: vec![
                Queue {
                    name: "jobs".into(),
                    dead_letter: false,
                },
                Queue {
                    name: "jobs-dlq".into(),
                    dead_letter: true,
                },
            ],
            state_machines: vec![StateMachine {
                name: "Fulfil".into(),
                arn: "arn:aws:states:eu-west-1:1:stateMachine:Fulfil".into(),
            }],
            lambdas: vec!["handler".into()],
            ..Resources::default()
        };
        let dashboard = propose(&inventory(resources), "cw");
        let panels = dashboard["panels"].as_array().unwrap();
        let titles: Vec<&str> = panels.iter().filter_map(|p| p["title"].as_str()).collect();
        for expected in [
            "Logs",
            "Queues",
            "Step Functions",
            "Lambda",
            "DLQ jobs-dlq — messages",
        ] {
            assert!(
                titles.contains(&expected),
                "{expected} missing from {titles:?}"
            );
        }
        let logs = panels.iter().find(|p| p["type"] == "logs").unwrap();
        assert_eq!(
            logs["targets"][0]["logGroupNames"],
            json!(["/app/api", "/aws/lambda/handler"])
        );
        // Every datasource reference points at the session's CloudWatch.
        let text = dashboard.to_string();
        assert!(!text.contains("\"uid\":\"\""));
        let known = vec!["cw".to_owned(), "dashr-testdata".to_owned()];
        let normalized = dashr_core::dashboard::normalize(
            &dashboard,
            &dashr_core::dashboard::Pins {
                uid: "u",
                refresh: "5s",
                time_from: "now-1h",
                known_datasources: &known,
            },
        )
        .expect("proposal passes validation");
        assert!(normalized.warnings.is_empty(), "{:?}", normalized.warnings);
        // No two panels overlap.
        let mut cells = std::collections::HashSet::new();
        for panel in panels {
            let grid = &panel["gridPos"];
            let (x, y, w, h) = (
                grid["x"].as_i64().unwrap(),
                grid["y"].as_i64().unwrap(),
                grid["w"].as_i64().unwrap(),
                grid["h"].as_i64().unwrap(),
            );
            for cx in x..x + w {
                for cy in y..y + h {
                    assert!(
                        cells.insert((cx, cy)),
                        "overlap at {cx},{cy} in {}",
                        panel["title"]
                    );
                }
            }
        }
    }

    #[test]
    fn many_log_groups_are_chunked() {
        let resources = Resources {
            log_groups: (0..23).map(|i| format!("/g/{i}")).collect(),
            ..Resources::default()
        };
        let dashboard = propose(&inventory(resources), "cw");
        let logs: Vec<&Value> = dashboard["panels"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["type"] == "logs")
            .collect();
        assert_eq!(logs.len(), 3);
        assert_eq!(
            logs[2]["targets"][0]["logGroupNames"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
}
