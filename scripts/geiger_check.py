#!/usr/bin/env python3
"""Compare a `cargo geiger --output-format Json` report against a committed baseline.

Usage: geiger_check.py REPORT.json BASELINE.txt [--write]
       geiger_check.py --self-test

Baseline lines (sorted): `unsafe <crate> <count>` = `unsafe` items geiger counts as *used* by the
build, summed over every version of that crate (a version bump alone does not trip the gate), and
`unscanned <file>` = a used .rs file geiger could not parse (its unsafe is invisible, so a new one
fails like a count increase). Exit 1 on growth or a new unscanned file; decreases only print a note.
"""
import json
import os
import re
import sys

REGISTRY = re.compile(r"/registry/src/[^/]+/([^/]+?)-\d+\.\d+\.\d+[^/]*/(.*)$")
BUILD_OUT = re.compile(r"/build/([^/]+?)-[0-9a-f]{16}/(out/.*)$")


def normalize(path, root):
    m = REGISTRY.search(path)
    if m:
        return f"{m.group(1)}/{m.group(2)}"
    m = BUILD_OUT.search(path)
    if m:
        return f"build:{m.group(1)}/{m.group(2)}"
    return os.path.relpath(path, root)


def summarize(report, root):
    counts = {}
    for p in report["packages"]:
        name = p["package"]["id"]["name"]
        used = p["unsafety"]["used"]
        counts[name] = counts.get(name, 0) + sum(v["unsafe_"] for v in used.values())
    unscanned = {
        normalize(f, root) for f in report["used_but_not_scanned_files"] if f.endswith(".rs")
    }
    return counts, unscanned


def render(counts, unscanned):
    lines = [f"unsafe {n} {c}" for n, c in sorted(counts.items()) if c > 0]
    lines += [f"unscanned {f}" for f in sorted(unscanned)]
    return "\n".join(lines) + "\n"


def parse(text):
    counts, unscanned = {}, set()
    for line in text.splitlines():
        kind, _, rest = line.partition(" ")
        if kind == "unsafe":
            name, count = rest.split()
            counts[name] = int(count)
        elif kind == "unscanned":
            unscanned.add(rest)
        elif line.strip() and not line.startswith("#"):
            raise ValueError(f"bad baseline line: {line!r}")
    return counts, unscanned


def compare(base, now):
    """Returns (failures, notes) as lists of strings."""
    (bc, bu), (nc, nu) = base, now
    failures, notes = [], []
    for name in sorted(set(bc) | set(nc)):
        b, n = bc.get(name, 0), nc.get(name, 0)
        if n > b:
            failures.append(f"{name}: unsafe {b} -> {n}")
        elif n < b:
            notes.append(f"{name}: unsafe {b} -> {n} (baseline can be lowered)")
    failures += [f"new unscanned file (unsafe not counted): {f}" for f in sorted(nu - bu)]
    notes += [f"no longer unscanned: {f}" for f in sorted(bu - nu)]
    return failures, notes


def self_test():
    root = "/w"
    report = {
        "packages": [
            {"package": {"id": {"name": "a", "version": "1.0.0"}},
             "unsafety": {"used": {"exprs": {"unsafe_": 3}, "functions": {"unsafe_": 1}}}},
            {"package": {"id": {"name": "a", "version": "2.0.0"}},
             "unsafety": {"used": {"exprs": {"unsafe_": 2}}}},
            {"package": {"id": {"name": "b", "version": "0.1.0"}},
             "unsafety": {"used": {"exprs": {"unsafe_": 0}}}},
        ],
        "used_but_not_scanned_files": [
            "/h/.cargo/registry/src/index.crates.io-abc/sig-reg-1.4.8/src/lib.rs",
            "/t/debug/build/serde-0123456789abcdef/out/private.rs",
            "/h/.cargo/registry/src/index.crates.io-abc/x-1.0.0/README.md",
        ],
    }
    now = summarize(report, root)
    assert now == ({"a": 6, "b": 0}, {"sig-reg/src/lib.rs", "build:serde/out/private.rs"}), now
    text = render(*now)
    assert "unsafe b" not in text and parse(text) == ({"a": 6}, now[1]), text
    assert compare(parse(text), now) == ([], [])
    grown = ({"a": 7, "c": 1}, now[1] | {"new/src/lib.rs"})
    fails, _ = compare(parse(text), grown)
    assert fails == ["a: unsafe 6 -> 7", "c: unsafe 0 -> 1",
                     "new unscanned file (unsafe not counted): new/src/lib.rs"], fails
    fails, notes = compare(parse(text), ({"a": 5}, set()))
    assert fails == [] and len(notes) == 3, notes
    print("geiger_check self-test: ok")


def main(argv):
    if argv == ["--self-test"]:
        self_test()
        return 0
    if len(argv) not in (2, 3) or (len(argv) == 3 and argv[2] != "--write"):
        print(__doc__, file=sys.stderr)
        return 2
    report_path, baseline_path = argv[0], argv[1]
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    with open(report_path) as f:
        now = summarize(json.load(f), root)
    if len(argv) == 3:
        with open(baseline_path, "w") as f:
            f.write(render(*now))
        print(f"wrote {baseline_path}")
        return 0
    if not os.path.exists(baseline_path):
        print(f"geiger: no baseline {baseline_path}; create it with --write and commit it")
        return 1
    with open(baseline_path) as f:
        base = parse(f.read())
    failures, notes = compare(base, now)
    total = sum(now[0].values())
    summary = [f"geiger: {total} used unsafe items in {sum(1 for c in now[0].values() if c)} "
               f"crates, {len(now[1])} unscanned files (baseline {baseline_path})"]
    summary += [f"FAIL {x}" for x in failures] + [f"note {x}" for x in notes]
    print("\n".join(summary))
    step = os.environ.get("GITHUB_STEP_SUMMARY")
    if step:
        with open(step, "a") as f:
            f.write("### cargo geiger\n\n```\n" + "\n".join(summary) + "\n```\n")
    if failures:
        print(f"unsafe grew: if intended, regenerate {baseline_path} with --write in this PR")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
