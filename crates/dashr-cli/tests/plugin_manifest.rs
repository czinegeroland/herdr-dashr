//! DASHR-HERDR-001: the checked-in manifest is exactly the generator output.

use std::process::Command;

#[test]
fn checked_in_manifest_matches_the_generator() {
    let output = Command::new(env!("CARGO_BIN_EXE_dashr"))
        .args(["herdr", "manifest"])
        .output()
        .expect("dashr runs");
    assert!(output.status.success());
    let generated = String::from_utf8(output.stdout).expect("utf-8");
    let checked_in = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../herdr-plugin.toml"
    ))
    .expect("herdr-plugin.toml exists");
    assert_eq!(
        checked_in, generated,
        "herdr-plugin.toml is stale; run `cargo run -q --bin dashr -- herdr manifest > herdr-plugin.toml`"
    );
}

#[test]
fn manifest_version_matches_the_crate() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../herdr-plugin.toml"
    ))
    .unwrap();
    assert!(text.contains(&format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))));
}
