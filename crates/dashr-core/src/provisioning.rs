//! Grafana datasource provisioning, generated from configuration.
//!
//! The output is JSON written to a `.yaml` file: JSON is valid YAML, Grafana
//! reads provisioning with a YAML parser, and emitting it with `serde_json`
//! means no hand-rolled quoting (decision DEC-010).
//!
//! Secrets never appear in the file. A `secret_env` entry becomes the
//! literal `$__env{NAME}` reference, which Grafana resolves from its own
//! environment, and the container receives that variable by name only
//! (requirement DASHR-DS-003).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{Config, DatasourceConfig, DatasourceKind};

/// The uid of the TestData datasource every session gets.
pub const TESTDATA_UID: &str = "dashr-testdata";

/// What the MCP server needs to know about a datasource to mask its data.
///
/// This is the only datasource information that reaches the agent: a name,
/// a uid, a type and a flag. No URL, user or secret reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatasourcePolicy {
    pub uid: String,
    pub name: String,
    #[serde(rename = "type")]
    pub plugin_type: String,
    pub personal: bool,
    #[serde(default)]
    pub allow_fields: Vec<String>,
}

/// Everything the container needs to start with its datasources.
#[derive(Debug, Clone, PartialEq)]
pub struct Provisioning {
    /// Contents of `provisioning/datasources/dashr.yaml`.
    pub datasources_file: String,
    /// Environment variable names the container must inherit.
    pub env_names: Vec<String>,
    pub policies: Vec<DatasourcePolicy>,
}

/// The built-in TestData datasource. Synthetic data only, so not personal.
pub fn testdata() -> DatasourceConfig {
    DatasourceConfig {
        name: "TestData".to_owned(),
        kind: DatasourceKind::Testdata,
        uid: Some(TESTDATA_UID.to_owned()),
        url: None,
        personal: false,
        allow_fields: Vec::new(),
        region: None,
        database: None,
        user: None,
        secret_env: Default::default(),
        json_data: Default::default(),
        default: false,
    }
}

/// A CloudWatch datasource for one region, added when a pipeline is opened.
///
/// Personal by default: CloudWatch Logs carry whatever the application
/// logged.
pub fn cloudwatch_for_region(region: &str) -> DatasourceConfig {
    DatasourceConfig {
        name: format!("CloudWatch ({region})"),
        kind: DatasourceKind::Cloudwatch,
        uid: Some(cloudwatch_uid(region)),
        url: None,
        personal: true,
        allow_fields: vec![
            "@timestamp".to_owned(),
            "@logStream".to_owned(),
            "@log".to_owned(),
        ],
        region: Some(region.to_owned()),
        database: None,
        user: None,
        secret_env: Default::default(),
        json_data: Default::default(),
        default: false,
    }
}

/// The uid used for an auto-provisioned CloudWatch datasource.
pub fn cloudwatch_uid(region: &str) -> String {
    format!("dashr-cloudwatch-{}", crate::ids::slug(region))
}

/// Rewrites a loopback URL so it resolves from inside the container.
///
/// `localhost` inside the container is the container. Docker's
/// `host.docker.internal` is the host on Docker Desktop, and on Linux once
/// the container is started with `--add-host host.docker.internal:host-gateway`
/// (requirement DASHR-DS-006).
pub fn container_url(url: &str) -> String {
    for loopback in ["localhost", "127.0.0.1", "[::1]"] {
        for scheme in ["http://", "https://", ""] {
            let prefix = format!("{scheme}{loopback}");
            if let Some(rest) = url.strip_prefix(&prefix)
                && (rest.is_empty() || rest.starts_with(':') || rest.starts_with('/'))
            {
                return format!("{scheme}host.docker.internal{rest}");
            }
        }
    }
    url.to_owned()
}

fn toml_to_json(value: &toml::Value) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn datasource_entry(datasource: &DatasourceConfig, is_default: bool) -> Value {
    let mut entry = Map::new();
    entry.insert("name".into(), json!(datasource.name));
    entry.insert("uid".into(), json!(datasource.effective_uid()));
    entry.insert("type".into(), json!(datasource.kind.plugin_type()));
    entry.insert("access".into(), json!("proxy"));
    entry.insert("isDefault".into(), json!(is_default));
    entry.insert("editable".into(), json!(false));

    let url = datasource.url.as_deref().map(container_url);
    let mut json_data = Map::new();
    match datasource.kind {
        DatasourceKind::Cloudwatch => {
            json_data.insert("authType".into(), json!("default"));
            json_data.insert(
                "defaultRegion".into(),
                json!(datasource.region.as_deref().unwrap_or("us-east-1")),
            );
        }
        DatasourceKind::Mssql => {
            if let Some(database) = &datasource.database {
                json_data.insert("database".into(), json!(database));
            }
            json_data.insert("encrypt".into(), json!("true"));
        }
        DatasourceKind::AzureMonitor => {
            json_data.insert("azureAuthType".into(), json!("clientsecret"));
            json_data.insert("cloudName".into(), json!("azuremonitor"));
        }
        DatasourceKind::Zabbix => {
            if let Some(user) = &datasource.user {
                json_data.insert("username".into(), json!(user));
            }
        }
        DatasourceKind::Seq => {
            if let Some(url) = &url {
                json_data.insert("allowedHosts".into(), json!([url]));
            }
            if datasource.secret_env.contains_key("httpHeaderValue1") {
                json_data.insert("httpHeaderName1".into(), json!("X-Seq-ApiKey"));
            }
        }
        DatasourceKind::Prometheus => {
            json_data.insert("httpMethod".into(), json!("POST"));
        }
        DatasourceKind::Loki | DatasourceKind::Tempo | DatasourceKind::Testdata => {}
    }
    for (key, value) in &datasource.json_data {
        json_data.insert(key.clone(), toml_to_json(value));
    }

    if let Some(url) = url {
        entry.insert("url".into(), json!(url));
    }
    if let Some(user) = &datasource.user
        && datasource.kind != DatasourceKind::Zabbix
    {
        entry.insert("user".into(), json!(user));
    }
    if !json_data.is_empty() {
        entry.insert("jsonData".into(), Value::Object(json_data));
    }
    if !datasource.secret_env.is_empty() {
        let secure: Map<String, Value> = datasource
            .secret_env
            .iter()
            .map(|(key, env)| (key.clone(), json!(format!("$__env{{{env}}}"))))
            .collect();
        entry.insert("secureJsonData".into(), Value::Object(secure));
    }
    Value::Object(entry)
}

/// The datasources the OpenTelemetry image provisions itself
/// (DASHR-OTEL-002). Logs and traces carry whatever the application wrote,
/// so they are personal; the resource and severity labels are safe to show.
/// Metrics and profiles are numbers keyed by names the application chose.
pub fn otel_policies() -> Vec<DatasourcePolicy> {
    let policy = |uid: &str, name: &str, plugin_type: &str, personal: bool, allow: &[&str]| {
        DatasourcePolicy {
            uid: uid.to_owned(),
            name: name.to_owned(),
            plugin_type: plugin_type.to_owned(),
            personal,
            allow_fields: allow.iter().map(|field| (*field).to_owned()).collect(),
        }
    };
    vec![
        policy(
            "loki",
            "Loki (OpenTelemetry logs)",
            "loki",
            true,
            &[
                "service_name",
                "detected_level",
                "severity_text",
                "level",
                "log_iostream",
            ],
        ),
        policy(
            "tempo",
            "Tempo (OpenTelemetry traces)",
            "tempo",
            true,
            &["service_name"],
        ),
        policy(
            "prometheus",
            "Prometheus (OpenTelemetry metrics)",
            "prometheus",
            false,
            &[],
        ),
        policy(
            "pyroscope",
            "Pyroscope (profiles)",
            "grafana-pyroscope-datasource",
            false,
            &[],
        ),
    ]
}

/// Builds the provisioning for a session.
///
/// `extra` holds datasources added for this session only, such as the
/// CloudWatch datasource of an opened pipeline. A configured datasource with
/// the same uid wins over an extra one.
pub fn build(config: &Config, extra: &[DatasourceConfig]) -> Provisioning {
    let mut all: Vec<DatasourceConfig> = config.datasources.clone();
    for datasource in extra {
        if !all
            .iter()
            .any(|existing| existing.effective_uid() == datasource.effective_uid())
        {
            all.push(datasource.clone());
        }
    }
    let mut builtin = testdata();
    builtin.personal = config.masking.testdata_personal;
    all.push(builtin);

    let explicit_default = all.iter().position(|datasource| datasource.default);
    let default_index = explicit_default.unwrap_or(0);

    let entries: Vec<Value> = all
        .iter()
        .enumerate()
        .map(|(index, datasource)| datasource_entry(datasource, index == default_index))
        .collect();

    let mut env_names: Vec<String> = all
        .iter()
        .flat_map(|datasource| datasource.secret_env.values().cloned())
        .collect();
    env_names.sort();
    env_names.dedup();

    let mut policies: Vec<DatasourcePolicy> = all
        .iter()
        .map(|datasource| DatasourcePolicy {
            uid: datasource.effective_uid(),
            name: datasource.name.clone(),
            plugin_type: datasource.kind.plugin_type().to_owned(),
            personal: datasource.personal,
            allow_fields: datasource.allow_fields.clone(),
        })
        .collect();
    if config.otel.enabled {
        policies.extend(otel_policies());
    }

    let document = json!({
        "apiVersion": 1,
        "datasources": entries,
    });
    Provisioning {
        datasources_file: serde_json::to_string_pretty(&document).unwrap_or_default(),
        env_names,
        policies,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text, "test").expect("config parses")
    }

    #[test]
    fn otel_mode_adds_the_image_datasources_to_the_policies_only() {
        let mut config = Config::default();
        config.otel.enabled = true;
        let provisioning = build(&config, &[]);
        let document: Value = serde_json::from_str(&provisioning.datasources_file).unwrap();
        // The image provisions its own; dashr only needs to know how to mask.
        assert_eq!(document["datasources"].as_array().unwrap().len(), 1);
        let uids: Vec<&str> = provisioning
            .policies
            .iter()
            .map(|p| p.uid.as_str())
            .collect();
        assert_eq!(
            uids,
            [TESTDATA_UID, "loki", "tempo", "prometheus", "pyroscope"]
        );
        assert!(provisioning.policies[1].personal, "logs are personal");
        assert!(!provisioning.policies[3].personal, "metrics are not");
        for uid in crate::config::OTEL_DATASOURCE_UIDS {
            assert!(uids.contains(uid));
        }
        assert!(build(&Config::default(), &[]).policies.len() == 1);
    }

    #[test]
    fn default_config_provisions_only_testdata() {
        let provisioning = build(&Config::default(), &[]);
        let document: Value = serde_json::from_str(&provisioning.datasources_file).unwrap();
        let datasources = document["datasources"].as_array().unwrap();
        assert_eq!(datasources.len(), 1);
        assert_eq!(datasources[0]["uid"], TESTDATA_UID);
        assert_eq!(datasources[0]["isDefault"], true);
        assert!(!provisioning.policies[0].personal);
    }

    #[test]
    fn secrets_are_env_references_never_values() {
        let config = parse(
            r#"
[[datasources]]
name = "Seq"
kind = "seq"
url = "http://localhost:5341"
secret_env = { httpHeaderValue1 = "SEQ_API_KEY" }
"#,
        );
        let provisioning = build(&config, &[]);
        assert!(
            provisioning
                .datasources_file
                .contains("\"httpHeaderValue1\": \"$__env{SEQ_API_KEY}\"")
        );
        assert_eq!(provisioning.env_names, vec!["SEQ_API_KEY".to_owned()]);
        assert!(provisioning.datasources_file.contains("X-Seq-ApiKey"));
        assert!(
            provisioning
                .datasources_file
                .contains("http://host.docker.internal:5341")
        );
    }

    #[test]
    fn loopback_urls_are_rewritten_and_others_kept() {
        assert_eq!(
            container_url("http://localhost:9090"),
            "http://host.docker.internal:9090"
        );
        assert_eq!(
            container_url("https://127.0.0.1/prom"),
            "https://host.docker.internal/prom"
        );
        assert_eq!(container_url("localhost:1433"), "host.docker.internal:1433");
        assert_eq!(
            container_url("http://localhostile.example"),
            "http://localhostile.example"
        );
        assert_eq!(
            container_url("http://prom.internal:9090"),
            "http://prom.internal:9090"
        );
    }

    #[test]
    fn every_kind_maps_to_its_plugin() {
        let config = parse(
            r#"
[[datasources]]
name = "P"
kind = "prometheus"
url = "http://p"
personal = false
[[datasources]]
name = "SQL"
kind = "mssql"
url = "db:1433"
database = "orders"
user = "reader"
secret_env = { password = "MSSQL_PASSWORD" }
[[datasources]]
name = "Z"
kind = "zabbix"
url = "http://z/api_jsonrpc.php"
user = "api"
secret_env = { password = "ZABBIX_PASSWORD" }
[[datasources]]
name = "Azure"
kind = "azure_monitor"
json_data = { tenantId = "t", clientId = "c" }
secret_env = { clientSecret = "AZURE_CLIENT_SECRET" }
default = true
"#,
        );
        let provisioning = build(&config, &[cloudwatch_for_region("eu-west-1")]);
        let document: Value = serde_json::from_str(&provisioning.datasources_file).unwrap();
        let by_name = |name: &str| {
            document["datasources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == name)
                .cloned()
                .unwrap()
        };
        assert_eq!(by_name("SQL")["jsonData"]["database"], "orders");
        assert_eq!(by_name("SQL")["user"], "reader");
        assert_eq!(by_name("Z")["jsonData"]["username"], "api");
        assert!(by_name("Z").get("user").is_none());
        assert_eq!(by_name("Azure")["jsonData"]["tenantId"], "t");
        assert_eq!(by_name("Azure")["isDefault"], true);
        assert_eq!(by_name("P")["isDefault"], false);
        let cloudwatch = by_name("CloudWatch (eu-west-1)");
        assert_eq!(cloudwatch["jsonData"]["defaultRegion"], "eu-west-1");
        assert_eq!(cloudwatch["uid"], "dashr-cloudwatch-eu-west-1");
        assert_eq!(
            provisioning.env_names,
            vec!["AZURE_CLIENT_SECRET", "MSSQL_PASSWORD", "ZABBIX_PASSWORD"]
        );
        let personal: Vec<bool> = provisioning.policies.iter().map(|p| p.personal).collect();
        assert_eq!(personal, vec![false, true, true, true, true, false]);
    }

    #[test]
    fn configured_datasource_wins_over_extra_with_same_uid() {
        let config = parse(
            r#"
[[datasources]]
name = "My CloudWatch"
kind = "cloudwatch"
uid = "dashr-cloudwatch-eu-west-1"
region = "eu-west-1"
personal = false
"#,
        );
        let provisioning = build(&config, &[cloudwatch_for_region("eu-west-1")]);
        assert_eq!(provisioning.policies.len(), 2);
        assert_eq!(provisioning.policies[0].name, "My CloudWatch");
    }
}
