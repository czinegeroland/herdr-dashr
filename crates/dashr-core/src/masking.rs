//! Masking: the boundary between real data and the agent.
//!
//! The agent designs dashboards from schemas and masked samples; only the
//! human sees real values (docs/DESIGN.md, "Privacy"). Everything the MCP
//! server returns that came out of a datasource passes through
//! [`Masker::mask_table`] first.
//!
//! Two layers apply to every string:
//!
//! 1. **Field rules.** A field whose name contains a personal token (`email`,
//!    `user`, `phone`, `ip`, ...) is replaced wholesale, unless the name is on
//!    an allow-list.
//! 2. **Value detectors.** Anything that *looks* personal or secret inside an
//!    otherwise harmless field (an email in a log message, a JWT in a URL) is
//!    replaced in place.
//!
//! Replacements are stable pseudonyms within one response (`<email#1>`
//! appears for every occurrence of the same address), so the agent can still
//! see cardinality and repetition without seeing the value
//! (requirement DASHR-PRIV-002).
//!
//! Datasources flagged non-personal skip the personal layer but keep the
//! secret detectors: a metrics label can still carry a leaked token
//! (DASHR-PRIV-005).

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::config::MaskingConfig;
use crate::provisioning::DatasourcePolicy;

/// Field-name tokens that mark a field as personal.
const PERSONAL_TOKENS: &[&str] = &[
    "email",
    "mail",
    "phone",
    "mobile",
    "tel",
    "msisdn",
    "address",
    "street",
    "city",
    "zip",
    "postcode",
    "postal",
    "ssn",
    "iban",
    "bic",
    "card",
    "pan",
    "cvv",
    "dob",
    "birth",
    "birthday",
    "ip",
    "ipaddress",
    "firstname",
    "lastname",
    "fullname",
    "surname",
    "username",
    "user",
    "login",
    "customer",
    "person",
    "name",
    "account",
    "passport",
    "gender",
    "geo",
    "lat",
    "lon",
    "latitude",
    "longitude",
];

/// Field-name tokens that mark a field as secret, for every datasource.
const SECRET_TOKENS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "cookie",
    "session",
    "authorization",
    "auth",
    "apikey",
    "key",
    "credential",
    "credentials",
    "signature",
];

/// Field names that pass even though a token matches.
///
/// `__name__` is the Prometheus metric name, `name` tokenises it; `job` and
/// `instance` are target identity, not people.
const DEFAULT_ALLOW: &[&str] = &[
    "__name__",
    "job",
    "instance",
    "level",
    "severity",
    "status",
    "status_code",
    "service",
    "service_name",
    "namespace",
    "pod",
    "container",
    "queue_name",
    "queuename",
    "functionname",
    "statemachinearn",
    "metric",
    "le",
    "quantile",
];

/// The kind of a masked value, which is also the pseudonym prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Detection {
    Email,
    Iban,
    Card,
    Phone,
    Ipv4,
    Ipv6,
    Jwt,
    AwsKey,
    Credential,
    Bearer,
    Custom,
}

impl Detection {
    fn label(self) -> &'static str {
        match self {
            Detection::Email => "email",
            Detection::Iban => "iban",
            Detection::Card => "card",
            Detection::Phone => "phone",
            Detection::Ipv4 => "ipv4",
            Detection::Ipv6 => "ipv6",
            Detection::Jwt => "jwt",
            Detection::AwsKey => "aws_key",
            Detection::Credential => "credential",
            Detection::Bearer => "bearer",
            Detection::Custom => "custom",
        }
    }

    /// Detectors that run on every datasource, personal or not: secrets,
    /// and personal data recognisable with high confidence (checksummed or
    /// unambiguous). A datasource flagged non-personal by mistake still
    /// cannot hand the agent an email address or a card number
    /// (DASHR-PRIV-005, found by the end-to-end suite).
    fn always_on(self) -> bool {
        matches!(
            self,
            Detection::Jwt
                | Detection::AwsKey
                | Detection::Credential
                | Detection::Bearer
                | Detection::Email
                | Detection::Iban
                | Detection::Card
                | Detection::Custom
        )
    }
}

struct Detector {
    kind: Detection,
    label: String,
    pattern: Regex,
    check: Check,
}

/// A post-match check, such as a checksum.
type Check = fn(&str) -> bool;

fn always(_: &str) -> bool {
    true
}

/// Luhn check for card numbers, on the digits only.
pub fn luhn(candidate: &str) -> bool {
    let digits: Vec<u32> = candidate.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(index, digit)| {
            if index % 2 == 1 {
                let doubled = digit * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                *digit
            }
        })
        .sum();
    sum % 10 == 0
}

/// ISO 13616 mod-97 check for IBANs.
pub fn iban_valid(candidate: &str) -> bool {
    let compact: String = candidate.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 15 || compact.len() > 34 {
        return false;
    }
    let (head, tail) = compact.split_at(4);
    let mut remainder: u32 = 0;
    for character in tail.chars().chain(head.chars()) {
        let value = match character {
            '0'..='9' => character as u32 - '0' as u32,
            'A'..='Z' => character as u32 - 'A' as u32 + 10,
            'a'..='z' => character as u32 - 'a' as u32 + 10,
            _ => return false,
        };
        let width = if value >= 10 { 100 } else { 10 };
        remainder = (remainder * width + value) % 97;
    }
    remainder == 1
}

fn phone_plausible(candidate: &str) -> bool {
    let digits = candidate.chars().filter(char::is_ascii_digit).count();
    if !(8..=15).contains(&digits) {
        return false;
    }
    let international = candidate.starts_with('+') || candidate.starts_with("00");
    let separated = candidate.chars().any(|c| " ().-".contains(c));
    // A bare run of digits is an id or an epoch timestamp far more often
    // than a phone number.
    if !international && !separated {
        return false;
    }
    // Dates: 2026-09-25, 25.09.2026, 2026.09.25.
    static DATE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\d{4}[-./]\d{2}[-./]\d{2}|^\d{2}[-./]\d{2}[-./]\d{4}")
            .expect("date pattern compiles")
    });
    !DATE.is_match(candidate)
}

fn detectors(extra: &BTreeMap<String, String>) -> Vec<Detector> {
    let builtin: [(Detection, &str, Check); 10] = [
        // Order matters: secrets first, so a JWT is not half-eaten by the
        // phone detector, and IBAN before card before phone.
        (
            Detection::Jwt,
            r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}",
            always,
        ),
        (Detection::AwsKey, r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", always),
        (
            Detection::Bearer,
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}",
            always,
        ),
        (
            Detection::Credential,
            r#"(?i)\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret)\b\s*[=:]\s*"?[^\s",;&]+"#,
            always,
        ),
        (
            Detection::Email,
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            always,
        ),
        (
            Detection::Iban,
            r"\b[A-Z]{2}[0-9]{2}(?:[ ]?[A-Z0-9]{4}){2,7}(?:[ ]?[A-Z0-9]{1,4})?\b",
            iban_valid,
        ),
        (Detection::Card, r"\b(?:[0-9][ -]?){12,18}[0-9]\b", luhn),
        (
            Detection::Ipv6,
            r"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b|(?i)\b(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}:?){1,6}\b",
            always,
        ),
        (
            Detection::Ipv4,
            r"\b(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])){3}\b",
            always,
        ),
        (
            Detection::Phone,
            r"(?:\+|\b00)?[0-9][0-9 ().-]{6,18}[0-9]\b",
            phone_plausible,
        ),
    ];
    let mut out: Vec<Detector> = builtin
        .into_iter()
        .map(|(kind, pattern, check)| Detector {
            kind,
            label: kind.label().to_owned(),
            pattern: Regex::new(pattern).expect("built-in pattern compiles"),
            check,
        })
        .collect();
    for (name, pattern) in extra {
        if let Ok(pattern) = Regex::new(pattern) {
            out.push(Detector {
                kind: Detection::Custom,
                label: crate::ids::sanitize(name),
                pattern,
                check: always,
            });
        }
    }
    out
}

/// Splits a field name into lowercase tokens on separators and camelCase.
pub fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// How a field is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldTreatment {
    /// On an allow-list: values pass untouched.
    Allowed,
    /// Personal or secret by name: every value is replaced.
    Redacted,
    /// Values pass through the detectors.
    Scanned,
}

/// A column to mask.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldInput {
    pub name: String,
    /// Grafana field type: `time`, `number`, `string`, `boolean`, ...
    pub field_type: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// A masked column description.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaskedField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub treatment: FieldTreatment,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

/// A masked table: what the agent is allowed to see.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MaskedTable {
    pub fields: Vec<MaskedField>,
    pub rows: Vec<Vec<Value>>,
    pub total_rows: usize,
    /// How many values of each kind were replaced.
    pub replaced: BTreeMap<String, usize>,
}

/// Replaces values with stable pseudonyms within one response.
#[derive(Default)]
struct Pseudonyms {
    seen: HashMap<(String, String), usize>,
    counters: HashMap<String, usize>,
    replaced: BTreeMap<String, usize>,
}

impl Pseudonyms {
    fn token(&mut self, label: &str, value: &str) -> String {
        *self.replaced.entry(label.to_owned()).or_default() += 1;
        let key = (label.to_owned(), value.to_owned());
        if let Some(number) = self.seen.get(&key) {
            return format!("<{label}#{number}>");
        }
        let counter = self.counters.entry(label.to_owned()).or_default();
        *counter += 1;
        let number = *counter;
        self.seen.insert(key, number);
        format!("<{label}#{number}>")
    }
}

/// The masking engine, built once from configuration.
pub struct Masker {
    detectors: Vec<Detector>,
    deny_tokens: Vec<String>,
    allow: Vec<String>,
    max_rows: usize,
    max_string_len: usize,
}

impl Masker {
    pub fn new(config: &MaskingConfig) -> Self {
        let mut deny_tokens: Vec<String> =
            PERSONAL_TOKENS.iter().map(|t| (*t).to_owned()).collect();
        deny_tokens.extend(
            config
                .deny_field_tokens
                .iter()
                .map(|t| t.to_ascii_lowercase()),
        );
        let mut allow: Vec<String> = DEFAULT_ALLOW.iter().map(|t| (*t).to_owned()).collect();
        allow.extend(config.allow_fields.iter().map(|t| t.to_ascii_lowercase()));
        Self {
            detectors: detectors(&config.extra_patterns),
            deny_tokens,
            allow,
            max_rows: config.max_rows,
            max_string_len: config.max_string_len,
        }
    }

    /// Decides how a field is treated under a datasource policy.
    pub fn treatment(&self, name: &str, policy: &DatasourcePolicy) -> FieldTreatment {
        let tokens = name_tokens(name);
        let secret = tokens.iter().any(|t| SECRET_TOKENS.contains(&t.as_str()));
        if secret {
            // Secrets are never allow-listed: a field literally named
            // `token` is redacted even on a non-personal datasource.
            return FieldTreatment::Redacted;
        }
        let lower = name.to_ascii_lowercase();
        let allowed = self.allow.contains(&lower)
            || policy
                .allow_fields
                .iter()
                .any(|field| field.eq_ignore_ascii_case(name));
        if allowed {
            return FieldTreatment::Allowed;
        }
        if policy.personal && tokens.iter().any(|t| self.deny_tokens.contains(t)) {
            return FieldTreatment::Redacted;
        }
        FieldTreatment::Scanned
    }

    fn scan(&self, text: &str, personal: bool, pseudonyms: &mut Pseudonyms) -> String {
        let mut out = text.to_owned();
        for detector in &self.detectors {
            if !personal && !detector.kind.always_on() {
                continue;
            }
            let mut result = String::with_capacity(out.len());
            let mut last = 0;
            for found in detector.pattern.find_iter(&out) {
                let candidate = found.as_str();
                // Already a pseudonym, or fails its checksum: leave it.
                if !(detector.check)(candidate) || is_inside_token(&out, found.start()) {
                    continue;
                }
                result.push_str(&out[last..found.start()]);
                result.push_str(&pseudonyms.token(&detector.label, candidate));
                last = found.end();
            }
            result.push_str(&out[last..]);
            out = result;
        }
        out
    }

    fn truncate(&self, text: String) -> String {
        let count = text.chars().count();
        if count <= self.max_string_len {
            return text;
        }
        let kept: String = text.chars().take(self.max_string_len).collect();
        format!("{kept}…(+{} chars)", count - self.max_string_len)
    }

    fn mask_value(
        &self,
        value: &Value,
        field: &FieldInput,
        treatment: FieldTreatment,
        personal: bool,
        pseudonyms: &mut Pseudonyms,
    ) -> Value {
        match treatment {
            FieldTreatment::Allowed => match value {
                Value::String(text) => Value::String(self.truncate(text.clone())),
                other => other.clone(),
            },
            FieldTreatment::Redacted => match value {
                Value::Null => Value::Null,
                other => {
                    let raw = match other {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let label = crate::ids::sanitize(&field.name);
                    let label = if label.is_empty() {
                        "field".to_owned()
                    } else {
                        label
                    };
                    Value::String(pseudonyms.token(&label, &raw))
                }
            },
            FieldTreatment::Scanned => match value {
                Value::String(text) => {
                    Value::String(self.truncate(self.scan(text, personal, pseudonyms)))
                }
                Value::Array(_) | Value::Object(_) => {
                    let text = value.to_string();
                    Value::String(self.truncate(self.scan(&text, personal, pseudonyms)))
                }
                // Numbers, booleans and timestamps are the metrics
                // themselves; they carry no identity (DASHR-PRIV-004).
                other => other.clone(),
            },
        }
    }

    fn mask_labels(
        &self,
        labels: &BTreeMap<String, String>,
        policy: &DatasourcePolicy,
        pseudonyms: &mut Pseudonyms,
    ) -> BTreeMap<String, String> {
        labels
            .iter()
            .map(|(key, value)| {
                let field = FieldInput {
                    name: key.clone(),
                    field_type: "string".to_owned(),
                    labels: BTreeMap::new(),
                };
                let treatment = self.treatment(key, policy);
                let masked = self.mask_value(
                    &Value::String(value.clone()),
                    &field,
                    treatment,
                    policy.personal,
                    pseudonyms,
                );
                (key.clone(), masked.as_str().unwrap_or_default().to_owned())
            })
            .collect()
    }

    /// Masks a table of rows under a datasource policy.
    ///
    /// At most `max_rows` rows are returned; `total_rows` says how many
    /// there were. Field names are kept: they are schema, and the agent
    /// needs them to write queries.
    pub fn mask_table(
        &self,
        fields: &[FieldInput],
        rows: &[Vec<Value>],
        policy: &DatasourcePolicy,
    ) -> MaskedTable {
        let mut pseudonyms = Pseudonyms::default();
        let treatments: Vec<FieldTreatment> = fields
            .iter()
            .map(|field| self.treatment(&field.name, policy))
            .collect();
        let masked_fields = fields
            .iter()
            .zip(&treatments)
            .map(|(field, treatment)| MaskedField {
                name: field.name.clone(),
                field_type: field.field_type.clone(),
                treatment: *treatment,
                labels: self.mask_labels(&field.labels, policy, &mut pseudonyms),
            })
            .collect();
        let masked_rows = rows
            .iter()
            .take(self.max_rows)
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(
                        |(index, value)| match (fields.get(index), treatments.get(index)) {
                            (Some(field), Some(treatment)) => self.mask_value(
                                value,
                                field,
                                *treatment,
                                policy.personal,
                                &mut pseudonyms,
                            ),
                            // A value without a field description is not trusted.
                            _ => Value::String("<unknown-field>".to_owned()),
                        },
                    )
                    .collect()
            })
            .collect();
        MaskedTable {
            fields: masked_fields,
            rows: masked_rows,
            total_rows: rows.len(),
            replaced: pseudonyms.replaced,
        }
    }

    /// Masks one free-standing string, such as a Grafana error message.
    ///
    /// Error messages from a datasource can echo the query and part of the
    /// data it choked on, so they are scanned like a personal field.
    pub fn mask_text(&self, text: &str) -> String {
        let mut pseudonyms = Pseudonyms::default();
        self.truncate(self.scan(text, true, &mut pseudonyms))
    }
}

/// Whether `index` falls inside an existing `<label#n>` pseudonym.
fn is_inside_token(text: &str, index: usize) -> bool {
    let before = &text[..index];
    match (before.rfind('<'), before.rfind('>')) {
        (Some(open), Some(close)) => open > close && text[open..].contains('#'),
        (Some(open), None) => text[open..].contains('#') && text[open..].contains('>'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(personal: bool) -> DatasourcePolicy {
        DatasourcePolicy {
            uid: "ds".into(),
            name: "DS".into(),
            plugin_type: "loki".into(),
            personal,
            allow_fields: vec!["region".into()],
        }
    }

    fn masker() -> Masker {
        Masker::new(&MaskingConfig::default())
    }

    fn field(name: &str, field_type: &str) -> FieldInput {
        FieldInput {
            name: name.into(),
            field_type: field_type.into(),
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn checksums() {
        assert!(luhn("4111 1111 1111 1111"));
        assert!(!luhn("4111 1111 1111 1112"));
        assert!(iban_valid("GB82 WEST 1234 5698 7654 32"));
        assert!(iban_valid("DE89370400440532013000"));
        assert!(!iban_valid("DE89370400440532013001"));
    }

    #[test]
    fn tokens_split_camel_case_and_separators() {
        assert_eq!(name_tokens("customerEmail"), vec!["customer", "email"]);
        assert_eq!(name_tokens("client_ip"), vec!["client", "ip"]);
        assert_eq!(name_tokens("__name__"), vec!["name"]);
        assert_eq!(name_tokens("HTTPStatus"), vec!["httpstatus"]);
    }

    #[test]
    fn personal_fields_are_redacted_with_stable_pseudonyms() {
        let fields = vec![field("email", "string"), field("count", "number")];
        let rows = vec![
            vec![json!("ann@example.com"), json!(3)],
            vec![json!("bob@example.com"), json!(4)],
            vec![json!("ann@example.com"), json!(5)],
        ];
        let table = masker().mask_table(&fields, &rows, &policy(true));
        assert_eq!(table.fields[0].treatment, FieldTreatment::Redacted);
        assert_eq!(table.rows[0][0], json!("<email#1>"));
        assert_eq!(table.rows[1][0], json!("<email#2>"));
        assert_eq!(table.rows[2][0], json!("<email#1>"));
        assert_eq!(table.rows[0][1], json!(3), "numbers pass");
        let dump = serde_json::to_string(&table).unwrap();
        assert!(!dump.contains("example.com"));
    }

    #[test]
    fn detectors_scrub_free_text() {
        let message = "login failed for ann@example.com from 10.1.2.3 card 4111-1111-1111-1111 \
                       iban DE89370400440532013000 call +36 30 123 4567 auth=Bearer abcdefghijkl123";
        let table = masker().mask_table(
            &[field("message", "string")],
            &[vec![json!(message)]],
            &policy(true),
        );
        let out = table.rows[0][0].as_str().unwrap();
        for leaked in [
            "ann@example.com",
            "10.1.2.3",
            "4111",
            "DE8937",
            "123 4567",
            "abcdefghijkl123",
        ] {
            assert!(!out.contains(leaked), "{leaked} leaked in {out}");
        }
        assert!(out.contains("<email#1>"));
        assert!(out.contains("<ipv4#1>"));
        assert!(out.contains("<card#1>"));
        assert!(out.contains("<iban#1>"));
        assert!(out.contains("<phone#1>"));
        assert!(out.starts_with("login failed for"), "{out}");
    }

    #[test]
    fn secrets_are_masked_even_on_non_personal_datasources() {
        let table = masker().mask_table(
            &[
                field("url", "string"),
                field("api_token", "string"),
                field("host_ip_note", "string"),
            ],
            &[vec![
                json!("GET /cb?token=s3cr3tvalue&x=1 eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.abcdefghij AKIAABCDEFGHIJKLMNOP"),
                json!("tok_live"),
                json!("10.0.0.1"),
            ]],
            &policy(false),
        );
        let url = table.rows[0][0].as_str().unwrap();
        assert!(!url.contains("s3cr3tvalue"), "{url}");
        assert!(!url.contains("eyJhbGci"), "{url}");
        assert!(!url.contains("AKIAABCD"), "{url}");
        assert_eq!(table.fields[1].treatment, FieldTreatment::Redacted);
        // Non-personal: IPs in scanned fields pass, personal field names too,
        // but emails and cards never do.
        let strict = masker().mask_table(
            &[field("note", "string")],
            &[vec![json!("ann@example.com paid with 4111 1111 1111 1111")]],
            &policy(false),
        );
        assert_eq!(strict.rows[0][0], json!("<email#1> paid with <card#1>"));
        assert_eq!(table.fields[2].treatment, FieldTreatment::Scanned);
        assert_eq!(table.rows[0][2], json!("10.0.0.1"));
    }

    #[test]
    fn allow_lists_apply_but_never_to_secrets() {
        let masker = masker();
        let p = policy(true);
        assert_eq!(masker.treatment("__name__", &p), FieldTreatment::Allowed);
        assert_eq!(masker.treatment("region", &p), FieldTreatment::Allowed);
        assert_eq!(masker.treatment("Region", &p), FieldTreatment::Allowed);
        assert_eq!(masker.treatment("userName", &p), FieldTreatment::Redacted);
        let mut p = p;
        p.allow_fields.push("session_token".into());
        assert_eq!(
            masker.treatment("session_token", &p),
            FieldTreatment::Redacted
        );
    }

    #[test]
    fn labels_are_masked_by_key_and_value() {
        let mut labels = BTreeMap::new();
        labels.insert("user".to_owned(), "ann".to_owned());
        labels.insert("job".to_owned(), "api".to_owned());
        labels.insert("path".to_owned(), "/u/ann@example.com".to_owned());
        let fields = vec![FieldInput {
            name: "value".into(),
            field_type: "number".into(),
            labels,
        }];
        let table = masker().mask_table(&fields, &[vec![json!(1.5)]], &policy(true));
        let labels = &table.fields[0].labels;
        assert_eq!(labels["user"], "<user#1>");
        assert_eq!(labels["job"], "api");
        assert_eq!(labels["path"], "/u/<email#1>");
    }

    #[test]
    fn rows_are_capped_and_long_strings_truncated() {
        let config = MaskingConfig {
            max_rows: 2,
            max_string_len: 10,
            ..MaskingConfig::default()
        };
        let masker = Masker::new(&config);
        let rows: Vec<Vec<Value>> = (0..5).map(|_| vec![json!("abcdefghijklmnop")]).collect();
        let table = masker.mask_table(&[field("msg", "string")], &rows, &policy(true));
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.total_rows, 5);
        assert_eq!(table.rows[0][0], json!("abcdefghij…(+6 chars)"));
    }

    #[test]
    fn extra_patterns_apply() {
        let mut config = MaskingConfig::default();
        config
            .extra_patterns
            .insert("order id".into(), r"ORD-\d{6}".into());
        let masker = Masker::new(&config);
        assert_eq!(masker.mask_text("see ORD-123456"), "see <order-id#1>");
    }

    #[test]
    fn nested_values_and_error_text_are_scanned() {
        let table = masker().mask_table(
            &[field("payload", "other")],
            &[vec![json!({"contact": "ann@example.com"})]],
            &policy(true),
        );
        assert!(!table.rows[0][0].as_str().unwrap().contains("ann@"));
        assert_eq!(
            masker().mask_text("bad row: ann@example.com"),
            "bad row: <email#1>"
        );
    }

    #[test]
    fn ordinary_numbers_in_text_are_not_phones() {
        let out = masker().mask_text("took 1234 ms, status 200, 2026-09-25T10:00:00Z");
        assert_eq!(out, "took 1234 ms, status 200, 2026-09-25T10:00:00Z");
    }

    #[test]
    fn values_without_field_descriptions_are_not_trusted() {
        let table = masker().mask_table(
            &[field("a", "number")],
            &[vec![json!(1), json!("ann@example.com")]],
            &policy(true),
        );
        assert_eq!(table.rows[0][1], json!("<unknown-field>"));
    }
}
