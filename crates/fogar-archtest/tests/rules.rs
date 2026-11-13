//! Hexagonal dependency rules (fogar-hexagonal-slices §2) as plain tests. Transitive rules use
//! `cargo tree -p <crate>` (features resolved per package), NOT `cargo metadata`'s resolve graph,
//! which unifies features across the whole workspace and reports false edges.
//!
//!  R1 inner crates never reach adapter / host / composition-root crates (normal+build, all targets)
//!  R2 test-support crates (testkit, testalloc) are only ever dev-dependencies
//!  R3 only adapter crates and the composition root declare target-specific deps
//!     (exception: `cfg(fogar_loom)` dev-dependencies, used by the loom gate)
//!  R4 inner crates never reach OS-binding or I/O-driver crates, even transitively
//!  R5 no platform cfg (`target_os`, `cfg(unix)`, `cfg_select!`...) in inner crates' sources
//!  R6 fogar-proto: dependency allowlist, every dep with default-features = false
//!  R7 every inner crate has a clippy.toml banning ambient time/threads/env/fs
//!  R8 no `unsafe` in inner crates, and each declares `#![forbid(unsafe_code)]`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

const INNER: &[&str] = &["fogar-proto", "fogar-ports", "fogar-core"];
const OUTER: &[&str] = &["fogar-host", "fogar-agentd"];
const ADAPTER_PREFIX: &str = "fogar-agent-";
const TEST_SUPPORT: &[&str] = &["fogar-testkit", "fogar-testalloc"];
const TARGET_DEPS_ALLOWED: &[&str] = &["fogar-agentd"];
const OS_CRATE_PREFIXES: &[&str] = &[
    "libc", "nix", "rustix", "mio", "socket2", "signal-hook", "objc2", "block2", "dispatch2", "core-foundation",
    "core-graphics", "io-kit", "security-framework", "windows", "windows-sys", "winapi", "zbus", "cpal", "coreaudio",
];
const PROTO_ALLOWED: &[&str] = &[
    "serde", "serde_core", "serde_derive", "serde_json", "itoa", "memchr", "zmij", "ryu", "proc-macro2", "quote",
    "syn", "unicode-ident",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn cargo(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO")).args(args).current_dir(root()).output().expect("run cargo");
    assert!(out.status.success(), "cargo {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn metadata() -> Value {
    serde_json::from_str(&cargo(&["metadata", "--format-version", "1", "--no-deps"])).unwrap()
}

/// Package names reachable from `krate` via normal + build edges, all targets, per-package features.
fn closure(krate: &str) -> BTreeSet<String> {
    cargo(&["tree", "-p", krate, "--target", "all", "-e", "normal,build", "--prefix", "none", "-f", "{p}"])
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

fn is_os_crate(n: &str) -> bool {
    OS_CRATE_PREFIXES.iter().any(|p| n == *p || n.starts_with(&format!("{p}-")))
}

fn rust_sources(krate: &str) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![root().join("crates").join(krate).join("src")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                out.push((p, text));
            }
        }
    }
    out
}

#[test]
fn r1_inner_never_reaches_outer_crates() {
    for c in INNER {
        let bad: Vec<_> =
            closure(c).into_iter().filter(|n| n.starts_with(ADAPTER_PREFIX) || OUTER.contains(&n.as_str())).collect();
        assert!(bad.is_empty(), "{c} depends on outer crates {bad:?}");
    }
}

#[test]
fn r2_test_support_only_as_dev_dependency() {
    for p in metadata()["packages"].as_array().unwrap() {
        for d in p["dependencies"].as_array().unwrap() {
            if TEST_SUPPORT.contains(&d["name"].as_str().unwrap()) {
                assert_eq!(d["kind"].as_str(), Some("dev"), "{} has {} as a non-dev dependency", p["name"], d["name"]);
            }
        }
    }
    // And nothing reaches them transitively through normal edges.
    for c in ["fogar-agentd", "fogar-host", "fogar-agent-linux", "fogar-agent-macos"] {
        let bad: Vec<_> = closure(c).into_iter().filter(|n| TEST_SUPPORT.contains(&n.as_str())).collect();
        assert!(bad.is_empty(), "{c} reaches test support {bad:?}");
    }
}

#[test]
fn r3_only_adapters_and_root_have_target_specific_deps() {
    for p in metadata()["packages"].as_array().unwrap() {
        let name = p["name"].as_str().unwrap();
        if name.starts_with(ADAPTER_PREFIX) || TARGET_DEPS_ALLOWED.contains(&name) {
            continue;
        }
        for d in p["dependencies"].as_array().unwrap() {
            let target = &d["target"];
            if target.is_null() {
                continue;
            }
            let loom_dev = target.as_str() == Some("cfg(fogar_loom)") && d["kind"].as_str() == Some("dev");
            assert!(loom_dev, "{name}: target-specific dep {} ({target})", d["name"]);
        }
    }
}

#[test]
fn r4_inner_never_reaches_os_crates() {
    for c in INNER {
        let bad: Vec<_> = closure(c).into_iter().filter(|n| is_os_crate(n)).collect();
        assert!(bad.is_empty(), "{c} reaches OS / I/O crates {bad:?}");
    }
}

#[test]
fn r5_no_platform_cfg_in_inner_sources() {
    let needles = ["target_os", "target_family", "target_vendor", "cfg(windows", "cfg(unix", "cfg_select!"];
    for c in INNER {
        for (p, text) in rust_sources(c) {
            for n in needles {
                assert!(!text.contains(n), "{}: contains `{n}` (platform cfg belongs in adapters)", p.display());
            }
        }
    }
}

#[test]
fn r6_proto_stays_lean_and_no_std_capable() {
    let md = metadata();
    let proto = md["packages"].as_array().unwrap().iter().find(|p| p["name"] == "fogar-proto").unwrap();
    for d in proto["dependencies"].as_array().unwrap() {
        if d["kind"].as_str() == Some("dev") {
            continue;
        }
        assert_eq!(d["uses_default_features"], Value::Bool(false), "fogar-proto dep {} must disable default features", d["name"]);
    }
    let extra: Vec<_> = closure("fogar-proto").into_iter().filter(|n| !PROTO_ALLOWED.contains(&n.as_str())).collect();
    assert!(extra.is_empty(), "fogar-proto pulls non-allowlisted crates {extra:?}");
    let lib = std::fs::read_to_string(root().join("crates/fogar-proto/src/lib.rs")).unwrap();
    assert!(lib.contains("#![no_std]"), "fogar-proto must stay #![no_std]");
}

#[test]
fn r7_inner_crates_have_clippy_bans() {
    for c in INNER {
        let p = root().join("crates").join(c).join("clippy.toml");
        let text = std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("{} missing", p.display()));
        for must in ["std::time::Instant::now", "std::thread::sleep", "std::env::var", "std::fs::File"] {
            assert!(text.contains(must), "{}: must ban {must}", p.display());
        }
    }
    let core = std::fs::read_to_string(root().join("crates/fogar-core/clippy.toml")).unwrap();
    for must in ["tokio::spawn", "tokio::time::sleep", "unbounded_channel"] {
        assert!(core.contains(must), "fogar-core/clippy.toml must ban {must}");
    }
    let ws = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    for lint in ["disallowed_methods = \"deny\"", "disallowed_types = \"deny\"", "unwrap_used = \"deny\""] {
        assert!(ws.contains(lint), "workspace lints must keep {lint}");
    }
}

#[test]
fn r8_no_unsafe_in_inner_crates() {
    for c in INNER {
        let lib = std::fs::read_to_string(root().join("crates").join(c).join("src/lib.rs")).unwrap();
        assert!(lib.contains("#![forbid(unsafe_code)]"), "{c}: missing #![forbid(unsafe_code)]");
        for (p, text) in rust_sources(c) {
            for (i, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                assert!(!code.contains("unsafe "), "{}:{}: `unsafe` in an inner crate", p.display(), i + 1);
            }
        }
    }
}
