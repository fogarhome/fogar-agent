//! Hexagonal dependency rules (fogar-hexagonal-slices §2) as plain tests. Transitive rules use
//! `cargo tree -p <crate>` (features resolved per package), NOT `cargo metadata`'s resolve graph,
//! which unifies features across the whole workspace and reports false edges.
//!
//!  R1 inner crates never reach adapter / host / composition-root crates (normal+build, all targets)
//!  R2 test-support crates (testkit, testalloc) are only ever dev-dependencies
//!  R3 only adapter crates and the composition root declare target-specific deps
//!     (exception: `cfg(fogar_loom)` dev-dependencies, used by the loom gate)
//!  R4 inner crates never reach OS-binding or I/O-driver crates, even transitively
//!  R5 no platform cfg (`target_os`, `cfg(unix)`, `cfg_select`, macro-generated `cfg($p)`,
//!     `std::env::consts`) in inner crates' sources (token-based)
//!  R6 fogar-proto: dependency allowlist, every dep with default-features = false
//!  R7 every inner crate has a clippy.toml banning ambient time/threads/env/fs
//!  R8 no `unsafe` in inner crates, and each declares `#![forbid(unsafe_code)]`
//!  R9 adapter crates (fogar-host, fogar-agent-*) never reach fogar-core
//!  R10 fogar-core sources never name the `tokio::time` module at all, nor rename `tokio`
//!     (token-based)
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

/// One Rust token, as far as the source rules need: comments and literal contents are gone
/// (round-2 review F7: the line/text scans saw `cfg(unix)` inside strings, missed `cfg ! (`,
/// `cfg/* */(`, multi-line `use` groups and comments inside paths).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Identifier or keyword (`r#ident` is reported without its `r#`).
    Ident(String),
    Punct(char),
    /// Any literal (string, raw string, byte string, char, number): contents dropped.
    Lit,
}

/// Tokenizes Rust source into (token, 1-based line). Good enough for lint-style rules: nested
/// block comments, raw strings with any number of `#`, byte/C strings, char literals vs
/// lifetimes.
fn tokens(text: &str) -> Vec<(Tok, usize)> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0usize, 1usize);
    let ident_start = |ch: char| ch.is_alphabetic() || ch == '_';
    let ident_cont = |ch: char| ch.is_alphanumeric() || ch == '_';
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        if ch == '\n' {
            line += 1;
            i += 1;
        } else if ch.is_whitespace() {
            i += 1;
        } else if ch == '/' && next == Some('/') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    if c[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if ch == '"'
            || ((ch == 'b' || ch == 'c') && next == Some('"'))
            || ((ch == 'r' || ch == 'b' || ch == 'c')
                && matches!(next, Some('"' | '#' | 'r'))
                && raw_string_at(&c, i).is_some())
        {
            let start_line = line;
            if let Some(end) = raw_string_at(&c, i) {
                line += c[i..end].iter().filter(|&&x| x == '\n').count();
                i = end;
            } else {
                i += if ch == '"' { 1 } else { 2 };
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    if c.get(i) == Some(&'\n') {
                        line += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            out.push((Tok::Lit, start_line));
        } else if ch == '\'' || (ch == 'b' && next == Some('\'')) {
            let q = if ch == 'b' { i + 1 } else { i };
            // Char literal: 'x' or '\..'; otherwise a lifetime / label.
            if c.get(q + 1) == Some(&'\\') {
                let mut j = q + 2;
                while j < c.len() && c[j] != '\'' {
                    j += 1;
                }
                out.push((Tok::Lit, line));
                i = j + 1;
            } else if c.get(q + 2) == Some(&'\'') {
                out.push((Tok::Lit, line));
                i = q + 3;
            } else {
                out.push((Tok::Punct('\''), line));
                i = q + 1;
            }
        } else if ch == 'r' && next == Some('#') && c.get(i + 2).is_some_and(|&x| ident_start(x)) {
            let mut j = i + 2;
            while j < c.len() && ident_cont(c[j]) {
                j += 1;
            }
            out.push((Tok::Ident(c[i + 2..j].iter().collect()), line));
            i = j;
        } else if ident_start(ch) {
            let mut j = i;
            while j < c.len() && ident_cont(c[j]) {
                j += 1;
            }
            out.push((Tok::Ident(c[i..j].iter().collect()), line));
            i = j;
        } else if ch.is_ascii_digit() {
            while i < c.len() && ident_cont(c[i]) {
                i += 1;
            }
            out.push((Tok::Lit, line));
        } else {
            out.push((Tok::Punct(ch), line));
            i += 1;
        }
    }
    out
}

/// If a raw string literal (`r"…"`, `r#"…"#`, `br##"…"##`, `cr"…"`) starts at `i`, the index
/// just past it.
fn raw_string_at(c: &[char], i: usize) -> Option<usize> {
    let mut j = i;
    if matches!(c.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if c.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while c.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if c.get(j) != Some(&'"') {
        return None;
    }
    j += 1;
    while j < c.len() {
        if c[j] == '"' && (1..=hashes).all(|k| c.get(j + k) == Some(&'#')) {
            return Some(j + 1 + hashes);
        }
        j += 1;
    }
    Some(c.len())
}

fn is_ident(t: Option<&(Tok, usize)>, name: &str) -> bool {
    matches!(t, Some((Tok::Ident(s), _)) if s == name)
}

fn is_punct(t: Option<&(Tok, usize)>, ch: char) -> bool {
    matches!(t, Some((Tok::Punct(p), _)) if *p == ch)
}

/// Index just past `::` at `i`, if there is one.
fn path_sep(t: &[(Tok, usize)], i: usize) -> Option<usize> {
    (is_punct(t.get(i), ':') && is_punct(t.get(i + 1), ':')).then_some(i + 2)
}

/// The tokens of the bracketed group opening at `i` (`(`, `[` or `{`), without the brackets.
fn group(t: &[(Tok, usize)], i: usize) -> &[(Tok, usize)] {
    let mut depth = 0usize;
    for (k, (tok, _)) in t.iter().enumerate().skip(i) {
        match tok {
            Tok::Punct('(' | '[' | '{') => depth += 1,
            Tok::Punct(')' | ']' | '}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &t[i + 1..k];
                }
            }
            _ => {}
        }
    }
    &t[(i + 1).min(t.len())..]
}

/// `seg` is named right after `::` at `i`, either directly (`::seg`) or as an entry of a use
/// group (`::{a, seg::x}`, any nesting).
fn names_segment(t: &[(Tok, usize)], i: usize, seg: &str) -> bool {
    let Some(j) = path_sep(t, i) else {
        return false;
    };
    if is_ident(t.get(j), seg) {
        return true;
    }
    if is_punct(t.get(j), '{') {
        let g = group(t, j);
        return g.iter().enumerate().any(|(k, (tok, _))| {
            matches!(tok, Tok::Ident(s) if s == seg)
                && (k == 0 || matches!(g[k - 1].0, Tok::Punct('{' | ',')))
        });
    }
    false
}

/// R5 checker: platform predicates anywhere inside a cfg (`unix`, `windows`, any `target_*`,
/// including under `not(..)`, `any(..)`, `all(..)`, `cfg_attr`, `cfg!`), any mention of
/// `cfg_select`, a cfg whose predicate is a macro metavariable (`#[cfg($p)]`: macro-generated
/// cfgs), and `std::env::consts` (the runtime spelling of `target_os`). Token-based.
fn platform_cfg_violations(sources: &[(PathBuf, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (p, text) in sources {
        let t = tokens(text);
        for (i, (tok, line)) in t.iter().enumerate() {
            let Tok::Ident(id) = tok else { continue };
            let at = format!("{}:{line}", p.display());
            match id.as_str() {
                "cfg_select" => bad.push(format!("{at}: cfg_select")),
                "cfg" | "cfg_attr" => {
                    let mut j = i + 1;
                    if is_punct(t.get(j), '!') {
                        j += 1;
                    }
                    if !is_punct(t.get(j), '(') {
                        continue;
                    }
                    for (g, _) in group(&t, j) {
                        match g {
                            Tok::Ident(x)
                                if x == "unix" || x == "windows" || x.starts_with("target_") =>
                            {
                                bad.push(format!("{at}: `{x}` in {id}(..)"));
                            }
                            Tok::Punct('$') => {
                                bad.push(format!("{at}: macro-generated {id}($..)"));
                            }
                            _ => {}
                        }
                    }
                }
                "env" if names_segment(&t, i + 1, "consts") => {
                    bad.push(format!("{at}: std::env::consts"));
                }
                "consts"
                    if path_sep(&t, i + 1).is_some()
                        && ["OS", "FAMILY", "ARCH"]
                            .iter()
                            .any(|c| names_segment(&t, i + 1, c)) =>
                {
                    bad.push(format!("{at}: consts::.."));
                }
                _ => {}
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
/// individual items; this catches anything it does not list, e.g. `use tokio::time;`). Token-based
/// (round-2 review F7): multi-line `use tokio::{ .., time, }` groups and comments inside paths are
/// seen; renaming `tokio` (`use tokio as t`, `extern crate tokio as t`, `tokio::{self as t}`) is
/// rejected outright, since an alias would hide every later path.
fn tokio_time_uses(sources: &[(PathBuf, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (p, text) in sources {
        let t = tokens(text);
        for (i, (tok, line)) in t.iter().enumerate() {
            if !matches!(tok, Tok::Ident(s) if s == "tokio") {
                continue;
            }
            let at = format!("{}:{line}", p.display());
            if names_segment(&t, i + 1, "time") {
                bad.push(format!("{at}: tokio::time"));
            }
            if is_ident(t.get(i + 1), "as") {
                bad.push(format!("{at}: tokio renamed"));
            }
            if path_sep(&t, i + 1).is_some_and(|j| is_punct(t.get(j), '{')) {
                let g = group(&t, i + 3);
                if g.windows(2).any(|w| {
                    matches!(&w[0].0, Tok::Ident(s) if s == "self")
                        && matches!(&w[1].0, Tok::Ident(s) if s == "as")
                }) {
                    bad.push(format!("{at}: tokio renamed"));
                }
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
        // Round-2 review F7: shapes the text scan missed.
        "std::cfg_select ! { _ => {} }",
        "use std::cfg_select; cfg_select! { _ => {} }",
        "fn a() -> bool { cfg ! (unix) }",
        "#[cfg/* hidden */(unix)] fn a() {}",
        "macro_rules! plat { ($p:meta) => { #[cfg($p)] fn a() {} } } plat!(any());",
        "macro_rules! plat { ($p:ident) => { #[cfg_attr($p, allow(dead_code))] fn a() {} } }",
        "fn a() -> &'static str { std::env::consts::OS }",
        "use std::env::consts; fn a() -> &'static str { consts::FAMILY }",
        "use std::env::{\n    args,\n    consts::ARCH,\n};",
        "fn a() -> bool { r#cfg!(unix) }",
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
        "fn a() -> &'static str { \"#[cfg(unix)] cfg_select! std::env::consts::OS\" }",
        "// #[cfg(unix)] cfg_select!\nfn a() {}",
        "/* #[cfg(windows)] /* nested */ std::env::consts::OS */ fn a() {}",
        "fn a<'a>(x: &'a str) -> (char, &'a str) { ('u', x) }",
        "fn a() -> &'static str { r##\"cfg(unix) \"# still raw\"## }",
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
        // Round-2 review F7: multi-line groups, comments inside paths, aliases.
        "use tokio::{\n    sync,\n    time,\n};",
        "use tokio::{\n    sync::mpsc,\n    time::{\n        Instant,\n    },\n};",
        "use ::tokio::/* x */time::Instant;",
        "use tokio as tk; fn f() { let _ = tk::time::Instant::now(); }",
        "extern crate tokio as tk;",
        "use tokio::{self as tk}; fn f() {}",
    ] {
        write(&dir.join("crates/fogar-core/src/lib.rs"), src);
        assert!(
            !tokio_time_uses(&rust_sources_in(&dir, "fogar-core")).is_empty(),
            "R10 missed {src}"
        );
    }
    for ok in [
        "// tokio::time is banned\nfn f() {}",
        "/* use tokio::time; */ fn f() {}",
        "fn f() -> &'static str { \"tokio::time\" }",
        "use tokio::{sync::mpsc, task::JoinSet}; fn time() {}",
        "use tokio_util::time::DelayQueue;",
    ] {
        write(&dir.join("crates/fogar-core/src/lib.rs"), ok);
        let bad = tokio_time_uses(&rust_sources_in(&dir, "fogar-core"));
        assert!(bad.is_empty(), "R10 false positive on {ok}: {bad:?}");
    }
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
