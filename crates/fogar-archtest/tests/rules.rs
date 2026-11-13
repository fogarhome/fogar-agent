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
//!  R9 adapter crates (fogar-host, fogar-agent-*) never reach fogar-core
//!  R10 fogar-core sources never name the `tokio::time` module at all
//!
//! Every checker has a self-test (`selftest_*`) that plants the violation in a temporary
//! fixture and asserts the rule fires (review code#4: a rule that cannot fail proves nothing).
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
    "libc",
    "nix",
    "rustix",
    "mio",
    "socket2",
    "signal-hook",
    "objc2",
    "block2",
    "dispatch2",
    "core-foundation",
    "core-graphics",
    "io-kit",
    "security-framework",
    "windows",
    "windows-sys",
    "winapi",
    "zbus",
    "cpal",
    "coreaudio",
];
/// Entries fogar-core/clippy.toml must carry (R7).
const CORE_CLIPPY_BANS: &[&str] = &[
    "tokio::spawn",
    "unbounded_channel",
    "tokio::time::sleep",
    "tokio::time::sleep_until",
    "tokio::time::timeout",
    "tokio::time::timeout_at",
    "tokio::time::interval",
    "tokio::time::interval_at",
    "tokio::time::Instant",
    "tokio::time::Sleep",
];
const PROTO_ALLOWED: &[&str] = &[
    "serde",
    "serde_core",
    "serde_derive",
    "serde_json",
    "itoa",
    "memchr",
    "zmij",
    "ryu",
    "proc-macro2",
    "quote",
    "syn",
    "unicode-ident",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn cargo(args: &[&str]) -> String {
    cargo_in(&root(), args)
}

fn cargo_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run cargo");
    assert!(
        out.status.success(),
        "cargo {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn metadata() -> Value {
    serde_json::from_str(&cargo(&["metadata", "--format-version", "1", "--no-deps"])).unwrap()
}

/// Package names reachable from `krate` via normal + build edges, all targets, per-package features.
fn closure(krate: &str) -> BTreeSet<String> {
    closure_in(&root(), krate)
}

fn closure_in(dir: &Path, krate: &str) -> BTreeSet<String> {
    cargo_in(
        dir,
        &[
            "tree",
            "-p",
            krate,
            "--target",
            "all",
            "-e",
            "normal,build",
            "--prefix",
            "none",
            "-f",
            "{p}",
        ],
    )
    .lines()
    .skip(1)
    .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
    .collect()
}

fn is_os_crate(n: &str) -> bool {
    OS_CRATE_PREFIXES
        .iter()
        .any(|p| n == *p || n.starts_with(&format!("{p}-")))
}

fn rust_sources(krate: &str) -> Vec<(PathBuf, String)> {
    rust_sources_in(&root(), krate)
}

fn rust_sources_in(dir: &Path, krate: &str) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("crates").join(krate).join("src")];
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
        let bad: Vec<_> = closure(c)
            .into_iter()
            .filter(|n| n.starts_with(ADAPTER_PREFIX) || OUTER.contains(&n.as_str()))
            .collect();
        assert!(bad.is_empty(), "{c} depends on outer crates {bad:?}");
    }
}

#[test]
fn r2_test_support_only_as_dev_dependency() {
    for p in metadata()["packages"].as_array().unwrap() {
        for d in p["dependencies"].as_array().unwrap() {
            if TEST_SUPPORT.contains(&d["name"].as_str().unwrap()) {
                assert_eq!(
                    d["kind"].as_str(),
                    Some("dev"),
                    "{} has {} as a non-dev dependency",
                    p["name"],
                    d["name"]
                );
            }
        }
    }
    // And nothing reaches them transitively through normal edges.
    for c in [
        "fogar-agentd",
        "fogar-host",
        "fogar-agent-linux",
        "fogar-agent-macos",
    ] {
        let bad: Vec<_> = closure(c)
            .into_iter()
            .filter(|n| TEST_SUPPORT.contains(&n.as_str()))
            .collect();
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
            let loom_dev =
                target.as_str() == Some("cfg(fogar_loom)") && d["kind"].as_str() == Some("dev");
            assert!(
                loom_dev,
                "{name}: target-specific dep {} ({target})",
                d["name"]
            );
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

/// Every `cfg(...)`, `cfg!(...)` and `cfg_attr(...)` predicate in `text` (balanced parentheses).
fn cfg_predicates(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find("cfg") {
        let start = i + off;
        i = start + 3;
        // Identifier boundary on the left (`xcfg(` is not a cfg).
        if start > 0 && (b[start - 1].is_ascii_alphanumeric() || b[start - 1] == b'_') {
            continue;
        }
        let mut j = i;
        for kw in ["_attr", "!"] {
            if text[j..].starts_with(kw) {
                j += kw.len();
            }
        }
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != b'(' {
            continue;
        }
        let mut depth = 0usize;
        let mut k = j;
        while k < b.len() {
            match b[k] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            k += 1;
        }
        out.push(&text[j..k.min(b.len())]);
        i = k.min(b.len());
    }
    out
}

/// Identifiers in a cfg predicate, string literals removed (`feature = "unix"` is not platform).
fn cfg_idents(pred: &str) -> Vec<String> {
    let mut idents = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for ch in pred.chars() {
        if ch == '"' {
            in_str = !in_str;
            continue;
        }
        if !in_str && (ch.is_ascii_alphanumeric() || ch == '_') {
            cur.push(ch);
        } else if !cur.is_empty() {
            idents.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        idents.push(cur);
    }
    idents
}

/// R5 checker: platform predicates anywhere inside a cfg (`unix`, `windows`, any `target_*`,
/// including under `not(..)`, `any(..)`, `all(..)`, `cfg_attr`, `cfg!`), and `cfg_select!`.
fn platform_cfg_violations(sources: &[(PathBuf, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (p, text) in sources {
        if text.contains("cfg_select!") {
            bad.push(format!("{}: cfg_select!", p.display()));
        }
        for pred in cfg_predicates(text) {
            for id in cfg_idents(pred) {
                if id == "unix" || id == "windows" || id.starts_with("target_") {
                    bad.push(format!("{}: `{id}` in cfg{pred}", p.display()));
                }
            }
        }
    }
    bad
}

#[test]
fn r5_no_platform_cfg_in_inner_sources() {
    for c in INNER {
        let bad = platform_cfg_violations(&rust_sources(c));
        assert!(bad.is_empty(), "platform cfg belongs in adapters: {bad:#?}");
    }
}

/// R9 checker: adapter crates (`fogar-host`, `fogar-agent-*`) implement ports; they never reach
/// the engine (`fogar-core`), not even transitively.
fn adapters_reaching_core(dir: &Path) -> Vec<String> {
    let md: Value = serde_json::from_str(&cargo_in(
        dir,
        &["metadata", "--format-version", "1", "--no-deps"],
    ))
    .unwrap();
    let mut bad = Vec::new();
    for p in md["packages"].as_array().unwrap() {
        let name = p["name"].as_str().unwrap();
        if (name == "fogar-host" || name.starts_with(ADAPTER_PREFIX))
            && closure_in(dir, name).contains("fogar-core")
        {
            bad.push(name.to_owned());
        }
    }
    bad
}

#[test]
fn r9_adapters_never_depend_on_core() {
    let bad = adapters_reaching_core(&root());
    assert!(bad.is_empty(), "adapters depend on fogar-core: {bad:?}");
}

/// R10 checker: the whole `tokio::time` module is banned in core sources (clippy.toml bans the
/// individual items; this catches anything it does not list, e.g. `use tokio::time;`).
fn tokio_time_uses(sources: &[(PathBuf, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (p, text) in sources {
        for (i, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            let squashed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
            let in_group = squashed.match_indices("tokio::{").any(|(at, m)| {
                squashed[at + m.len()..]
                    .split(['{', '}', ','])
                    .take_while(|seg| !seg.ends_with(';'))
                    .any(|seg| seg == "time" || seg.starts_with("time::"))
            });
            if squashed.contains("tokio::time") || in_group {
                bad.push(format!("{}:{}", p.display(), i + 1));
            }
        }
    }
    bad
}

#[test]
fn r10_core_never_uses_tokio_time() {
    let bad = tokio_time_uses(&rust_sources("fogar-core"));
    assert!(
        bad.is_empty(),
        "tokio::time in fogar-core: {bad:?} (use the Clock port)"
    );
}

#[test]
fn r6_proto_stays_lean_and_no_std_capable() {
    let md = metadata();
    let proto = md["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "fogar-proto")
        .unwrap();
    for d in proto["dependencies"].as_array().unwrap() {
        if d["kind"].as_str() == Some("dev") {
            continue;
        }
        assert_eq!(
            d["uses_default_features"],
            Value::Bool(false),
            "fogar-proto dep {} must disable default features",
            d["name"]
        );
    }
    let extra: Vec<_> = closure("fogar-proto")
        .into_iter()
        .filter(|n| !PROTO_ALLOWED.contains(&n.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "fogar-proto pulls non-allowlisted crates {extra:?}"
    );
    let lib = std::fs::read_to_string(root().join("crates/fogar-proto/src/lib.rs")).unwrap();
    assert!(
        lib.contains("#![no_std]"),
        "fogar-proto must stay #![no_std]"
    );
}

#[test]
fn r7_inner_crates_have_clippy_bans() {
    for c in INNER {
        let p = root().join("crates").join(c).join("clippy.toml");
        let text =
            std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("{} missing", p.display()));
        for must in [
            "std::time::Instant::now",
            "std::thread::sleep",
            "std::env::var",
            "std::fs::File",
        ] {
            assert!(text.contains(must), "{}: must ban {must}", p.display());
        }
    }
    let core = std::fs::read_to_string(root().join("crates/fogar-core/clippy.toml")).unwrap();
    for must in CORE_CLIPPY_BANS {
        assert!(
            core.contains(must),
            "fogar-core/clippy.toml must ban {must}"
        );
    }
    let ws = std::fs::read_to_string(root().join("Cargo.toml")).unwrap();
    for lint in [
        "disallowed_methods = \"deny\"",
        "disallowed_types = \"deny\"",
        "unwrap_used = \"deny\"",
    ] {
        assert!(ws.contains(lint), "workspace lints must keep {lint}");
    }
}

#[test]
fn r8_no_unsafe_in_inner_crates() {
    for c in INNER {
        let lib =
            std::fs::read_to_string(root().join("crates").join(c).join("src/lib.rs")).unwrap();
        assert!(
            lib.contains("#![forbid(unsafe_code)]"),
            "{c}: missing #![forbid(unsafe_code)]"
        );
        for (p, text) in rust_sources(c) {
            for (i, line) in text.lines().enumerate() {
                let code = line.split("//").next().unwrap_or_default();
                assert!(
                    !code.contains("unsafe "),
                    "{}:{}: `unsafe` in an inner crate",
                    p.display(),
                    i + 1
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Self-tests: plant each violation in a temporary fixture and assert the checker fires.

fn fixture_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fogar-archtest-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn selftest_r5_fires_on_every_platform_predicate_shape() {
    let planted = [
        "#[cfg(unix)] fn a() {}",
        "#[cfg(windows)] fn a() {}",
        "#[cfg(not(windows))] fn a() {}",
        "#[cfg(any(unix, windows))] fn a() {}",
        "#[cfg(all(test, target_os = \"macos\"))] fn a() {}",
        "#[cfg( target_family = \"unix\" )] fn a() {}",
        "#[cfg(target_arch = \"aarch64\")] fn a() {}",
        "#[cfg(target_pointer_width = \"64\")] fn a() {}",
        "#[cfg_attr(unix, allow(dead_code))] fn a() {}",
        "fn a() -> bool { cfg!(not(target_vendor = \"apple\")) }",
        "#[cfg(all(feature = \"std\",\n    any(unix)))] fn a() {}",
        "std::cfg_select! { _ => {} }",
    ];
    let dir = fixture_dir("r5");
    for (i, src) in planted.iter().enumerate() {
        let lib = dir.join("crates/fogar-core/src/lib.rs");
        write(&lib, src);
        let bad = platform_cfg_violations(&rust_sources_in(&dir, "fogar-core"));
        assert!(!bad.is_empty(), "R5 missed planted case {i}: {src}");
    }
    // ... and stays quiet on legitimate cfgs.
    for ok in [
        "#[cfg(test)] fn a() {}",
        "#[cfg(fogar_loom)] fn a() {}",
        "#[cfg(not(fogar_loom))] fn a() {}",
        "#[cfg(feature = \"unix\")] fn a() {}",
        "#[cfg_attr(test, allow(dead_code))] fn a() {}",
        "fn a() { let config = 1; let _ = config; }",
    ] {
        let lib = dir.join("crates/fogar-core/src/lib.rs");
        write(&lib, ok);
        let bad = platform_cfg_violations(&rust_sources_in(&dir, "fogar-core"));
        assert!(bad.is_empty(), "R5 false positive on {ok}: {bad:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selftest_r9_fires_when_an_adapter_reaches_core() {
    let dir = fixture_dir("r9");
    let krate = |name: &str, deps: &str| {
        write(
            &dir.join("crates").join(name).join("Cargo.toml"),
            &format!(
                "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}"
            ),
        );
        write(&dir.join("crates").join(name).join("src/lib.rs"), "");
    };
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\"]\n",
    );
    krate("fogar-core", "");
    krate("fogar-ports", "");
    krate(
        "fogar-host",
        "fogar-ports = { path = \"../fogar-ports\" }\n",
    );
    // Transitive: fogar-agent-x -> helper -> fogar-core.
    krate("helper", "fogar-core = { path = \"../fogar-core\" }\n");
    krate("fogar-agent-x", "helper = { path = \"../helper\" }\n");
    krate(
        "fogar-agentd",
        "fogar-core = { path = \"../fogar-core\" }\n",
    );
    let bad = adapters_reaching_core(&dir);
    assert_eq!(
        bad,
        ["fogar-agent-x"],
        "composition root is allowed, adapters are not"
    );
    krate("fogar-host", "fogar-core = { path = \"../fogar-core\" }\n");
    let mut bad = adapters_reaching_core(&dir);
    bad.sort();
    assert_eq!(bad, ["fogar-agent-x", "fogar-host"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn selftest_r10_fires_on_any_tokio_time_use() {
    let dir = fixture_dir("r10");
    for src in [
        "use tokio::time;",
        "use tokio::{sync, time::Instant};",
        "fn f() { let _ = tokio::time::Instant::now(); }",
        "async fn f() { tokio :: time :: sleep_until(x).await }",
    ] {
        write(&dir.join("crates/fogar-core/src/lib.rs"), src);
        assert!(
            !tokio_time_uses(&rust_sources_in(&dir, "fogar-core")).is_empty(),
            "R10 missed {src}"
        );
    }
    write(
        &dir.join("crates/fogar-core/src/lib.rs"),
        "// tokio::time is banned\nfn f() {}",
    );
    assert!(tokio_time_uses(&rust_sources_in(&dir, "fogar-core")).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// R7 self-test with the real tool: a fixture crate using fogar-core's clippy.toml must fail
/// clippy for each planted ambient-time use (the module-wide ban is R10).
#[test]
fn selftest_r7_core_clippy_toml_rejects_tokio_time() {
    let dir = fixture_dir("r7");
    std::fs::copy(
        root().join("crates/fogar-core/clippy.toml"),
        dir.join("clippy.toml"),
    )
    .unwrap();
    std::fs::copy(root().join("Cargo.lock"), dir.join("Cargo.lock")).unwrap();
    write(
        &dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [dependencies]\ntokio = { version = \"=1.54.1\", default-features = false, features = [\"time\", \"sync\", \"test-util\"] }\n",
    );
    let target = root().join("target/archtest-fixture");
    for (i, body) in [
        "pub async fn f() { tokio::time::sleep_until(tokio::time::Instant::now()).await }",
        "pub fn f() -> Option<tokio::time::Interval> { None }",
        "pub async fn f() { let _ = tokio::time::timeout_at(todo_at(), async {}).await; }\n\
         fn todo_at() -> tokio::time::Instant { unreachable!() }",
        "pub fn f() { tokio::time::pause(); }",
    ]
    .iter()
    .enumerate()
    {
        write(&dir.join("src/lib.rs"), body);
        let out = Command::new(env!("CARGO"))
            .args([
                "clippy",
                "--offline",
                "--quiet",
                "--",
                "-D",
                "clippy::disallowed_methods",
                "-D",
                "clippy::disallowed_types",
            ])
            .env("CARGO_TARGET_DIR", &target)
            .current_dir(&dir)
            .output()
            .expect("run cargo clippy");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && err.contains("disallowed"),
            "planted case {i} passed clippy: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
