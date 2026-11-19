//! Drift check: the mock gateway follows the spec pinned in `fogar_testkit::gateway`. With
//! `FOGAR_GATEWAY_DIR` pointing at a fogar-gateway checkout, fail if its
//! `docs/client-protocol.md` is not that one. Without it there is nothing to compare: skipped.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use fogar_testkit::gateway::{SPEC_BLOB, SPEC_GATEWAY_COMMIT};

#[test]
fn mock_follows_the_gateway_spec() {
    let Some(dir) = std::env::var_os("FOGAR_GATEWAY_DIR") else {
        eprintln!("FOGAR_GATEWAY_DIR not set: spec drift check skipped");
        return;
    };
    let doc = std::path::Path::new(&dir).join("docs/client-protocol.md");
    let out = std::process::Command::new("git")
        .arg("hash-object")
        .arg(&doc)
        .output()
        .expect("git");
    assert!(out.status.success(), "git hash-object {}", doc.display());
    let blob = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        blob.trim(),
        SPEC_BLOB,
        "the gateway spec changed since the mock was checked. Review \
         `git -C {} diff {SPEC_GATEWAY_COMMIT} -- docs/client-protocol.md`, update the mock \
         (crates/fogar-testkit/src/gateway), then bump SPEC_GATEWAY_COMMIT and SPEC_BLOB.",
        dir.to_string_lossy()
    );
}
