//! Parsing CodePipeline console URLs.

use serde::Serialize;

/// A pipeline named by a console URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelineRef {
    pub region: String,
    pub name: String,
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("not an AWS console URL: {0}")]
    NotConsole(String),
    #[error("no CodePipeline pipeline name in {0}")]
    NoPipeline(String),
    #[error("no region in {0}; add ?region=<region>")]
    NoRegion(String),
}

fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&segment[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| decode(value))
        .filter(|value| !value.is_empty())
}

fn valid_region(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 25
        && region
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && region.contains('-')
}

/// Pipeline names: `[A-Za-z0-9.@_-]{1,100}`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".@_-".contains(c))
}

/// Parses the console URL forms seen in the wild:
///
/// * `https://eu-west-1.console.aws.amazon.com/codesuite/codepipeline/pipelines/<name>/view?region=eu-west-1`
/// * `.../pipelines/<name>/executions/<id>/timeline?region=...`
/// * `https://console.aws.amazon.com/codepipeline/home?region=us-east-1#/view/<name>`
pub fn parse(url: &str) -> Result<PipelineRef, UrlError> {
    let rest = url
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| UrlError::NotConsole(url.to_owned()))?;
    let (host, path_and_more) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host_region = if host == "console.aws.amazon.com" {
        None
    } else if let Some(prefix) = host.strip_suffix(".console.aws.amazon.com") {
        Some(prefix.to_owned())
    } else {
        return Err(UrlError::NotConsole(url.to_owned()));
    };

    let (before_fragment, fragment) = match path_and_more.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (path_and_more, None),
    };
    let (path, query) = before_fragment
        .split_once('?')
        .unwrap_or((before_fragment, ""));

    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (name, execution_id) = if let Some(position) = segments
        .windows(3)
        .position(|w| w == ["codesuite", "codepipeline", "pipelines"])
    {
        let name = segments
            .get(position + 3)
            .map(|s| decode(s))
            .ok_or_else(|| UrlError::NoPipeline(url.to_owned()))?;
        let execution = match segments.get(position + 4) {
            Some(&"executions") => segments.get(position + 5).map(|s| decode(s)),
            _ => None,
        };
        (name, execution)
    } else if segments.starts_with(&["codepipeline", "home"]) {
        let fragment = fragment.unwrap_or("");
        let name = fragment
            .trim_start_matches('/')
            .strip_prefix("view/")
            .map(|name| decode(name.split(['/', '?']).next().unwrap_or("")))
            .ok_or_else(|| UrlError::NoPipeline(url.to_owned()))?;
        (name, None)
    } else {
        return Err(UrlError::NoPipeline(url.to_owned()));
    };
    if !valid_name(&name) {
        return Err(UrlError::NoPipeline(url.to_owned()));
    }

    let region = query_param(query, "region")
        .or(host_region)
        .filter(|region| valid_region(region))
        .ok_or_else(|| UrlError::NoRegion(url.to_owned()))?;
    Ok(PipelineRef {
        region,
        name,
        execution_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_console_view() {
        let pipeline = parse(
            "https://eu-west-1.console.aws.amazon.com/codesuite/codepipeline/pipelines/api-deploy/view?region=eu-west-1",
        )
        .unwrap();
        assert_eq!(pipeline.region, "eu-west-1");
        assert_eq!(pipeline.name, "api-deploy");
        assert_eq!(pipeline.execution_id, None);
    }

    #[test]
    fn execution_timeline() {
        let pipeline = parse(
            "https://console.aws.amazon.com/codesuite/codepipeline/pipelines/feature-x_deploy/executions/0f2c-11aa/timeline?region=us-east-2",
        )
        .unwrap();
        assert_eq!(pipeline.region, "us-east-2");
        assert_eq!(pipeline.name, "feature-x_deploy");
        assert_eq!(pipeline.execution_id.as_deref(), Some("0f2c-11aa"));
    }

    #[test]
    fn old_console_fragment() {
        let pipeline = parse(
            "https://console.aws.amazon.com/codepipeline/home?region=ap-southeast-2#/view/legacy",
        )
        .unwrap();
        assert_eq!(pipeline.region, "ap-southeast-2");
        assert_eq!(pipeline.name, "legacy");
    }

    #[test]
    fn region_from_host_when_query_lacks_it() {
        let pipeline = parse(
            "https://eu-central-1.console.aws.amazon.com/codesuite/codepipeline/pipelines/p/view",
        )
        .unwrap();
        assert_eq!(pipeline.region, "eu-central-1");
    }

    #[test]
    fn rejects_other_urls() {
        assert!(matches!(
            parse("https://github.com/a/b"),
            Err(UrlError::NotConsole(_))
        ));
        assert!(matches!(
            parse("https://console.aws.amazon.com.evil.example/codepipeline/home"),
            Err(UrlError::NotConsole(_))
        ));
        assert!(matches!(
            parse("https://console.aws.amazon.com/s3/home?region=eu-west-1"),
            Err(UrlError::NoPipeline(_))
        ));
        assert!(matches!(
            parse("https://console.aws.amazon.com/codesuite/codepipeline/pipelines/p/view"),
            Err(UrlError::NoRegion(_))
        ));
        assert!(matches!(
            parse(
                "https://console.aws.amazon.com/codesuite/codepipeline/pipelines/a;rm/view?region=eu-west-1"
            ),
            Err(UrlError::NoPipeline(_))
        ));
    }
}
