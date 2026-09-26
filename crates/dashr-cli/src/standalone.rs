//! Commands that work without Herdr: sessions, apply, status, MCP, image.

use std::path::{Path, PathBuf};

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

pub fn session_start(paths: &Paths, name: &str, pipeline: Option<&str>, otel: bool) -> Result<()> {
    let mut config = load_config(paths)?;
    config.otel.enabled |= otel;
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
        "otlp_endpoint": started.record.otlp.map(|otlp| otlp.http_endpoint()),
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

/// The skills directories to use: `--dir`, else `agent.skill_dirs`.
fn skill_dirs(paths: &Paths, dir: Option<PathBuf>) -> Result<Vec<PathBuf>> {
    match dir {
        Some(dir) => Ok(vec![dir]),
        None => Ok(load_config(paths)?
            .agent
            .skill_dirs
            .iter()
            .map(|dir| dashr_runtime::skill::expand_home(dir))
            .collect()),
    }
}

pub fn skill_install(paths: &Paths, dir: Option<PathBuf>, force: bool) -> Result<()> {
    for skills_dir in skill_dirs(paths, dir)? {
        let outcome = dashr_runtime::skill::install(&skills_dir, force)
            .map_err(|error| format!("{}: {error}", skills_dir.display()))?;
        println!("dashr: {outcome}");
    }
    Ok(())
}

pub fn skill_uninstall(paths: &Paths, dir: Option<PathBuf>) -> Result<()> {
    for skills_dir in skill_dirs(paths, dir)? {
        match dashr_runtime::skill::uninstall(&skills_dir) {
            Ok(true) => println!(
                "dashr: removed the herdr-dashr skill from {}",
                skills_dir.display()
            ),
            Ok(false) => println!(
                "dashr: no dashr-installed skill in {}",
                skills_dir.display()
            ),
            Err(error) => return Err(format!("{}: {error}", skills_dir.display())),
        }
    }
    Ok(())
}

/// The session a command acts on: the one named, else `$DASHR_SESSION`,
/// else the only session that `fits`.
fn pick_session(
    store: &SessionStore,
    named: Option<&str>,
    fits: impl Fn(&dashr_core::session::SessionRecord) -> bool,
    what: &str,
) -> Result<dashr_core::session::SessionRecord> {
    let named = named
        .map(str::to_owned)
        .or_else(|| std::env::var("DASHR_SESSION").ok())
        .filter(|id| !id.is_empty());
    if let Some(id) = named {
        return store.load(&id).map_err(|error| error.to_string());
    }
    let mut candidates: Vec<_> = store.list().into_iter().filter(|r| fits(r)).collect();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(format!(
            "no {what} is running; open one (\"Open live logs and traces\" in Herdr, or `dashr session start --otel`)"
        )),
        _ => Err(format!(
            "{} {what}s are running; pick one with --session or DASHR_SESSION: {}",
            candidates.len(),
            candidates
                .iter()
                .map(|r| r.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

pub fn tail(
    paths: &Paths,
    session: Option<&str>,
    service: Option<&str>,
    command: &[String],
) -> Result<()> {
    let store = SessionStore::new(&paths.state_dir);
    let record = pick_session(
        &store,
        session,
        |r| r.otlp.is_some(),
        "OpenTelemetry session",
    )?;
    let otlp = record.otlp.ok_or_else(|| {
        format!(
            "session {} has no OpenTelemetry endpoint; open it in OpenTelemetry mode",
            record.session_id
        )
    })?;
    let exporter = dashr_runtime::otlp::Exporter::new(&otlp.http_endpoint());
    let service = service
        .map(str::to_owned)
        .or_else(|| {
            command.first().map(|program| {
                Path::new(program)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| program.clone())
            })
        })
        .unwrap_or_else(|| "stdin".to_owned());
    let outcome = if command.is_empty() {
        dashr_runtime::otlp::tail_stdin(exporter, &service)
    } else {
        dashr_runtime::otlp::tail_command(exporter, &service, command)
            .map_err(|error| format!("could not run {}: {error}", command[0]))?
    };
    if outcome.dropped > 0 {
        eprintln!(
            "dashr tail: {} line(s) did not reach the dashboard",
            outcome.dropped
        );
    }
    match outcome.code {
        Some(0) => Ok(()),
        // Exit with the command's own status, not dashr's error path.
        Some(code) => std::process::exit(code),
        None => std::process::exit(1),
    }
}

pub struct ExpectRequest {
    pub present: Vec<String>,
    pub absent: Vec<String>,
    pub selector: Option<String>,
    pub check: bool,
    pub clear: bool,
}

pub fn expect(paths: &Paths, session: Option<&str>, request: ExpectRequest) -> Result<()> {
    use dashr_core::logx::Presence;
    use dashr_runtime::logx;

    let config = load_config(paths)?;
    let store = SessionStore::new(&paths.state_dir);
    let record = pick_session(
        &store,
        session,
        |r| logx::loki_uid(r, None).is_ok(),
        "session with Loki",
    )?;
    let client = dashr_grafana::Client::local(&record.grafana_url());
    let browser = config
        .browser
        .enabled
        .then(|| Browser::new(&config.browser.command));
    let browser = browser.as_ref();
    let time_from = &config.grafana.time_from;
    if request.clear {
        let armed = logx::clear(&record, &client, browser, &store, time_from)
            .map_err(|error| error.to_string())?;
        println!(
            "{}",
            if armed {
                "log expectations cleared"
            } else {
                "no log expectations were armed"
            }
        );
        return Ok(());
    }
    if request.check {
        let report = logx::check(&record, &client, &store, &Masker::new(&config.masking))
            .map_err(|error| error.to_string())?;
        print_json(&report)?;
        return if report.passed {
            Ok(())
        } else {
            Err("log expectations not met".to_owned())
        };
    }
    let expectations: Vec<_> = request
        .present
        .iter()
        .map(|text| logx::parse_expectation(text, Presence::Present))
        .chain(
            request
                .absent
                .iter()
                .map(|text| logx::parse_expectation(text, Presence::Absent)),
        )
        .collect();
    let armed = logx::arm(
        &record,
        &client,
        browser,
        &store,
        time_from,
        expectations,
        request.selector.as_deref(),
        None,
    )
    .map_err(|error| error.to_string())?;
    print_json(&armed)
}
