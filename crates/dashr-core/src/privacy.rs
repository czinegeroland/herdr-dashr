//! What the agent may see of a span: pseudonyms instead of personal data
//! and secrets (DASHR-PRIV-001).
//!
//! The human sees raw values in their own browser. Everything that reaches
//! the agent — trace trees, sequences, verdicts, source trial reports —
//! passes through [`Masker`]: emails, card numbers, IBANs, phone numbers,
//! IP addresses, tokens and keys become stable pseudonyms such as
//! `<email#1>`, so equal values stay visibly equal and an id can still be
//! followed across services. Attributes whose name marks them as personal
//! (`customer.email`, `user.name`) or secret (`auth.token`) are replaced
//! whole. Expected values the agent wrote itself are compared against the
//! raw values inside dashr; only the masked actual value is reported.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
/// OpenTelemetry semantic-convention namespaces. Their attributes are
/// scanned for personal values but never replaced by name: `service.name`
/// and `db.name` are not a person's name.
const SEMCONV_NAMESPACES: &[&str] = &[
    "service",
    "telemetry",
    "otel",
    "http",
    "url",
    "server",
    "client",
    "network",
    "net",
    "db",
    "rpc",
    "messaging",
    "faas",
    "cloud",
    "aws",
    "gcp",
    "azure",
    "az",
    "k8s",
    "container",
    "host",
    "os",
    "process",
    "deployment",
    "code",
    "exception",
    "error",
    "thread",
    "span",
    "peer",
    "event",
    "graphql",
    "feature_flag",
    "browser",
    "device",
    "user_agent",
    "xray",
    "appinsights",
    "dashr",
    "test",
    "flow",
];

/// Masking settings (`[masking]` in dashr's configuration file).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaskingConfig {
    /// Off only for synthetic data you are happy for the agent to read.
    pub enabled: bool,
    /// Attribute names that always pass unmasked.
    pub allow_keys: Vec<String>,
    /// Extra name tokens that mark an attribute as personal.
    pub deny_tokens: Vec<String>,
    /// Extra value patterns (regular expressions) to replace, by label.
    pub extra_patterns: BTreeMap<String, String>,
    /// Strings longer than this are cut after masking.
    pub max_string_len: usize,
}

impl Default for MaskingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_keys: Vec::new(),
            deny_tokens: Vec::new(),
            extra_patterns: BTreeMap::new(),
            max_string_len: 200,
        }
    }
}

/// Lowercase letters, digits and `_`; everything else becomes `_`.
pub fn sanitize(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let out = out.trim_matches('_').to_owned();
    if out.is_empty() {
        "value".to_owned()
    } else {
        out
    }
}

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
}

struct Detector {
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
            label: kind.label().to_owned(),
            pattern: Regex::new(pattern).expect("built-in pattern compiles"),
            check,
        })
        .collect();
    for (name, pattern) in extra {
        if let Ok(pattern) = Regex::new(pattern) {
            out.push(Detector {
                label: sanitize(name),
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

/// How an attribute is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    Allowed,
    Replaced,
    Scanned,
}

/// Replaces values with stable pseudonyms within one response.
#[derive(Default)]
pub struct Pseudonyms {
    seen: HashMap<(String, String), usize>,
    counters: HashMap<String, usize>,
    replaced: BTreeMap<String, usize>,
}

impl Pseudonyms {
    pub fn token(&mut self, label: &str, value: &str) -> String {
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
    enabled: bool,
    detectors: Vec<Detector>,
    deny_tokens: Vec<String>,
    allow: Vec<String>,
    max_string_len: usize,
}

impl Masker {
    pub fn new(config: &MaskingConfig) -> Self {
        let mut deny_tokens: Vec<String> =
            PERSONAL_TOKENS.iter().map(|t| (*t).to_owned()).collect();
        deny_tokens.extend(config.deny_tokens.iter().map(|t| t.to_ascii_lowercase()));
        Self {
            enabled: config.enabled,
            detectors: detectors(&config.extra_patterns),
            deny_tokens,
            allow: config
                .allow_keys
                .iter()
                .map(|k| k.to_ascii_lowercase())
                .collect(),
            max_string_len: config.max_string_len,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// How the attribute `key` is treated.
    pub fn treatment(&self, key: &str) -> Treatment {
        let tokens = name_tokens(key);
        if tokens.iter().any(|t| SECRET_TOKENS.contains(&t.as_str())) {
            return Treatment::Replaced;
        }
        if self.allow.contains(&key.to_ascii_lowercase()) {
            return Treatment::Allowed;
        }
        let namespace = key
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if key.contains('.') && SEMCONV_NAMESPACES.contains(&namespace.as_str()) {
            return Treatment::Scanned;
        }
        if tokens.iter().any(|t| self.deny_tokens.contains(t)) {
            return Treatment::Replaced;
        }
        Treatment::Scanned
    }

    fn scan(&self, text: &str, pseudonyms: &mut Pseudonyms) -> String {
        let mut out = text.to_owned();
        for detector in &self.detectors {
            let mut result = String::with_capacity(out.len());
            let mut last = 0;
            for found in detector.pattern.find_iter(&out) {
                let candidate = found.as_str();
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

    /// Masks a free-standing string: an error message, a span name.
    pub fn text(&self, text: &str, pseudonyms: &mut Pseudonyms) -> String {
        if !self.enabled {
            return self.truncate(text.to_owned());
        }
        self.truncate(self.scan(text, pseudonyms))
    }

    /// Masks one attribute value.
    pub fn value(&self, key: &str, value: &Value, pseudonyms: &mut Pseudonyms) -> Value {
        if !self.enabled {
            return match value {
                Value::String(text) => Value::String(self.truncate(text.clone())),
                other => other.clone(),
            };
        }
        match (self.treatment(key), value) {
            (_, Value::Null) => Value::Null,
            (Treatment::Allowed, Value::String(text)) => Value::String(self.truncate(text.clone())),
            (Treatment::Allowed, other) => other.clone(),
            (Treatment::Replaced, other) => {
                let raw = match other {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                Value::String(pseudonyms.token(&sanitize(key), &raw))
            }
            (Treatment::Scanned, Value::String(text)) => {
                Value::String(self.truncate(self.scan(text, pseudonyms)))
            }
            (Treatment::Scanned, Value::Array(_) | Value::Object(_)) => {
                Value::String(self.truncate(self.scan(&value.to_string(), pseudonyms)))
            }
            // Numbers and booleans are counts, sizes and flags.
            (Treatment::Scanned, other) => other.clone(),
        }
    }

    /// Masks a whole attribute map.
    pub fn attributes(
        &self,
        attributes: &BTreeMap<String, Value>,
        pseudonyms: &mut Pseudonyms,
    ) -> BTreeMap<String, Value> {
        attributes
            .iter()
            .map(|(key, value)| (key.clone(), self.value(key, value, pseudonyms)))
            .collect()
    }
}

impl Default for Masker {
    fn default() -> Self {
        Self::new(&MaskingConfig::default())
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

    #[test]
    fn checksums() {
        assert!(luhn("4111 1111 1111 1111"));
        assert!(!luhn("4111 1111 1111 1112"));
        assert!(iban_valid("GB82 WEST 1234 5698 7654 32"));
        assert!(!iban_valid("DE89370400440532013001"));
    }

    #[test]
    fn semantic_conventions_are_not_names() {
        let masker = Masker::default();
        let mut p = Pseudonyms::default();
        assert_eq!(
            masker.value("service.name", &json!("orders"), &mut p),
            json!("orders")
        );
        assert_eq!(
            masker.value("db.name", &json!("shop"), &mut p),
            json!("shop")
        );
        assert_eq!(
            masker.value("http.route", &json!("/orders/{id}"), &mut p),
            json!("/orders/{id}")
        );
        assert_eq!(masker.value("order.items", &json!(3), &mut p), json!(3));
    }

    #[test]
    fn personal_and_secret_attributes_become_pseudonyms() {
        let masker = Masker::default();
        let mut p = Pseudonyms::default();
        assert_eq!(
            masker.value("customer.email", &json!("ann@example.com"), &mut p),
            json!("<customer_email#1>")
        );
        assert_eq!(
            masker.value("customer.email", &json!("ann@example.com"), &mut p),
            json!("<customer_email#1>")
        );
        assert_eq!(
            masker.value("auth.token", &json!("abc"), &mut p),
            json!("<auth_token#1>")
        );
        assert_eq!(
            masker.value(
                "url.full",
                &json!("https://x/api?email=bob@example.com"),
                &mut p
            ),
            json!("https://x/api?email=<email#1>")
        );
        assert_eq!(
            masker.value(
                "note",
                &json!("card 4111 1111 1111 1111 from 10.1.2.3"),
                &mut p
            ),
            json!("card <card#1> from <ipv4#1>")
        );
    }

    #[test]
    fn masking_can_be_switched_off_for_synthetic_data() {
        let masker = Masker::new(&MaskingConfig {
            enabled: false,
            ..MaskingConfig::default()
        });
        let mut p = Pseudonyms::default();
        assert_eq!(
            masker.value("customer.email", &json!("ann@example.com"), &mut p),
            json!("ann@example.com")
        );
    }
}
