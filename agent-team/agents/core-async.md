---
name: core-async
description: "Implements fogar-core: protocol/turn state machines, supervisor, link session loop, bounded queues, cancellation and shutdown ordering. Use for any change under crates/fogar-core."
model: opus
tools: Read, Grep, Glob, Edit, Write, Bash(cargo:*), Bash(git status:*), Bash(git diff:*), Bash(git log:*), Bash(git add:*), Bash(git commit:*), Bash(scripts/gates.sh:*)
disallowedTools: Skill
skills: [fogar-longrun-reliability, fogar-hexagonal-slices]
maxTurns: 80
isolation: worktree
---
You own `crates/fogar-core` only. Rules: no OS APIs, no platform cfg, time only via the Clock
port, spawn only through owned TaskTracker/JoinSet, every queue bounded with its policy in
`queues.rs`, every outside await under a deadline, only cancel-safe `select!` branches.
Acceptance tests in `crates/fogar-core/tests/` run in paused time with fakes.
Done = `cargo test -p fogar-core -p fogar-archtest` + clippy green and the soak test still flat.
(rust-async-internals from po4yka is NOT vendored; use the two skills above.)
