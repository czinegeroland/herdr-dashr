//! DASHR-TECH-005: the four places that list release platforms agree.
//!
//! The release matrix builds archives, `scripts/build-npm-packages.mjs`
//! packs them, `npm/dashr/bin.js` resolves them at run time and
//! `scripts/install.sh` picks one for the plugin. A platform added to one and
//! not the others would publish a package nobody can resolve, or install
//! nothing on a machine the release supports.

use std::collections::BTreeSet;

fn read(path: &str) -> String {
    std::fs::read_to_string(format!("{}/../../{path}", env!("CARGO_MANIFEST_DIR")))
        .unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// Every `x86_64-...`/`aarch64-...` target triple mentioned in a file.
fn triples(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|word| {
            (word.starts_with("x86_64-") || word.starts_with("aarch64-"))
                && (word.ends_with("-gnu") || word.ends_with("-darwin"))
        })
        .map(str::to_owned)
        .collect()
}

/// Every `linux-x64`-style npm suffix mentioned in a file.
fn suffixes(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| {
            ["linux-", "darwin-"].iter().any(|os| word.starts_with(os))
                && (word.ends_with("-x64") || word.ends_with("-arm64"))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn release_packaging_shim_and_installer_list_the_same_platforms() {
    let release = triples(&read(".github/workflows/release.yml"));
    let packaging = read("scripts/build-npm-packages.mjs");
    let installer = read("scripts/install.sh");
    let shim = read("npm/dashr/bin.js");

    assert_eq!(release.len(), 4, "release matrix: {release:?}");
    assert_eq!(
        triples(&packaging),
        release,
        "build-npm-packages.mjs targets"
    );
    assert_eq!(triples(&installer), release, "install.sh targets");

    let npm = suffixes(&packaging);
    assert_eq!(npm.len(), 4, "npm suffixes: {npm:?}");
    assert_eq!(suffixes(&shim), npm, "bin.js PACKAGES");
    assert_eq!(suffixes(&installer), npm, "install.sh SUFFIX values");
}

#[test]
fn package_names_and_archive_names_match_the_release() {
    let packaging = read("scripts/build-npm-packages.mjs");
    let shim = read("npm/dashr/bin.js");
    let release = read(".github/workflows/release.yml");
    let installer = read("scripts/install.sh");
    assert!(packaging.contains("const ROOT_PACKAGE = 'herdr-dashr'"));
    assert!(shim.contains("`herdr-dashr-${suffix}`"));
    assert!(installer.contains("herdr-dashr-$SUFFIX/dashr"));
    // The archive name the release writes is the one both consumers read.
    assert!(release.contains(r#"archive="dashr-${VERSION}-${{ matrix.target }}.tar.gz""#));
    assert!(packaging.contains("`dashr-${version}-${platform.target}.tar.gz`"));
    assert!(installer.contains(r#"archive="dashr-${VERSION}-${TARGET}.tar.gz""#));
}

#[test]
fn npm_publishing_follows_the_release_and_needs_a_token() {
    let workflow = read(".github/workflows/npm-publish.yml");
    assert!(workflow.contains("workflows: [Release]"));
    assert!(workflow.contains("NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}"));
    assert!(workflow.contains("scripts/build-npm-packages.mjs"));
    // Platform packages go out before the root package that depends on them.
    let platforms = workflow.find(r#"for dir in npm-dist/*/; do"#).unwrap();
    let root = workflow.rfind("publish npm-dist/herdr-dashr").unwrap();
    assert!(platforms < root);
}
