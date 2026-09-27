//! The `dashr` binary: Herdr plugin entrypoints and standalone commands.

mod agent;
mod doctor;
mod herdr_cmds;
mod pane;
mod standalone;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "dashr",
    version,
    about = "Live Grafana dashboards in a Herdr pane"
)]
struct Cli {
    /// Configuration directory (default: HERDR_PLUGIN_CONFIG_DIR, then XDG).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// State directory (default: DASHR_STATE_DIR, then the platform default).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Entrypoints Herdr invokes from the plugin manifest.
    Herdr {
        #[command(subcommand)]
        command: HerdrCommand,
    },
    /// Serve the MCP tools for one session over stdio.
    Mcp {
        #[arg(long, env = "DASHR_SESSION")]
        session: String,
        /// The herdr executable, for opening new dashboard tabs.
        #[arg(long)]
        herdr_bin: Option<String>,
    },
    /// Wait for a dashboard pane's Grafana, then print the session and the
    /// agent's briefing as JSON. For the AI session that opened the pane.
    Wait {
        /// A session id or its dashboard pane id (what `herdr plugin pane
        /// open` printed); defaults to $DASHR_SESSION, else the only session.
        #[arg(long)]
        session: Option<String>,
        /// Seconds to wait; the first start pulls the Grafana image.
        #[arg(long, default_value_t = 180)]
        timeout: u64,
    },
    /// Call one dashboard tool (the MCP tools, as a command) and print its
    /// JSON answer, masked like every tool answer. Without a name, list the
    /// tools and their arguments.
    Tool {
        /// e.g. list_datasources, probe_query, apply_dashboard, panel_status.
        name: Option<String>,
        /// A session id or its dashboard pane id; defaults to $DASHR_SESSION,
        /// else the only session.
        #[arg(long)]
        session: Option<String>,
        /// The tool's arguments as a JSON object.
        #[arg(long, conflicts_with = "args_file")]
        args: Option<String>,
        /// A file holding the arguments (`-` for stdin), for large dashboards.
        #[arg(long)]
        args_file: Option<PathBuf>,
    },
    /// Manage sessions without Herdr.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Validate and push a dashboard JSON file into a session.
    Apply {
        #[arg(long, env = "DASHR_SESSION")]
        session: String,
        file: PathBuf,
    },
    /// Print panel status (no values) for a session.
    Status {
        #[arg(long, env = "DASHR_SESSION")]
        session: String,
    },
    /// Promote a session's dashboard to the persistent Grafana.
    Promote {
        #[arg(long, env = "DASHR_SESSION")]
        session: String,
        #[arg(long)]
        title: Option<String>,
    },
    /// Run a command and ship its stdout and stderr lines to a session's
    /// OpenTelemetry endpoint (or ship stdin when no command is given).
    /// The output still reaches the terminal and the exit code passes through.
    Tail {
        /// Defaults to $DASHR_SESSION, else the only OpenTelemetry session.
        #[arg(long)]
        session: Option<String>,
        /// The `service.name` the lines are stored under; defaults to the
        /// command's name.
        #[arg(long)]
        service: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Arm log expectations on a session's dashboard: tiles that turn green
    /// when an expected message is logged (red for a forbidden one) and a
    /// live trail highlighting them. `'NAME = PATTERN'` or just `PATTERN`.
    Expect {
        /// Defaults to $DASHR_SESSION, else the only session with Loki.
        #[arg(long)]
        session: Option<String>,
        /// A message that should appear (repeatable).
        #[arg(short = 'p', long = "present")]
        present: Vec<String>,
        /// A message that must not appear (repeatable).
        #[arg(short = 'a', long = "absent")]
        absent: Vec<String>,
        /// LogQL stream selector, e.g. '{service_name="checkout"}'.
        #[arg(long)]
        selector: Option<String>,
        /// Print the current verdict instead of arming; exit 1 unless passed.
        #[arg(long, conflicts_with_all = ["present", "absent", "clear"])]
        check: bool,
        /// Remove the expectations from the dashboard.
        #[arg(long, conflicts_with_all = ["present", "absent"])]
        clear: bool,
    },
    /// Dashboards saved on this machine: save a session's dashboard under a
    /// name and load it into a later session.
    Dashboards {
        #[command(subcommand)]
        command: DashboardsCommand,
    },
    /// Inspect a CodePipeline and print the inventory and proposed dashboard.
    Pipeline {
        url: String,
        /// Print only the proposed dashboard JSON.
        #[arg(long)]
        dashboard_only: bool,
    },
    /// Stop dashr containers whose owner is gone.
    Reap,
    /// Build the custom Grafana image with Infinity and Zabbix plugins.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Configuration helpers.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check prerequisites.
    Doctor,
    /// The dashboard-building agent skill (installed into ~/.claude/skills by default).
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
}

#[derive(Subcommand)]
enum HerdrCommand {
    /// Print the plugin manifest.
    Manifest,
    /// Run a manifest action.
    Action { id: String },
    /// Run a manifest pane.
    Pane { id: String },
    /// Handle a manifest event hook.
    Event { name: String },
    /// The startup hook.
    Startup,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start a Grafana session and print its record as JSON.
    Start {
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long)]
        pipeline: Option<String>,
        /// Start in OpenTelemetry mode (Grafana with Loki, Tempo, Prometheus
        /// and an OTLP endpoint), whatever the configuration says.
        #[arg(long)]
        otel: bool,
        /// Show this saved dashboard instead of the welcome dashboard.
        #[arg(long)]
        load: Option<String>,
    },
    /// Stop a session and delete its files.
    Stop { session: String },
    /// List sessions.
    List,
}

#[derive(Subcommand)]
enum DashboardsCommand {
    /// Save a session's current dashboard under a name.
    Save {
        name: String,
        /// Defaults to $DASHR_SESSION, else the only running session.
        #[arg(long)]
        session: Option<String>,
        /// Replace a saved dashboard with the same name.
        #[arg(long)]
        force: bool,
    },
    /// List saved dashboards, newest first.
    List,
    /// Replace a session's dashboard with a saved one.
    Load {
        name: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Print a saved dashboard's JSON.
    Show { name: String },
    /// Delete a saved dashboard.
    Delete { name: String },
}

#[derive(Subcommand)]
enum ImageCommand {
    Build {
        #[arg(long)]
        tag: Option<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print a commented example configuration.
    Example,
    /// Print the configuration file path.
    Path,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Install or refresh the skill in the configured skill directories.
    Install {
        /// Install into this skills directory instead of the configured ones.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Replace a same-named skill that dashr did not write.
        #[arg(long)]
        force: bool,
        /// Report problems but always exit 0 (used by the plugin build step).
        #[arg(long)]
        best_effort: bool,
    },
    /// Remove the skill, if dashr installed it.
    Uninstall {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Print one of the skill's files (default SKILL.md).
    Print {
        #[arg(default_value = "SKILL.md")]
        file: String,
    },
    /// List the skill's files.
    Files,
}

/// An error for the user: printed to stderr, exit status 1.
pub type Result<T> = std::result::Result<T, String>;

fn run(cli: Cli) -> Result<()> {
    let paths = dashr_runtime::Paths::resolve(cli.config_dir, cli.state_dir);
    match cli.command {
        Command::Herdr { command } => match command {
            HerdrCommand::Manifest => {
                print!(
                    "{}",
                    dashr_herdr::manifest::render(env!("CARGO_PKG_VERSION"))
                );
                Ok(())
            }
            HerdrCommand::Action { id } => herdr_cmds::action(&paths, &id),
            HerdrCommand::Pane { id } => match id.as_str() {
                "dashboard" => pane::dashboard(&paths),
                "doctor" => doctor::pane(&paths),
                other => Err(format!("unknown pane {other}")),
            },
            HerdrCommand::Event { name } => match name.as_str() {
                "pane-closed" => herdr_cmds::pane_closed(&paths),
                other => Err(format!("unknown event {other}")),
            },
            HerdrCommand::Startup => herdr_cmds::startup(&paths),
        },
        Command::Mcp { session, herdr_bin } => standalone::mcp(&paths, &session, herdr_bin),
        Command::Wait { session, timeout } => agent::wait(
            &paths,
            session.as_deref(),
            std::time::Duration::from_secs(timeout),
        ),
        Command::Tool {
            name,
            session,
            args,
            args_file,
        } => agent::tool(
            &paths,
            session.as_deref(),
            name.as_deref(),
            args.as_deref(),
            args_file.as_deref(),
        ),
        Command::Session { command } => match command {
            SessionCommand::Start {
                name,
                pipeline,
                otel,
                load,
            } => {
                standalone::session_start(&paths, &name, pipeline.as_deref(), otel, load.as_deref())
            }
            SessionCommand::Stop { session } => standalone::session_stop(&paths, &session),
            SessionCommand::List => standalone::session_list(&paths),
        },
        Command::Apply { session, file } => standalone::apply(&paths, &session, &file),
        Command::Status { session } => standalone::status(&paths, &session),
        Command::Promote { session, title } => {
            standalone::promote(&paths, &session, title.as_deref())
        }
        Command::Tail {
            session,
            service,
            command,
        } => standalone::tail(&paths, session.as_deref(), service.as_deref(), &command),
        Command::Expect {
            session,
            present,
            absent,
            selector,
            check,
            clear,
        } => standalone::expect(
            &paths,
            session.as_deref(),
            standalone::ExpectRequest {
                present,
                absent,
                selector,
                check,
                clear,
            },
        ),
        Command::Dashboards { command } => match command {
            DashboardsCommand::Save {
                name,
                session,
                force,
            } => standalone::dashboards_save(&paths, session.as_deref(), &name, force),
            DashboardsCommand::List => standalone::dashboards_list(&paths),
            DashboardsCommand::Load { name, session } => {
                standalone::dashboards_load(&paths, session.as_deref(), &name)
            }
            DashboardsCommand::Show { name } => standalone::dashboards_show(&paths, &name),
            DashboardsCommand::Delete { name } => standalone::dashboards_delete(&paths, &name),
        },
        Command::Pipeline {
            url,
            dashboard_only,
        } => standalone::pipeline(&paths, &url, dashboard_only),
        Command::Reap => standalone::reap(&paths),
        Command::Image {
            command: ImageCommand::Build { tag },
        } => standalone::image_build(&paths, tag),
        Command::Config { command } => match command {
            ConfigCommand::Example => {
                print!("{}", dashr_core::config::EXAMPLE);
                Ok(())
            }
            ConfigCommand::Path => {
                println!(
                    "{}",
                    paths
                        .config_dir
                        .join(dashr_core::config::FILE_NAME)
                        .display()
                );
                Ok(())
            }
        },
        Command::Doctor => doctor::run(&paths).map(|_| ()),
        Command::Skill { command } => match command {
            SkillCommand::Install {
                dir,
                force,
                best_effort,
            } => {
                let result = standalone::skill_install(&paths, dir, force);
                match (result, best_effort) {
                    (Err(message), true) => {
                        eprintln!("dashr: skill not installed: {message}");
                        Ok(())
                    }
                    (result, _) => result,
                }
            }
            SkillCommand::Uninstall { dir } => standalone::skill_uninstall(&paths, dir),
            SkillCommand::Print { file } => dashr_runtime::skill::FILES
                .iter()
                .find(|(name, _)| *name == file)
                .map(|(_, contents)| print!("{contents}"))
                .ok_or_else(|| format!("no skill file {file}; see `dashr skill files`")),
            SkillCommand::Files => {
                for (name, _) in dashr_runtime::skill::FILES {
                    println!("{name}");
                }
                Ok(())
            }
        },
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("dashr: {message}");
            ExitCode::FAILURE
        }
    }
}
