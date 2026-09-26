//! User configuration, read from `dashr.toml` in the plugin config directory.
//!
//! Every field has a default, so a missing file is a valid configuration:
//! a Grafana with only the TestData datasource, terminal-browser on top and
//! Claude Code underneath. The file names environment variables for secrets;
//! it never holds a secret itself (requirement DASHR-DS-003).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file name looked up inside `HERDR_PLUGIN_CONFIG_DIR`.
pub const FILE_NAME: &str = "dashr.toml";

/// The Grafana image used when the configuration names none.
///
/// Pinned to an exact release (requirement DASHR-GRAF-007): a floating tag
/// would change the dashboard schema under an agent's feet between two panes.
pub const DEFAULT_IMAGE: &str = "grafana/grafana:12.1.1";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not valid dashr configuration: {message}")]
    Parse { path: String, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub grafana: GrafanaConfig,
    pub docker: DockerConfig,
    pub browser: BrowserConfig,
    pub agent: AgentConfig,
    pub monitor: MonitorConfig,
    pub aws: AwsConfig,
    pub masking: MaskingConfig,
    pub otel: OtelConfig,
    pub promote: Option<PromoteConfig>,
    pub datasources: Vec<DatasourceConfig>,
}

/// The image `[otel] enabled = true` runs: Grafana, an OpenTelemetry
/// collector, Loki, Tempo and Prometheus in one container, so a session is
/// still exactly one container (requirement DASHR-OTEL-001). Pinned like the
/// Grafana image.
pub const DEFAULT_OTEL_IMAGE: &str = "grafana/otel-lgtm:0.34.0";

/// Uids of the datasources the OpenTelemetry image provisions itself.
pub const OTEL_DATASOURCE_UIDS: &[&str] = &["loki", "tempo", "prometheus", "pyroscope"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct OtelConfig {
    /// Run the all-in-one OpenTelemetry image instead of plain Grafana: the
    /// session then receives OTLP logs, traces and metrics on loopback.
    pub enabled: bool,
    pub image: String,
    /// Memory limit; the stack needs more than Grafana alone.
    pub memory: String,
    /// Dashboard refresh for OTel sessions, fast enough for a live log trail.
    pub refresh: String,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            image: DEFAULT_OTEL_IMAGE.to_owned(),
            memory: "2g".to_owned(),
            refresh: "2s".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GrafanaConfig {
    /// Image to run. `dashr image build` produces `herdr-dashr-grafana:<ver>`
    /// with the Infinity and Zabbix plugins baked in.
    pub image: String,
    /// Docker memory limit. Swap is set to the same value so the tmpfs that
    /// holds Grafana's database cannot be paged out to disk.
    pub memory: String,
    /// Seconds to wait for `/api/health` before giving up.
    pub startup_timeout_secs: u64,
    /// Browser-side refresh interval in the kiosk URL.
    pub refresh: String,
    /// Default dashboard time range.
    pub time_from: String,
}

impl Default for GrafanaConfig {
    fn default() -> Self {
        Self {
            image: DEFAULT_IMAGE.to_owned(),
            memory: "768m".to_owned(),
            startup_timeout_secs: 90,
            refresh: "5s".to_owned(),
            time_from: "now-1h".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    pub command: String,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            command: "docker".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    /// `false` always uses the text status view (requirement DASHR-VIEW-003).
    pub enabled: bool,
    pub command: String,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: "terminal-browser".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// `false` leaves the chat pane out entirely.
    pub enabled: bool,
    /// Argv template. `{mcp_config}` becomes the generated MCP config path
    /// and `{prompt}` the opening prompt.
    pub command: Vec<String>,
    /// Also register `mcp-grafana`. Off by default: its query tools return
    /// raw rows and would bypass masking (decision DEC-011).
    pub mcp_grafana: bool,
    pub mcp_grafana_command: String,
    /// Fraction of the tab the dashboard keeps when the chat pane splits off.
    pub split_ratio: f32,
    /// Keep the herdr-dashr agent skill installed and current in
    /// `skill_dirs` (DASHR-SKILL-002).
    pub install_skill: bool,
    /// Skill directories to install into; `~/` is expanded.
    pub skill_dirs: Vec<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // The prompt comes first: Claude Code's `--mcp-config` takes
            // several values and would swallow anything after it.
            command: vec![
                "claude".to_owned(),
                "{prompt}".to_owned(),
                "--mcp-config".to_owned(),
                "{mcp_config}".to_owned(),
            ],
            mcp_grafana: false,
            mcp_grafana_command: "mcp-grafana".to_owned(),
            split_ratio: 0.62,
            install_skill: true,
            skill_dirs: vec!["~/.claude/skills".to_owned()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    /// Seconds between panel health checks and watch evaluations.
    pub interval_secs: u64,
    /// Raise a Herdr notification when a watch breaches.
    pub notify: bool,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval_secs: 15,
            notify: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AwsConfig {
    pub cli: String,
    /// Profile to export short-lived credentials from. Unset uses whatever
    /// the AWS CLI's default chain resolves.
    pub profile: Option<String>,
}

impl Default for AwsConfig {
    fn default() -> Self {
        Self {
            cli: "aws".to_owned(),
            profile: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MaskingConfig {
    /// Maximum rows a sample returns.
    pub max_rows: usize,
    /// Strings longer than this are truncated after masking.
    pub max_string_len: usize,
    /// Extra field-name tokens that mark a field as personal.
    pub deny_field_tokens: Vec<String>,
    /// Field names that always pass unmasked, for every datasource.
    pub allow_fields: Vec<String>,
    /// Extra value patterns (regular expressions) to replace.
    pub extra_patterns: BTreeMap<String, String>,
    /// Treat the built-in TestData datasource as personal. Off by default:
    /// TestData is synthetic. The end-to-end suite turns it on to exercise
    /// full masking against a real Grafana.
    pub testdata_personal: bool,
}

impl Default for MaskingConfig {
    fn default() -> Self {
        Self {
            max_rows: 20,
            max_string_len: 160,
            deny_field_tokens: Vec::new(),
            allow_fields: Vec::new(),
            extra_patterns: BTreeMap::new(),
            testdata_personal: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromoteConfig {
    /// Base URL of the persistent Grafana.
    pub url: String,
    /// Environment variable holding a service-account token.
    pub token_env: String,
    /// Folder title dashboards are promoted into.
    #[serde(default = "default_folder")]
    pub folder: String,
}

fn default_folder() -> String {
    "dashr".to_owned()
}

/// The datasource types dashr knows how to provision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DatasourceKind {
    Prometheus,
    Loki,
    Tempo,
    Cloudwatch,
    Mssql,
    AzureMonitor,
    Zabbix,
    Seq,
    Testdata,
}

impl DatasourceKind {
    /// Grafana's plugin id for the kind.
    pub fn plugin_type(self) -> &'static str {
        match self {
            DatasourceKind::Prometheus => "prometheus",
            DatasourceKind::Loki => "loki",
            DatasourceKind::Tempo => "tempo",
            DatasourceKind::Cloudwatch => "cloudwatch",
            DatasourceKind::Mssql => "mssql",
            DatasourceKind::AzureMonitor => "grafana-azure-monitor-datasource",
            DatasourceKind::Zabbix => "alexanderzobnin-zabbix-datasource",
            // Seq has no official datasource; its HTTP API is read through
            // the Infinity plugin (docs/DESIGN.md, verified in DEC-012).
            DatasourceKind::Seq => "yesoreyeram-infinity-datasource",
            DatasourceKind::Testdata => "grafana-testdata-datasource",
        }
    }

    /// Whether the kind needs a plugin that the stock image lacks.
    pub fn needs_custom_image(self) -> bool {
        matches!(self, DatasourceKind::Zabbix | DatasourceKind::Seq)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DatasourceConfig {
    pub name: String,
    pub kind: DatasourceKind,
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Whether values may contain personal data. Defaults to `true`: a
    /// datasource is personal until someone says otherwise (DASHR-DS-005).
    #[serde(default = "default_true")]
    pub personal: bool,
    /// Field names that pass unmasked for this datasource.
    #[serde(default)]
    pub allow_fields: Vec<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    /// `secureJsonData` key -> environment variable name.
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
    /// Extra `jsonData`, merged over the generated defaults.
    #[serde(default)]
    pub json_data: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub default: bool,
}

fn default_true() -> bool {
    true
}

impl DatasourceConfig {
    /// The uid the datasource is provisioned under.
    pub fn effective_uid(&self) -> String {
        self.uid
            .clone()
            .unwrap_or_else(|| crate::ids::slug(&self.name))
    }
}

impl Config {
    /// Parses configuration text; `origin` names it in error messages.
    pub fn parse(text: &str, origin: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|error| ConfigError::Parse {
            path: origin.to_owned(),
            message: error.message().to_owned(),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads `dashr.toml` from `dir`, or the defaults when it does not exist.
    pub fn load_from_dir(dir: &Path) -> Result<Self, ConfigError> {
        let path = dir.join(FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text, &path.display().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let mut uids = std::collections::BTreeSet::new();
        for datasource in &self.datasources {
            if datasource.name.trim().is_empty() {
                return Err(ConfigError::Invalid("a datasource has no name".into()));
            }
            let uid = datasource.effective_uid();
            if uid == crate::provisioning::TESTDATA_UID {
                return Err(ConfigError::Invalid(format!(
                    "datasource uid {uid} is reserved for the built-in TestData datasource"
                )));
            }
            if !uids.insert(uid.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate datasource uid {uid}"
                )));
            }
            for (key, env) in &datasource.secret_env {
                if !is_env_name(env) {
                    return Err(ConfigError::Invalid(format!(
                        "datasource {}: secret_env.{key} must name an environment variable, \
                         not hold a value",
                        datasource.name
                    )));
                }
            }
            let needs_url = !matches!(
                datasource.kind,
                DatasourceKind::Cloudwatch
                    | DatasourceKind::AzureMonitor
                    | DatasourceKind::Testdata
            );
            if needs_url && datasource.url.is_none() {
                return Err(ConfigError::Invalid(format!(
                    "datasource {} ({:?}) needs a url",
                    datasource.name, datasource.kind
                )));
            }
        }
        if self.otel.enabled {
            for datasource in &self.datasources {
                let uid = datasource.effective_uid();
                if OTEL_DATASOURCE_UIDS.contains(&uid.as_str()) {
                    return Err(ConfigError::Invalid(format!(
                        "datasource uid {uid} is provisioned by the OpenTelemetry image; give {} another uid",
                        datasource.name
                    )));
                }
            }
        }
        if let Some(promote) = &self.promote
            && !is_env_name(&promote.token_env)
        {
            return Err(ConfigError::Invalid(
                "promote.token_env must name an environment variable".into(),
            ));
        }
        if !(0.2..=0.9).contains(&self.agent.split_ratio) {
            return Err(ConfigError::Invalid(
                "agent.split_ratio must be between 0.2 and 0.9".into(),
            ));
        }
        if self.masking.max_rows == 0 || self.masking.max_rows > 200 {
            return Err(ConfigError::Invalid(
                "masking.max_rows must be between 1 and 200".into(),
            ));
        }
        for (name, pattern) in &self.masking.extra_patterns {
            regex::Regex::new(pattern).map_err(|error| {
                ConfigError::Invalid(format!("masking.extra_patterns.{name}: {error}"))
            })?;
        }
        if self.monitor.interval_secs < 2 {
            return Err(ConfigError::Invalid(
                "monitor.interval_secs must be at least 2".into(),
            ));
        }
        Ok(())
    }

    /// Whether any configured datasource needs the custom image.
    pub fn needs_custom_image(&self) -> bool {
        self.datasources
            .iter()
            .any(|datasource| datasource.kind.needs_custom_image())
    }
}

/// An environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn is_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A commented example configuration, printed by `dashr config example`.
pub const EXAMPLE: &str = r#"# dashr configuration. Every section is optional.

[grafana]
# image = "herdr-dashr-grafana:0.1.0"   # after `dashr image build`
memory = "768m"
refresh = "5s"

[browser]
enabled = true            # false: text status view instead of terminal-browser

[agent]
command = ["claude", "{prompt}", "--mcp-config", "{mcp_config}"]
mcp_grafana = false       # true registers mcp-grafana; its query tools bypass masking
install_skill = true      # keep the herdr-dashr skill installed and current
skill_dirs = ["~/.claude/skills"]

[aws]
# profile = "dev"         # short-lived credentials are exported from this profile

[otel]
enabled = false           # true: one container that also receives OTLP logs, traces and metrics

[masking]
max_rows = 20
allow_fields = ["level", "status_code"]

# [promote]
# url = "https://grafana.example.com"
# token_env = "DASHR_PROMOTE_TOKEN"
# folder = "dashr"

[[datasources]]
name = "Prometheus"
kind = "prometheus"
url = "http://localhost:9090"
personal = false

[[datasources]]
name = "Loki"
kind = "loki"
url = "http://localhost:3100"
allow_fields = ["level", "service_name"]

# [[datasources]]
# name = "Seq"
# kind = "seq"
# url = "http://localhost:5341"
# secret_env = { httpHeaderValue1 = "SEQ_API_KEY" }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_the_default_configuration() {
        let config = Config::parse("", "test").expect("empty config parses");
        assert_eq!(config, Config::default());
        assert!(config.agent.command.contains(&"{mcp_config}".to_owned()));
    }

    #[test]
    fn example_configuration_parses() {
        let config = Config::parse(EXAMPLE, "example").expect("example parses");
        assert_eq!(config.datasources.len(), 2);
        assert!(!config.datasources[0].personal);
        assert!(config.datasources[1].personal, "personal defaults to true");
    }

    #[test]
    fn secret_env_must_be_a_name_not_a_value() {
        let text = r#"
[[datasources]]
name = "Seq"
kind = "seq"
url = "http://localhost:5341"
secret_env = { httpHeaderValue1 = "sk-live-12345/abc" }
"#;
        let error = Config::parse(text, "t").unwrap_err().to_string();
        assert!(
            error.contains("must name an environment variable"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::parse("[grafana]\nimgae = \"x\"\n", "t").is_err());
    }

    #[test]
    fn duplicate_and_reserved_uids_are_rejected() {
        let duplicate = r#"
[[datasources]]
name = "A"
kind = "prometheus"
url = "http://a"
uid = "same"
[[datasources]]
name = "B"
kind = "prometheus"
url = "http://b"
uid = "same"
"#;
        assert!(Config::parse(duplicate, "t").is_err());
        let reserved =
            "[[datasources]]\nname = \"T\"\nkind = \"testdata\"\nuid = \"dashr-testdata\"\n";
        assert!(Config::parse(reserved, "t").is_err());
    }

    #[test]
    fn url_is_required_for_network_datasources() {
        assert!(
            Config::parse(
                "[[datasources]]\nname = \"P\"\nkind = \"prometheus\"\n",
                "t"
            )
            .is_err()
        );
        assert!(
            Config::parse(
                "[[datasources]]\nname = \"C\"\nkind = \"cloudwatch\"\n",
                "t"
            )
            .is_ok()
        );
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            Config::load_from_dir(dir.path()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn custom_image_is_needed_only_for_plugin_datasources() {
        let mut config = Config::default();
        assert!(!config.needs_custom_image());
        config.datasources.push(DatasourceConfig {
            name: "Z".into(),
            kind: DatasourceKind::Zabbix,
            uid: None,
            url: Some("http://z".into()),
            personal: true,
            allow_fields: vec![],
            region: None,
            database: None,
            user: None,
            secret_env: BTreeMap::new(),
            json_data: BTreeMap::new(),
            default: false,
        });
        assert!(config.needs_custom_image());
    }

    #[test]
    fn otel_mode_reserves_its_datasource_uids() {
        let text = "[otel]\nenabled = true\n[[datasources]]\nname = \"L\"\nkind = \"loki\"\nurl = \"http://l\"\nuid = \"loki\"\n";
        assert!(
            Config::parse(text, "t")
                .unwrap_err()
                .to_string()
                .contains("OpenTelemetry image")
        );
        let off = text.replace("enabled = true", "enabled = false");
        assert!(Config::parse(&off, "t").is_ok());
        assert_eq!(Config::default().otel.image, DEFAULT_OTEL_IMAGE);
    }

    #[test]
    fn env_names() {
        assert!(is_env_name("AWS_SESSION_TOKEN"));
        assert!(is_env_name("_x1"));
        assert!(!is_env_name("1ABC"));
        assert!(!is_env_name("A-B"));
        assert!(!is_env_name(""));
    }
}
