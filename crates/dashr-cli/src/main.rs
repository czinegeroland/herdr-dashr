//! `dashr`: end-to-end testing by traces.
//!
//! A session (a Herdr pane, or `dashr serve`) runs Jaeger and a live
//! viewer. The human's AI session instruments a feature with OpenTelemetry,
//! writes the flow the feature should produce for the human to review,
//! connects the traces of every service it runs on, and checks the run
//! against the flow — all through these commands.

mod agent;
mod client;
mod doctor;
mod herdr_cmds;
mod pane;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use dashr_core::Config;
use dashr_runtime::{Paths, SessionRecord};

#[derive(Parser)]
#[command(
    name = "dashr",
    version,
    about = "End-to-end testing by traces: one live sequence diagram of every service's spans, checked against the flow you expect",
    after_help = "Exit codes: 0 ok or passed, 1 the flow failed, 2 bad command line, 3 the human asked for changes, 4 timed out, 5 other error."
)]
struct Cli {
    /// Configuration directory (default: HERDR_PLUGIN_CONFIG_DIR, then the platform default).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// State directory (default: DASHR_STATE_DIR, then the platform default).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// The session: its id or its Herdr pane id (default: DASHR_SESSION, then the running one).
    #[arg(long, global = true)]
    session: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a session in the foreground, without Herdr.
    Serve,
    /// Wait for a session (the pane `herdr plugin pane open` created) and print its endpoints.
    Wait {
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// The session: endpoints, trace counts, flows, sources.
    Status,
    /// The OTEL_* variables that send a local service's traces to the session.
    Env {
        /// sh, powershell, cmd or json.
        #[arg(long, default_value = "sh")]
        shell: String,
    },
    /// Recent traces, newest first (masked).
    Traces {
        /// Only traces that ended within this window: 30s, 10m, 2h, 1d.
        #[arg(long)]
        since: Option<String>,
        /// A glob on a service name.
        #[arg(long)]
        service: Option<String>,
        /// A glob on a span name.
        #[arg(long)]
        name: Option<String>,
        /// key=value an attribute of some span must have (repeatable; `*` for any value).
        #[arg(long = "attr")]
        attrs: Vec<String>,
        /// Only traces with an error span.
        #[arg(long)]
        errors: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One trace as a sequence (masked); --json for every span.
    Trace {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Flows: the trace a feature should produce, reviewed by the human, checked against runs.
    Flow {
        #[command(subcommand)]
        command: FlowCommand,
    },
    /// Pull sources: commands that fetch traces from wherever the services run.
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    /// Import traces once from a file or stdin (`-`).
    Ingest {
        file: PathBuf,
        /// auto, otlp, otlp-proto, xray, zipkin, jaeger, appinsights, cloudtrace.
        #[arg(long, default_value = "auto")]
        format: String,
        #[arg(long, default_value = "import")]
        source: String,
    },
    /// Every session this machine knows, and whether it answers.
    Sessions,
    /// Check Docker, Herdr and the cloud CLIs.
    Doctor,
    /// Install the `dashr` command globally with npm, at this version.
    Global {
        #[command(subcommand)]
        command: GlobalCommand,
    },
    /// Entrypoints Herdr invokes from the plugin manifest.
    #[command(hide = true)]
    Herdr {
        #[command(subcommand)]
        command: HerdrCommand,
    },
}

#[derive(Subcommand)]
enum FlowCommand {
    /// Add or replace a flow from a JSON file (`-` for stdin); a changed flow needs review again.
    Set {
        #[arg(default_value = "-")]
        file: PathBuf,
    },
    /// Every flow with its review and status.
    List,
    /// A flow, its review and its verdict (masked).
    Show { name: String },
    /// Count only runs from now on.
    Arm { name: String },
    /// Wait for the human's review (--review) or for the verdict.
    Wait {
        name: String,
        #[arg(long)]
        review: bool,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Remove a flow.
    Rm { name: String },
}

#[derive(Subcommand)]
enum SourceCommand {
    /// Add a source: tried once now, then run every --every seconds. The command gets
    /// DASHR_SINCE/DASHR_UNTIL (Unix seconds, also _MS and _ISO) for the window to read.
    Add {
        name: String,
        #[arg(long, default_value_t = 15)]
        every: u64,
        /// auto, otlp, otlp-proto, xray, zipkin, jaeger, appinsights, cloudtrace.
        #[arg(long, default_value = "auto")]
        format: String,
        /// Minutes the first run reads back.
        #[arg(long)]
        lookback: Option<u64>,
        /// Keep the source when its trial run fails (a login still to come).
        #[arg(long)]
        keep_on_error: bool,
        /// The command, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Sources and their health.
    List,
    /// Stop and remove a source.
    Rm { name: String },
}

#[derive(Subcommand)]
enum GlobalCommand {
    Install {
        /// Report a failure instead of failing (the plugin's build step).
        #[arg(long)]
        best_effort: bool,
    },
}

#[derive(Subcommand)]
enum HerdrCommand {
    /// Print the generated herdr-plugin.toml.
    Manifest,
    Startup,
    Event {
        name: String,
    },
    Action {
        id: String,
    },
    Pane {
        id: String,
    },
}

/// The OTEL_* settings that send a service's traces to the session.
pub fn jaeger_env(record: &SessionRecord) -> Value {
    json!({
        "OTEL_EXPORTER_OTLP_ENDPOINT": record.otlp_http(),
        "OTEL_EXPORTER_OTLP_PROTOCOL": "http/protobuf",
        "OTEL_TRACES_EXPORTER": "otlp",
        "OTEL_TRACES_SAMPLER": "always_on",
        "OTEL_BSP_SCHEDULE_DELAY": "500",
        "OTEL_METRICS_EXPORTER": "none",
        "OTEL_LOGS_EXPORTER": "none",
    })
}

fn global_install_argv(windows: bool) -> Vec<String> {
    let npm = [
        "npm",
        "install",
        "-g",
        "--no-audit",
        "--no-fund",
        concat!("herdr-dashr@", env!("CARGO_PKG_VERSION")),
    ];
    let prefix: &[&str] = if windows { &["cmd", "/c"] } else { &[] };
    prefix
        .iter()
        .chain(npm.iter())
        .map(|a| (*a).to_owned())
        .collect()
}

/// Puts `dashr` on the human's PATH for their AI session. With
/// `best_effort` a failure is reported and the plugin install goes on.
fn global_install(best_effort: bool) -> Result<(), String> {
    let argv = global_install_argv(cfg!(windows));
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .status();
    let failure = match status {
        Ok(status) if status.success() => {
            println!(
                "dashr: installed the dashr command globally ({})",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        Ok(status) => format!("npm exited with {status}"),
        Err(error) => format!("could not run npm: {error}"),
    };
    let message = format!(
        "the dashr command was not installed globally ({failure}); run `npm install -g herdr-dashr@{}` yourself",
        env!("CARGO_PKG_VERSION")
    );
    if best_effort {
        println!("dashr: {message}");
        Ok(())
    } else {
        Err(message)
    }
}

fn run(cli: Cli) -> agent::Outcome {
    let paths = Paths::resolve(cli.config_dir, cli.state_dir);
    let session = cli.session.as_deref();
    let config = || Config::load_from_dir(&paths.config_dir);
    let done = |result: Result<(), String>| result.map(|()| 0u8);
    match cli.command {
        Command::Serve => done(pane::serve(&paths, &config()?, cli.session.clone())),
        Command::Wait { timeout } => agent::wait(&paths, session, timeout),
        Command::Status => agent::status(&paths, session),
        Command::Env { shell } => agent::env(&paths, session, &shell),
        Command::Traces {
            since,
            service,
            name,
            attrs,
            errors,
            limit,
        } => agent::traces(&paths, session, since, service, name, attrs, errors, limit),
        Command::Trace { id, json } => agent::trace(&paths, session, &id, json),
        Command::Flow { command } => match command {
            FlowCommand::Set { file } => agent::flow_set(&paths, session, &file),
            FlowCommand::List => agent::flow_list(&paths, session),
            FlowCommand::Show { name } => agent::flow_show(&paths, session, &name),
            FlowCommand::Arm { name } => agent::flow_arm(&paths, session, &name),
            FlowCommand::Wait {
                name,
                review,
                timeout,
            } => agent::flow_wait(&paths, session, &name, review, timeout),
            FlowCommand::Rm { name } => agent::flow_remove(&paths, session, &name),
        },
        Command::Source { command } => match command {
            SourceCommand::Add {
                name,
                every,
                format,
                lookback,
                keep_on_error,
                command,
            } => agent::source_add(
                &paths,
                session,
                &name,
                every,
                &format,
                lookback,
                keep_on_error,
                command,
            ),
            SourceCommand::List => agent::source_list(&paths, session),
            SourceCommand::Rm { name } => agent::source_remove(&paths, session, &name),
        },
        Command::Ingest {
            file,
            format,
            source,
        } => agent::ingest(&paths, session, &file, &format, &source),
        Command::Sessions => agent::sessions(&paths),
        Command::Doctor => done(doctor::run(&paths)),
        Command::Global {
            command: GlobalCommand::Install { best_effort },
        } => done(global_install(best_effort)),
        Command::Herdr { command } => done(match command {
            HerdrCommand::Manifest => {
                print!(
                    "{}",
                    dashr_herdr::manifest::render(env!("CARGO_PKG_VERSION"))
                );
                Ok(())
            }
            HerdrCommand::Startup => herdr_cmds::startup(&paths, &config()?),
            HerdrCommand::Event { name } if name == "pane-closed" => {
                herdr_cmds::pane_closed(&paths, &config()?)
            }
            HerdrCommand::Event { name } => Err(format!("unknown event {name:?}")),
            HerdrCommand::Action { id } => herdr_cmds::action(&id),
            HerdrCommand::Pane { id } if id == "traces" => pane::traces(&paths, &config()?),
            HerdrCommand::Pane { id } if id == "doctor" => {
                let result = doctor::run(&paths);
                println!("\nPress Enter to close.");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                result
            }
            HerdrCommand::Pane { id } => Err(format!("unknown pane {id:?}")),
        }),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("dashr: {error}");
            ExitCode::from(5)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_this_version_through_cmd_on_windows() {
        let pinned = format!("herdr-dashr@{}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            global_install_argv(false),
            [
                "npm",
                "install",
                "-g",
                "--no-audit",
                "--no-fund",
                pinned.as_str()
            ]
        );
        assert_eq!(global_install_argv(true)[..3], ["cmd", "/c", "npm"]);
    }

    #[test]
    fn the_command_line_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from([
            "dashr",
            "source",
            "add",
            "xray",
            "--format",
            "xray",
            "--every",
            "20",
            "--",
            "sh",
            "-c",
            "aws xray get-trace-summaries",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Command::Source { command: SourceCommand::Add { ref command, .. } } if command.len() == 3)
        );
    }
}
