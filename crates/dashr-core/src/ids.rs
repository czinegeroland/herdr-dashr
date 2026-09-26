//! Identifiers derived from Herdr's pane and socket identity.
//!
//! Herdr pane ids such as `w1:p3` are only unique inside one Herdr server:
//! every named session has its own `w1:p1`. A container label of the bare
//! pane id would let the startup reaper of one session stop the Grafana of
//! another (decision DEC-009), so every session id also carries a short,
//! stable hash of the socket path the pane's server listens on.

/// 64-bit FNV-1a. Stable across platforms and releases, which a std hasher
/// is not, and small enough not to need a dependency.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Eight hex characters identifying one Herdr server by its socket path.
pub fn socket_hash(socket_path: &str) -> String {
    format!("{:08x}", fnv1a64(socket_path.as_bytes()) as u32)
}

/// Replaces every character Docker and file systems dislike with `-`.
///
/// Docker container names allow `[a-zA-Z0-9][a-zA-Z0-9_.-]`; the result is
/// also safe as a file name and as a Grafana dashboard uid.
pub fn sanitize(value: &str) -> String {
    let mut out: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '.' {
                character
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_ascii_lowercase()
}

/// The session id for one dashboard pane: `<socket hash>-<pane id>`.
pub fn session_id(socket_path: &str, pane_id: &str) -> String {
    format!("{}-{}", socket_hash(socket_path), sanitize(pane_id))
}

/// The Docker container name for a session.
pub fn container_name(session_id: &str) -> String {
    format!("herdr-grafana-{session_id}")
}

/// The Grafana dashboard uid owned by a session.
///
/// Grafana caps uids at 40 characters, so a long session id is shortened to
/// its hash rather than truncated into a collision.
pub fn dashboard_uid(session_id: &str) -> String {
    let uid = format!("dashr-{session_id}");
    if uid.len() <= 40 {
        uid
    } else {
        format!("dashr-{:016x}", fnv1a64(session_id.as_bytes()))
    }
}

/// A url-safe slug of a display name, used for datasource uids.
pub fn slug(name: &str) -> String {
    let slug = sanitize(name).replace('.', "-");
    if slug.is_empty() {
        format!("ds-{:08x}", fnv1a64(name.as_bytes()) as u32)
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_matches_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn session_ids_differ_between_servers_for_the_same_pane() {
        let one = session_id("/run/a/herdr.sock", "w1:p1");
        let two = session_id("/run/b/herdr.sock", "w1:p1");
        assert_ne!(one, two);
        assert!(one.ends_with("-w1-p1"));
    }

    #[test]
    fn sanitize_produces_docker_safe_names() {
        assert_eq!(sanitize("w1:p3"), "w1-p3");
        assert_eq!(sanitize("::A b//c::"), "a-b-c");
        let name = container_name(&session_id("/s", "w12:p7"));
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        );
    }

    #[test]
    fn dashboard_uids_fit_grafana_limit() {
        assert_eq!(dashboard_uid("abc-w1-p1"), "dashr-abc-w1-p1");
        let long = dashboard_uid(&"x".repeat(80));
        assert!(long.len() <= 40);
        assert!(long.starts_with("dashr-"));
    }

    #[test]
    fn slug_never_returns_empty() {
        assert_eq!(slug("CloudWatch (eu-west-1)"), "cloudwatch-eu-west-1");
        assert!(slug("***").starts_with("ds-"));
    }
}
