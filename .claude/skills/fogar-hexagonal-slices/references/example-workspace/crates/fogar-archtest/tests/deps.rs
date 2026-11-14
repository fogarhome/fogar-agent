//! Hexagonal dependency rules as a plain `cargo test`. Runs on any host because
//! `cargo metadata` (without --filter-platform) resolves deps for ALL targets.
//!
//! Rules:
//!  R1 inner crates never depend (normal/build, transitively) on adapter/host crates
//!  R2 fogar-testkit is only ever a dev-dependency
//!  R3 only adapter crates declare target-specific (`[target.'cfg(..)'.dependencies]`) deps
//!  R4 inner crates never reach OS-binding crates, even transitively
//!  R5 no `cfg(target_os|windows|unix|target_family|target_vendor)` in inner crates' sources
//!  R6 fogar-proto: dependency allowlist, all with default-features = false (no_std)

use serde_json::Value;
use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Command,
};

const INNER: &[&str] = &["fogar-proto", "fogar-ports", "fogar-core"];
const ADAPTER_PREFIX: &str = "fogar-agent-";
const HOST: &str = "fogar-host";
const OS_CRATE_PREFIXES: &[&str] = &[
    "libc",
    "nix",
    "rustix",
    "objc2",
    "block2",
    "dispatch2",
    "core-foundation",
    "core-graphics",
    "io-kit",
    "windows",
    "windows-sys",
    "winapi",
    "security-framework",
    "zbus",
];
const PROTO_ALLOWED: &[&str] = &[
    "thiserror",
    "thiserror-impl",
    "proc-macro2",
    "quote",
    "syn",
    "unicode-ident",
];

struct Meta {
    v: Value,
    workspace_root: PathBuf,
}

fn metadata() -> Meta {
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let out = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&ws)
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let workspace_root = PathBuf::from(v["workspace_root"].as_str().unwrap());
    Meta { v, workspace_root }
}

impl Meta {
    fn packages(&self) -> &Vec<Value> {
        self.v["packages"].as_array().unwrap()
    }
    fn name_of(&self) -> HashMap<String, String> {
        self.packages()
            .iter()
            .map(|p| {
                (
                    p["id"].as_str().unwrap().to_owned(),
                    p["name"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
    fn workspace_names(&self) -> BTreeSet<String> {
        let ids: BTreeSet<_> = self.v["workspace_members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.as_str().unwrap().to_owned())
            .collect();
        let names = self.name_of();
        ids.iter().map(|i| names[i].clone()).collect()
    }
    /// Names reachable from `root` via normal + build edges (dev excluded), ALL targets,
    /// with features resolved for `root` alone.
    ///
    /// PITFALL (found while building this example): `cargo metadata`'s `resolve` graph
    /// unifies features across the whole workspace, so walking it reports e.g. tokio->libc
    /// under fogar-ports just because the agent crate enables tokio/"signal". `cargo tree -p`
    /// resolves features per package (resolver 2/3), which is what the rule is about.
    fn closure(&self, root: &str) -> BTreeSet<String> {
        let out = Command::new(env!("CARGO"))
            .args([
                "tree",
                "-p",
                root,
                "--target",
                "all",
                "-e",
                "normal,build",
                "--prefix",
                "none",
                "-f",
                "{p}",
            ])
            .arg("--manifest-path")
            .arg(self.workspace_root.join("Cargo.toml"))
            .output()
            .expect("run cargo tree");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .skip(1) // the root itself
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect()
    }
}

#[test]
fn r1_inner_never_depends_on_adapters() {
    let m = metadata();
    for c in INNER {
        let bad: Vec<_> = m
            .closure(c)
            .into_iter()
            .filter(|n| n.starts_with(ADAPTER_PREFIX) || n == HOST)
            .collect();
        assert!(bad.is_empty(), "{c} depends on adapter crates {bad:?}");
    }
}

#[test]
fn r2_testkit_only_as_dev_dependency() {
    let m = metadata();
    for p in m.packages() {
        for d in p["dependencies"].as_array().unwrap() {
            if d["name"] == "fogar-testkit" {
                assert_eq!(
                    d["kind"].as_str(),
                    Some("dev"),
                    "{} has fogar-testkit as non-dev dep",
                    p["name"]
                );
            }
        }
    }
}

#[test]
fn r3_only_adapters_have_target_specific_deps() {
    let m = metadata();
    let ws = m.workspace_names();
    for p in m
        .packages()
        .iter()
        .filter(|p| ws.contains(p["name"].as_str().unwrap()))
    {
        let name = p["name"].as_str().unwrap();
        if name.starts_with(ADAPTER_PREFIX) {
            continue;
        }
        for d in p["dependencies"].as_array().unwrap() {
            assert!(
                d["target"].is_null(),
                "{name}: target-specific dep {} ({})",
                d["name"],
                d["target"]
            );
        }
    }
}

#[test]
fn r4_inner_never_reaches_os_crates() {
    let m = metadata();
    for c in INNER {
        let bad: Vec<_> = m
            .closure(c)
            .into_iter()
            .filter(|n| {
                OS_CRATE_PREFIXES
                    .iter()
                    .any(|p| n == p || n.starts_with(&format!("{p}-")))
            })
            .collect();
        assert!(bad.is_empty(), "{c} reaches OS crates {bad:?}");
    }
}

#[test]
fn r5_no_platform_cfg_in_inner_sources() {
    let m = metadata();
    let needles = [
        "target_os",
        "target_family",
        "target_vendor",
        "cfg(windows",
        "cfg(unix",
        "cfg_select!",
    ];
    for c in INNER {
        let dir = m.workspace_root.join("crates").join(c).join("src");
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    for n in needles {
                        assert!(
                            !text.contains(n),
                            "{}: contains `{n}` (platform cfg belongs in adapter crates)",
                            p.display()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn r6_proto_stays_no_std_and_lean() {
    let m = metadata();
    let proto = m
        .packages()
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
    let extra: Vec<_> = m
        .closure("fogar-proto")
        .into_iter()
        .filter(|n| !PROTO_ALLOWED.contains(&n.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "fogar-proto pulls non-allowlisted crates {extra:?}"
    );
}
