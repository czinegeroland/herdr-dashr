//! Turning CodePipeline and CloudFormation answers into an inventory.
//!
//! Pure functions of the JSON the `aws` CLI prints, so every shape is tested
//! against fixtures. Resource names and ARNs are infrastructure metadata, not
//! personal data, and are returned to the agent as-is (DASHR-AWS-005).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::url::PipelineRef;

/// A CloudFormation deployment action inside a pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StackTarget {
    pub stage: String,
    pub action: String,
    pub stack_name: String,
    pub region: String,
}

/// One stage's latest state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageState {
    pub name: String,
    pub status: String,
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Queue {
    pub name: String,
    pub dead_letter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateMachine {
    pub name: String,
    pub arn: String,
}

/// What a pipeline deploys, grouped by what dashboards care about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Resources {
    pub log_groups: Vec<String>,
    pub queues: Vec<Queue>,
    pub state_machines: Vec<StateMachine>,
    pub lambdas: Vec<String>,
    pub nested_stacks: Vec<String>,
    /// Counts of every other resource type, for context.
    pub other: BTreeMap<String, usize>,
}

impl Resources {
    /// Adds another stack's resources, keeping names unique.
    pub fn merge(&mut self, other: Resources) {
        for group in other.log_groups {
            if !self.log_groups.contains(&group) {
                self.log_groups.push(group);
            }
        }
        for queue in other.queues {
            if !self.queues.contains(&queue) {
                self.queues.push(queue);
            }
        }
        for machine in other.state_machines {
            if !self.state_machines.contains(&machine) {
                self.state_machines.push(machine);
            }
        }
        for lambda in other.lambdas {
            if !self.lambdas.contains(&lambda) {
                self.lambdas.push(lambda);
            }
        }
        self.nested_stacks.extend(other.nested_stacks);
        for (kind, count) in other.other {
            *self.other.entry(kind).or_default() += count;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.log_groups.is_empty()
            && self.queues.is_empty()
            && self.state_machines.is_empty()
            && self.lambdas.is_empty()
    }

    /// Log groups including each Lambda's implicit `/aws/lambda/<name>`.
    pub fn all_log_groups(&self) -> Vec<String> {
        let mut groups = self.log_groups.clone();
        for lambda in &self.lambdas {
            let group = format!("/aws/lambda/{lambda}");
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
        groups
    }
}

/// Everything the bootstrap found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inventory {
    pub pipeline: PipelineRef,
    pub stages: Vec<StageState>,
    pub stacks: Vec<StackTarget>,
    pub resources: Resources,
    pub warnings: Vec<String>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// CloudFormation deploy actions in a `get-pipeline` answer.
pub fn stack_targets(get_pipeline: &Value, default_region: &str) -> Vec<StackTarget> {
    let mut targets = Vec::new();
    let stages = get_pipeline
        .pointer("/pipeline/stages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for stage in &stages {
        let stage_name = text(stage, "name").unwrap_or_default();
        for action in stage
            .get("actions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let provider = action
                .pointer("/actionTypeId/provider")
                .and_then(Value::as_str)
                .unwrap_or("");
            if provider != "CloudFormation" {
                continue;
            }
            let Some(stack_name) = action
                .pointer("/configuration/StackName")
                .and_then(Value::as_str)
            else {
                continue;
            };
            let target = StackTarget {
                stage: stage_name.clone(),
                action: text(action, "name").unwrap_or_default(),
                stack_name: stack_name.to_owned(),
                region: text(action, "region").unwrap_or_else(|| default_region.to_owned()),
            };
            // A change set is usually created and executed by two actions on
            // the same stack; one entry is enough.
            if !targets.iter().any(|t: &StackTarget| {
                t.stack_name == target.stack_name && t.region == target.region
            }) {
                targets.push(target);
            }
        }
    }
    targets
}

/// Providers of deploy actions that are not CloudFormation, for a warning.
pub fn other_deploy_providers(get_pipeline: &Value) -> Vec<String> {
    let mut providers: Vec<String> = get_pipeline
        .pointer("/pipeline/stages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|stage| {
            stage
                .get("actions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|action| {
            action
                .pointer("/actionTypeId/category")
                .and_then(Value::as_str)
                == Some("Deploy")
        })
        .filter_map(|action| {
            action
                .pointer("/actionTypeId/provider")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|provider| provider != "CloudFormation")
        .collect();
    providers.sort();
    providers.dedup();
    providers
}

/// Stage states in a `get-pipeline-state` answer.
pub fn stage_states(state: &Value) -> Vec<StageState> {
    state
        .get("stageStates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|stage| StageState {
            name: text(stage, "stageName").unwrap_or_default(),
            status: stage
                .pointer("/latestExecution/status")
                .and_then(Value::as_str)
                .unwrap_or("NotRun")
                .to_owned(),
            execution_id: stage
                .pointer("/latestExecution/pipelineExecutionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect()
}

fn last_segment(value: &str, separator: char) -> String {
    value.rsplit(separator).next().unwrap_or(value).to_owned()
}

fn is_dead_letter(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("dlq") || lower.contains("deadletter") || lower.contains("dead-letter")
}

/// Classifies a `list-stack-resources` answer.
pub fn classify(list: &Value) -> Resources {
    let mut resources = Resources::default();
    let summaries = list
        .get("StackResourceSummaries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for summary in &summaries {
        let kind = text(summary, "ResourceType").unwrap_or_default();
        let Some(physical) = text(summary, "PhysicalResourceId").filter(|p| !p.is_empty()) else {
            continue;
        };
        let status = text(summary, "ResourceStatus").unwrap_or_default();
        if status.starts_with("DELETE") {
            continue;
        }
        match kind.as_str() {
            "AWS::Logs::LogGroup" => resources.log_groups.push(physical),
            "AWS::SQS::Queue" => {
                let name = last_segment(&physical, '/');
                resources.queues.push(Queue {
                    dead_letter: is_dead_letter(&name),
                    name,
                });
            }
            "AWS::StepFunctions::StateMachine" => resources.state_machines.push(StateMachine {
                name: last_segment(&physical, ':'),
                arn: physical,
            }),
            "AWS::Lambda::Function" => resources.lambdas.push(physical),
            "AWS::CloudFormation::Stack" => resources.nested_stacks.push(physical),
            _ => *resources.other.entry(kind).or_default() += 1,
        }
    }
    resources
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pipeline() -> Value {
        json!({"pipeline": {"name": "api", "stages": [
            {"name": "Source", "actions": [{"name": "Src", "actionTypeId": {"category": "Source", "provider": "CodeStarSourceConnection"}}]},
            {"name": "Deploy", "actions": [
                {"name": "CreateChangeSet", "actionTypeId": {"category": "Deploy", "provider": "CloudFormation"}, "configuration": {"StackName": "api-dev", "ActionMode": "CHANGE_SET_REPLACE"}},
                {"name": "ExecuteChangeSet", "actionTypeId": {"category": "Deploy", "provider": "CloudFormation"}, "configuration": {"StackName": "api-dev", "ActionMode": "CHANGE_SET_EXECUTE"}},
                {"name": "Workers", "region": "us-east-1", "actionTypeId": {"category": "Deploy", "provider": "CloudFormation"}, "configuration": {"StackName": "workers"}},
                {"name": "Ecs", "actionTypeId": {"category": "Deploy", "provider": "ECS"}, "configuration": {}}
            ]}
        ]}})
    }

    #[test]
    fn finds_cloudformation_stacks_once_each() {
        let targets = stack_targets(&pipeline(), "eu-west-1");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].stack_name, "api-dev");
        assert_eq!(targets[0].region, "eu-west-1");
        assert_eq!(targets[1].region, "us-east-1");
        assert_eq!(other_deploy_providers(&pipeline()), vec!["ECS"]);
    }

    #[test]
    fn stage_states_read_latest_execution() {
        let state = json!({"stageStates": [
            {"stageName": "Source", "latestExecution": {"status": "Succeeded", "pipelineExecutionId": "e1"}},
            {"stageName": "Deploy", "latestExecution": {"status": "Failed", "pipelineExecutionId": "e1"}},
            {"stageName": "Prod"}
        ]});
        let stages = stage_states(&state);
        assert_eq!(stages[1].status, "Failed");
        assert_eq!(stages[2].status, "NotRun");
        assert_eq!(stages[0].execution_id.as_deref(), Some("e1"));
    }

    #[test]
    fn classifies_resources() {
        let list = json!({"StackResourceSummaries": [
            {"ResourceType": "AWS::Logs::LogGroup", "PhysicalResourceId": "/app/api", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::SQS::Queue", "PhysicalResourceId": "https://sqs.eu-west-1.amazonaws.com/123456789012/orders", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::SQS::Queue", "PhysicalResourceId": "https://sqs.eu-west-1.amazonaws.com/123456789012/orders-DLQ", "ResourceStatus": "UPDATE_COMPLETE"},
            {"ResourceType": "AWS::StepFunctions::StateMachine", "PhysicalResourceId": "arn:aws:states:eu-west-1:123456789012:stateMachine:Fulfil", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::Lambda::Function", "PhysicalResourceId": "api-handler", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::CloudFormation::Stack", "PhysicalResourceId": "arn:aws:cloudformation:eu-west-1:1:stack/nested/x", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::IAM::Role", "PhysicalResourceId": "r1", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::IAM::Role", "PhysicalResourceId": "r2", "ResourceStatus": "CREATE_COMPLETE"},
            {"ResourceType": "AWS::Lambda::Function", "PhysicalResourceId": "gone", "ResourceStatus": "DELETE_COMPLETE"},
            {"ResourceType": "AWS::SQS::Queue", "ResourceStatus": "CREATE_FAILED"}
        ]});
        let resources = classify(&list);
        assert_eq!(resources.log_groups, vec!["/app/api"]);
        assert_eq!(resources.queues.len(), 2);
        assert!(!resources.queues[0].dead_letter);
        assert!(resources.queues[1].dead_letter);
        assert_eq!(resources.state_machines[0].name, "Fulfil");
        assert_eq!(resources.lambdas, vec!["api-handler"]);
        assert_eq!(resources.nested_stacks.len(), 1);
        assert_eq!(resources.other["AWS::IAM::Role"], 2);
        assert_eq!(
            resources.all_log_groups(),
            vec!["/app/api", "/aws/lambda/api-handler"]
        );
    }

    #[test]
    fn merge_deduplicates() {
        let mut a = classify(&json!({"StackResourceSummaries": [
            {"ResourceType": "AWS::Lambda::Function", "PhysicalResourceId": "f", "ResourceStatus": "CREATE_COMPLETE"}
        ]}));
        let b = a.clone();
        a.merge(b);
        assert_eq!(a.lambdas.len(), 1);
        assert!(!a.is_empty());
        assert!(Resources::default().is_empty());
    }
}
