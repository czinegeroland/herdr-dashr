//! The `dashr` binary: Herdr plugin entrypoints and standalone commands.

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
    about = "Agent-built, live Grafana dashboards in a Herdr pane"
)]
struct Cli {
    /// Configuration directory (default: HERDR_PLUGIN_CONFIG_DIR, then XDG).
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    /// State directory (default: HERDR_PLUGIN_STATE_DIR, then XDG).
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
    },
    /// Stop a session and delete its files.
    Stop { session: String },
    /// List sessions.
    List,
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
        Command::Session { command } => match command {
            SessionCommand::Start { name, pipeline } => {
                standalone::session_start(&paths, &name, pipeline.as_deref())
            }
            SessionCommand::Stop { session } => standalone::session_stop(&paths, &session),
            SessionCommand::List => standalone::session_list(&paths),
        },
        Command::Apply { session, file } => standalone::apply(&paths, &session, &file),
        Command::Status { session } => standalone::status(&paths, &session),
        Command::Promote { session, title } => {
            standalone::promote(&paths, &session, title.as_deref())
        }
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
