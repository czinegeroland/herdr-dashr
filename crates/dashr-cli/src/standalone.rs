//! Commands that work without Herdr: sessions, apply, status, MCP, image.

use std::path::Path;

use dashr_core::masking::Masker;
use dashr_core::session::SessionStore;
use dashr_docker::Docker;
use dashr_herdr::Herdr;
use dashr_mcp::Server;
use dashr_mcp::tools::{DashrTools, INSTRUCTIONS};
use dashr_runtime::Paths;
use dashr_runtime::browser::Browser;
use dashr_runtime::session::{self, Identity};

use crate::Result;
use crate::herdr_cmds::load_config;

/// The agent skill, shipped in the repository and embedded in the binary.
pub const SKILL: &str = include_str!("../../../.agents/skills/herdr-dashr/SKILL.md");

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

pub fn mcp(paths: &Paths, session: &str, herdr_bin: Option<String>) -> Result<()> {
    let config = load_config(paths)?;
    let store = SessionStore::new(&paths.state_dir);
    let herdr = herdr_bin
        .or_else(|| std::env::var("HERDR_BIN_PATH").ok())
        .filter(|bin| !bin.is_empty())
        .map(|bin| Herdr::new(&bin));
    let tools = DashrTools::new(config, store, session, herdr);
    let mut server = Server::new(
        tools,
        "herdr-dashr",
        env!("CARGO_PKG_VERSION"),
        INSTRUCTIONS,
    );
    let stdin = std::io::stdin();
    server
        .serve(stdin.lock(), std::io::stdout())
        .map_err(|error| error.to_string())
}

pub fn session_start(paths: &Paths, name: &str, pipeline: Option<&str>) -> Result<()> {
    let config = load_config(paths)?;
    let pipeline = pipeline
        .map(dashr_aws::url::parse)
        .transpose()
        .map_err(|error| error.to_string())?;
    let aws = dashr_aws::cli::AwsCli::new(&config.aws.cli, config.aws.profile.as_deref());
    let started = session::start(
        &config,
        &Identity::standalone(name),
        pipeline.as_ref(),
        &SessionStore::new(&paths.state_dir),
        &Docker::new(&config.docker.command),
        Some(&aws),
    )
    .map_err(|error| error.to_string())?;
    for warning in &started.warnings {
        eprintln!("dashr: {warning}");
    }
    print_json(&serde_json::json!({
        "session_id": started.record.session_id,
        "url": started.record.kiosk_url(),
        "container": started.record.container,
        "in_memory": started.in_memory,
    }))
}

pub fn session_stop(paths: &Paths, id: &str) -> Result<()> {
    let config = load_config(paths)?;
    let store = SessionStore::new(&paths.state_dir);
    let record = store.load(id).map_err(|error| error.to_string())?;
    session::stop(&record, &Docker::new(&config.docker.command), &store)
        .map_err(|error| error.to_string())?;
    println!("stopped {}", record.session_id);
    Ok(())
}

pub fn session_list(paths: &Paths) -> Result<()> {
    let records = SessionStore::new(&paths.state_dir).list();
    let rows: Vec<_> = records
        .iter()
        .map(|record| {
            serde_json::json!({
                "session_id": record.session_id,
                "pane": record.pane_id,
                "url": record.kiosk_url(),
                "pipeline": record.pipeline,
            })
        })
        .collect();
    print_json(&rows)
}

pub fn apply(paths: &Paths, id: &str, file: &Path) -> Result<()> {
    let config = load_config(paths)?;
    let record = SessionStore::new(&paths.state_dir)
        .load(id)
        .map_err(|error| error.to_string())?;
    let text =
        std::fs::read_to_string(file).map_err(|error| format!("{}: {error}", file.display()))?;
    let input: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", file.display()))?;
    let browser = config
        .browser
        .enabled
        .then(|| Browser::new(&config.browser.command));
    let outcome = dashr_runtime::apply::apply(
        &record,
        &dashr_grafana::Client::local(&record.grafana_url()),
        browser.as_ref(),
        &config.grafana.time_from,
        &input,
        "applied with dashr apply",
    )
    .map_err(|error| error.to_string())?;
    print_json(&outcome)
}

pub fn status(paths: &Paths, id: &str) -> Result<()> {
    let config = load_config(paths)?;
    let record = SessionStore::new(&paths.state_dir)
        .load(id)
        .map_err(|error| error.to_string())?;
    let client = dashr_grafana::Client::local(&record.grafana_url());
    let model = client
        .dashboard(&record.dashboard_uid)
        .map_err(|error| error.to_string())?;
    let statuses =
        dashr_runtime::status::dashboard_status(&client, &model, &Masker::new(&config.masking));
    print_json(&serde_json::json!({
        "summary": dashr_runtime::status::Summary::of(&statuses),
        "panels": statuses,
    }))
}

pub fn promote(paths: &Paths, id: &str, title: Option<&str>) -> Result<()> {
    let config = load_config(paths)?;
    let record = SessionStore::new(&paths.state_dir)
        .load(id)
        .map_err(|error| error.to_string())?;
    let promoted = dashr_runtime::promote::promote(
        &record,
        &dashr_grafana::Client::local(&record.grafana_url()),
        config.promote.as_ref(),
        title,
    )
    .map_err(|error| error.to_string())?;
    print_json(&promoted)
}

pub fn pipeline(paths: &Paths, url: &str, dashboard_only: bool) -> Result<()> {
    let config = load_config(paths)?;
    let pipeline = dashr_aws::url::parse(url).map_err(|error| error.to_string())?;
    let aws = dashr_aws::cli::AwsCli::new(&config.aws.cli, config.aws.profile.as_deref());
    let inventory = aws.discover(&pipeline).map_err(|error| error.to_string())?;
    let proposal = dashr_aws::propose::propose(
        &inventory,
        &dashr_core::provisioning::cloudwatch_uid(&pipeline.region),
    );
    if dashboard_only {
        print_json(&proposal)
    } else {
        print_json(&serde_json::json!({"inventory": inventory, "dashboard": proposal}))
    }
}

pub fn reap(paths: &Paths) -> Result<()> {
    let config = load_config(paths)?;
    let docker = Docker::new(&config.docker.command);
    let store = SessionStore::new(&paths.state_dir);
    // Outside Herdr there is no pane list: stop standalone sessions whose
    // container is gone, and report the rest.
    let running: Vec<String> = docker
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|container| container.name)
        .collect();
    let mut removed = 0;
    for record in store.list() {
        if !running.contains(&record.container) {
            session::stop(&record, &docker, &store).ok();
            removed += 1;
        }
    }
    println!(
        "removed {removed} stale session record(s); {} dashr container(s) running",
        running.len()
    );
    Ok(())
}

pub fn image_build(paths: &Paths, tag: Option<String>) -> Result<()> {
    let config = load_config(paths)?;
    let tag = tag.unwrap_or_else(|| format!("herdr-dashr-grafana:{}", env!("CARGO_PKG_VERSION")));
    let context = std::env::temp_dir().join(format!("dashr-image-{}", std::process::id()));
    std::fs::create_dir_all(&context).map_err(|error| error.to_string())?;
    let result = Docker::new(&config.docker.command).build_image(
        &tag,
        dashr_core::config::DEFAULT_IMAGE,
        &context,
    );
    let _ = std::fs::remove_dir_all(&context);
    result.map_err(|error| error.to_string())?;
    println!(
        "built {tag}; set `image = \"{tag}\"` under [grafana] in {}",
        paths
            .config_dir
            .join(dashr_core::config::FILE_NAME)
            .display()
    );
    Ok(())
}

pub fn skill_install() -> Result<()> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let dir = Path::new(&home)
        .join(".claude")
        .join("skills")
        .join("herdr-dashr");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    std::fs::write(dir.join("SKILL.md"), SKILL).map_err(|error| error.to_string())?;
    println!("installed {}", dir.join("SKILL.md").display());
    Ok(())
}
