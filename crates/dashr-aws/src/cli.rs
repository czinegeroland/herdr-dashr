//! The `aws` CLI: discovery calls and short-lived credential export.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::inventory::{self, Inventory, Resources};
use crate::url::PipelineRef;

#[derive(Debug, thiserror::Error)]
pub enum AwsError {
    #[error("`{0}` was not found; install the AWS CLI v2")]
    Missing(String),
    #[error("aws {command} failed: {message}")]
    Failed { command: String, message: String },
    #[error("aws {command} printed something that is not JSON")]
    NotJson { command: String },
}

/// Credential variables `export-credentials` may print.
pub const CREDENTIAL_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_CREDENTIAL_EXPIRATION",
];

/// Parses `aws configure export-credentials --format env-no-export`.
///
/// Only known credential variables are kept, so nothing unexpected is ever
/// forwarded to the container.
pub fn parse_credentials(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("export ").or(Some(line.trim())))
        .filter_map(|line| line.split_once('='))
        .filter(|(key, _)| CREDENTIAL_VARS.contains(key))
        .map(|(key, value)| (key.to_owned(), value.trim_matches('"').to_owned()))
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

/// Nested stacks are followed this deep. Real deployments rarely go past
/// two; a cycle cannot happen, but a bound costs nothing.
const MAX_NESTING: usize = 3;

#[derive(Debug, Clone)]
pub struct AwsCli {
    bin: String,
    profile: Option<String>,
}

impl AwsCli {
    pub fn new(bin: &str, profile: Option<&str>) -> Self {
        Self {
            bin: bin.to_owned(),
            profile: profile.map(str::to_owned),
        }
    }

    fn run(&self, args: &[&str], region: Option<&str>) -> Result<String, AwsError> {
        let mut command = Command::new(&self.bin);
        command.args(args);
        if let Some(region) = region {
            command.args(["--region", region]);
        }
        if let Some(profile) = &self.profile {
            command.args(["--profile", profile]);
        }
        let name = args.iter().take(2).copied().collect::<Vec<_>>().join(" ");
        let output = command
            .env("AWS_PAGER", "")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => AwsError::Missing(self.bin.clone()),
                _ => AwsError::Failed {
                    command: name.clone(),
                    message: error.to_string(),
                },
            })?;
        if !output.status.success() {
            return Err(AwsError::Failed {
                command: name,
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(500)
                    .collect(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn json(&self, args: &[&str], region: &str) -> Result<Value, AwsError> {
        let mut full: Vec<&str> = args.to_vec();
        full.extend(["--output", "json"]);
        let text = self.run(&full, Some(region))?;
        serde_json::from_str(&text).map_err(|_| AwsError::NotJson {
            command: args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
        })
    }

    /// Short-lived credentials for the container, when the CLI can export
    /// them. `Ok(empty)` means the container falls back to nothing, and
    /// CloudWatch panels will say so in `panel_status`.
    pub fn export_credentials(&self) -> Result<BTreeMap<String, String>, AwsError> {
        let text = self.run(
            &[
                "configure",
                "export-credentials",
                "--format",
                "env-no-export",
            ],
            None,
        )?;
        Ok(parse_credentials(&text))
    }

    fn stack_resources(
        &self,
        stack: &str,
        region: &str,
        depth: usize,
        warnings: &mut Vec<String>,
    ) -> Resources {
        match self.json(
            &[
                "cloudformation",
                "list-stack-resources",
                "--stack-name",
                stack,
            ],
            region,
        ) {
            Ok(list) => {
                let mut resources = inventory::classify(&list);
                if depth < MAX_NESTING {
                    for nested in std::mem::take(&mut resources.nested_stacks) {
                        let more = self.stack_resources(&nested, region, depth + 1, warnings);
                        resources.merge(more);
                    }
                }
                resources
            }
            Err(error) => {
                warnings.push(format!("stack {stack}: {error}"));
                Resources::default()
            }
        }
    }

    /// Runs the whole bootstrap for one pipeline.
    pub fn discover(&self, pipeline: &PipelineRef) -> Result<Inventory, AwsError> {
        let definition = self.json(
            &["codepipeline", "get-pipeline", "--name", &pipeline.name],
            &pipeline.region,
        )?;
        let mut warnings = Vec::new();
        let stages = match self.json(
            &[
                "codepipeline",
                "get-pipeline-state",
                "--name",
                &pipeline.name,
            ],
            &pipeline.region,
        ) {
            Ok(state) => inventory::stage_states(&state),
            Err(error) => {
                warnings.push(format!("pipeline state: {error}"));
                Vec::new()
            }
        };
        let stacks = inventory::stack_targets(&definition, &pipeline.region);
        if stacks.is_empty() {
            warnings.push("the pipeline has no CloudFormation deploy action".to_owned());
        }
        for provider in inventory::other_deploy_providers(&definition) {
            warnings.push(format!(
                "deploy provider {provider} is not inspected; add its resources by hand"
            ));
        }
        let mut resources = Resources::default();
        for stack in &stacks {
            let found = self.stack_resources(&stack.stack_name, &stack.region, 0, &mut warnings);
            resources.merge(found);
        }
        Ok(Inventory {
            pipeline: pipeline.clone(),
            stages,
            stacks,
            resources,
            warnings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_keep_only_known_variables() {
        let output = "AWS_ACCESS_KEY_ID=ASIAEXAMPLE\nAWS_SECRET_ACCESS_KEY=abc\nexport AWS_SESSION_TOKEN=\"tok\"\nPATH=/evil\nAWS_CREDENTIAL_EXPIRATION=\n";
        let credentials = parse_credentials(output);
        assert_eq!(credentials.len(), 3);
        assert_eq!(credentials["AWS_SESSION_TOKEN"], "tok");
        assert!(!credentials.contains_key("PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn discover_follows_nested_stacks_through_a_fake_cli() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("aws");
        // Written as a plain file and copied into place by `cp`: a write
        // handle held here could leak into a parallel test's fork and make
        // running the script fail with ETXTBSY.
        let source = dir.path().join("aws.sh");
        std::fs::write(
            &source,
            r#"#!/bin/sh
case "$1 $2" in
  "codepipeline get-pipeline")
    echo '{"pipeline":{"stages":[{"name":"Deploy","actions":[{"name":"Cfn","actionTypeId":{"category":"Deploy","provider":"CloudFormation"},"configuration":{"StackName":"root"}}]}]}}' ;;
  "codepipeline get-pipeline-state")
    echo '{"stageStates":[{"stageName":"Deploy","latestExecution":{"status":"Failed","pipelineExecutionId":"e9"}}]}' ;;
  "cloudformation list-stack-resources")
    if [ "$4" = "root" ]; then
      echo '{"StackResourceSummaries":[{"ResourceType":"AWS::CloudFormation::Stack","PhysicalResourceId":"child","ResourceStatus":"CREATE_COMPLETE"},{"ResourceType":"AWS::Lambda::Function","PhysicalResourceId":"fn-a","ResourceStatus":"CREATE_COMPLETE"}]}'
    else
      echo '{"StackResourceSummaries":[{"ResourceType":"AWS::SQS::Queue","PhysicalResourceId":"https://sqs/1/jobs-dlq","ResourceStatus":"CREATE_COMPLETE"}]}'
    fi ;;
  *) echo "unexpected $*" >&2; exit 2 ;;
esac
"#,
        )
        .unwrap();
        let copied = std::process::Command::new("cp")
            .arg(&source)
            .arg(&script)
            .status()
            .unwrap();
        assert!(copied.success());
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = AwsCli::new(script.to_str().unwrap(), None);
        let pipeline = PipelineRef {
            region: "eu-west-1".into(),
            name: "api".into(),
            execution_id: None,
        };
        let inventory = cli.discover(&pipeline).unwrap();
        assert_eq!(inventory.stages[0].status, "Failed");
        assert_eq!(inventory.resources.lambdas, vec!["fn-a"]);
        assert_eq!(inventory.resources.queues[0].name, "jobs-dlq");
        assert!(inventory.resources.queues[0].dead_letter);
        assert!(inventory.warnings.is_empty(), "{:?}", inventory.warnings);
    }

    #[test]
    fn missing_cli_is_reported() {
        let cli = AwsCli::new("definitely-not-aws-xyz", None);
        assert!(matches!(
            cli.export_credentials(),
            Err(AwsError::Missing(_))
        ));
    }
}
