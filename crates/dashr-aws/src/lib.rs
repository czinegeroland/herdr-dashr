//! CodePipeline bootstrap: from a console URL to a first dashboard.
//!
//! `GetPipeline` names the CloudFormation stacks the pipeline deploys;
//! `ListStackResources` names what is in them — log groups, queues and their
//! dead-letter queues, state machines, Lambda functions. From that inventory
//! [`propose`] builds a dashboard with no model involved, so the human sees
//! something useful before the agent has read a line (docs/DESIGN.md,
//! "CodePipeline bootstrap").
//!
//! AWS is reached through the `aws` CLI, never an SDK: dashr then holds no
//! credentials of its own and inherits SSO, profiles and MFA exactly as the
//! user configured them (requirement DASHR-AWS-006, decision DEC-014).

pub mod cli;
pub mod inventory;
pub mod propose;
pub mod url;

pub use inventory::Inventory;
pub use url::PipelineRef;
