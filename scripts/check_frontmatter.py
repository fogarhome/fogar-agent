#!/usr/bin/env python3
"""Strict frontmatter checker for the agent-team configuration (review round 2, F2; ADR-0007).

Called by scripts/check_vendored.sh as `python3 -I scripts/check_frontmatter.py <repo-root>`.
Python 3 standard library only. Prints one `FORBIDDEN: ...` line per problem; exit 1 if any.

The frontmatter is NOT parsed as YAML. It is parsed by a deliberately tiny grammar, and anything
outside that grammar is rejected, so no YAML trick (continuation lines, indentation, duplicate
keys, quoting, CRLF, BOM, `--- ` delimiters, flow mappings, anchors, ...) can smuggle a key or a
value past the rules:

    file        := "---\\n" line* "---\\n" body          (no BOM, no CR, no TAB anywhere)
    line        := comment | pair
    comment     := "#" text                              (column 0)
    pair        := KEY ": " VALUE                         (column 0, KEY = [A-Za-z][A-Za-z0-9]*)
    VALUE       := plain text, non-empty, no leading/trailing space; for skills only,
                   `description` may instead be a block scalar indicator (">-", ">", "|", "|-")
                   followed by one or more lines indented by the same >= 1 spaces (plain text).

Keys are case-sensitive and must be unique. The agent role (reviewer or not) comes from the FILE
NAME (`*-reviewer.md`), and `name:` must equal the file stem, so renaming or quoting cannot change
the rules that apply. Bash scopes are an explicit allowlist per role.

If PyYAML happens to be importable, the frontmatter is additionally parsed with yaml.safe_load and
must give exactly the same mapping (defence in depth; not required).
"""
import os
import re
import sys

BAD = []


def bad(msg):
    BAD.append(msg)
    print("FORBIDDEN: " + msg)


KEY_RE = re.compile(r"^([A-Za-z][A-Za-z0-9]*): (.*)$")
BLOCK_INDICATORS = (">-", ">", "|", "|-")
# Characters that would make a YAML plain scalar something else (flow collection, alias, tag,
# block scalar, directive, quoted string, ...). A value may not START with any of them, except the
# explicitly handled cases (quoted description, `skills: [...]`, block-scalar description).
YAML_SPECIAL_START = set("&*!|>%@`{}[]\"',#?:-")

SKILL_KEYS = {"name", "description"}
AGENT_KEYS = {
    "name", "description", "model", "tools", "disallowedTools", "skills", "maxTurns",
    "isolation", "permissionMode",
}
AGENT_REQUIRED = {"name", "description", "tools", "disallowedTools"}
MODELS = {"opus", "sonnet", "haiku", "inherit"}
PERMISSION_MODES = {"default", "plan"}
ISOLATION = {"worktree"}
MAX_SKILLS = 2

PLAIN_TOOLS_IMPLEMENTER = {"Read", "Grep", "Glob", "Edit", "Write"}
# Explicit Bash scope allowlist (no `bash`, `sh`, `env`, `curl`, `python`, ... ever).
BASH_SCOPES_IMPLEMENTER = {
    "cargo", "cargo fmt", "cargo test", "cargo clippy", "cargo generate-lockfile", "cargo update",
    "git status", "git diff", "git log", "git show", "git add", "git commit", "scripts/gates.sh",
}
REVIEWER_TOOLS = {
    "Read", "Grep", "Glob", "Bash(cargo test:*)", "Bash(cargo clippy:*)", "Bash(git diff:*)",
    "Bash(git log:*)", "Bash(git show:*)",
}
UNRESTRICTED_BASH_RE = re.compile(r"^# unrestricted-bash: .{10,}$")


def read_frontmatter(path):
    """Returns (pairs: dict, comments: list) or None after reporting the problem."""
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError as e:
        bad(f"{path}: unreadable ({e})")
        return None
    if raw.startswith(b"\xef\xbb\xbf"):
        bad(f"{path}: byte-order mark")
        return None
    if b"\r" in raw:
        bad(f"{path}: carriage return (CR/CRLF)")
        return None
    if b"\t" in raw.split(b"\n---\n", 1)[0]:
        bad(f"{path}: TAB in frontmatter")
        return None
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        bad(f"{path}: not UTF-8")
        return None
    lines = text.split("\n")
    if not lines or lines[0] != "---":
        bad(f"{path}: first line is not exactly '---'")
        return None
    try:
        end = lines.index("---", 1)
    except ValueError:
        bad(f"{path}: no closing '---' line (exactly '---', no trailing space)")
        return None
    fm = lines[1:end]
    pairs, comments = {}, []
    i = 0
    while i < len(fm):
        line = fm[i]
        if line.startswith("#"):
            comments.append(line)
            i += 1
            continue
        m = KEY_RE.match(line)
        if not m:
            bad(f"{path}: frontmatter line {i + 2} is not 'key: value' at column 0: {line!r}")
            return None
        key, val = m.group(1), m.group(2)
        if key in pairs:
            bad(f"{path}: duplicate frontmatter key '{key}'")
            return None
        if val != val.strip() or val == "":
            bad(f"{path}: key '{key}' has an empty value or leading/trailing space")
            return None
        if val in BLOCK_INDICATORS:
            # Block scalar: only `description`; caller decides whether it is allowed here.
            j, body, indent = i + 1, [], None
            while j < len(fm) and fm[j].startswith(" "):
                cur = len(fm[j]) - len(fm[j].lstrip(" "))
                if indent is None:
                    indent = cur
                if cur < indent or fm[j].strip() == "":
                    bad(f"{path}: block scalar for '{key}' has a badly indented/blank line")
                    return None
                body.append(fm[j][indent:])
                j += 1
            if not body:
                bad(f"{path}: key '{key}' has a block indicator but no indented text")
                return None
            pairs[key] = ("block", val, body)
            i = j
            continue
        if i + 1 < len(fm) and fm[i + 1].startswith(" "):
            bad(f"{path}: key '{key}' is continued on an indented next line")
            return None
        pairs[key] = ("plain", val, None)
        i += 1
    return pairs, comments


def plain_value(path, pairs, key, allow=()):
    """The value of a single-line key; rejects YAML-special leading characters unless allowed."""
    kind, val, _ = pairs[key]
    if kind != "plain":
        bad(f"{path}: key '{key}' may not be a block scalar")
        return None
    if val[0] in YAML_SPECIAL_START and val[0] not in allow:
        bad(f"{path}: key '{key}' value starts with YAML-special character {val[0]!r}")
        return None
    if val[0] in allow and val[0] in "\"'":
        q = val[0]
        if len(val) < 2 or val[-1] != q or q in val[1:-1]:
            bad(f"{path}: key '{key}' has a malformed quoted value")
            return None
        if q == '"' and "\\" in val:
            bad(f"{path}: key '{key}' has an escape sequence in a quoted value")
            return None
        return val[1:-1]
    if " #" in val:
        bad(f"{path}: key '{key}' value contains ' #' (YAML comment)")
        return None
    if ": " in val or val.endswith(":"):
        bad(f"{path}: key '{key}' value contains ': '")
        return None
    return val


def yaml_cross_check(path, pairs):
    try:
        import yaml  # optional
    except ImportError:
        return
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    fm = text.split("\n---\n", 1)[0][4:]
    try:
        data = yaml.safe_load(fm)
    except yaml.YAMLError as e:
        bad(f"{path}: frontmatter is not valid YAML ({e.__class__.__name__})")
        return
    if not isinstance(data, dict) or set(map(str, data.keys())) != set(pairs.keys()):
        bad(f"{path}: YAML parse disagrees with the strict parse (keys)")


def check_skill(path, skills_root):
    r = read_frontmatter(path)
    if r is None:
        return
    pairs, _ = r
    for k in pairs:
        if k not in SKILL_KEYS:
            bad(f"{path}: frontmatter key '{k}' (skills may only have name/description)")
    if set(pairs) != SKILL_KEYS:
        bad(f"{path}: skills need exactly name and description")
        return
    name = plain_value(path, pairs, "name")
    if name is not None and name != os.path.basename(os.path.dirname(path)):
        bad(f"{path}: name '{name}' != directory name")
    if pairs["description"][0] == "plain":
        plain_value(path, pairs, "description", allow="\"'")
    yaml_cross_check(path, pairs)


def check_agent(path, skills_root):
    r = read_frontmatter(path)
    if r is None:
        return
    pairs, comments = r
    stem = os.path.basename(path)[:-3]
    reviewer = stem.endswith("-reviewer")
    for k in pairs:
        if k not in AGENT_KEYS:
            bad(f"{path}: frontmatter key '{k}' not allowed")
    for k in AGENT_REQUIRED - set(pairs):
        bad(f"{path}: missing required key '{k}'")
    if BAD and any(m.startswith(path) for m in BAD):
        return
    vals = {}
    for k in pairs:
        if k == "description" and pairs[k][0] == "plain":
            vals[k] = plain_value(path, pairs, k, allow="\"'")
        elif k == "description":
            bad(f"{path}: agent description must be a single line")
            vals[k] = None
        elif k == "skills":
            vals[k] = plain_value(path, pairs, k, allow="[")
        else:
            vals[k] = plain_value(path, pairs, k)
    if any(v is None for v in vals.values()):
        return
    if vals["name"] != stem:
        bad(f"{path}: name '{vals['name']}' must equal the file stem '{stem}'")
    if "model" in vals and vals["model"] not in MODELS:
        bad(f"{path}: model '{vals['model']}' not in {sorted(MODELS)}")
    if "permissionMode" in vals and vals["permissionMode"] not in PERMISSION_MODES:
        bad(f"{path}: permissionMode '{vals['permissionMode']}' (only default/plan)")
    if "isolation" in vals and vals["isolation"] not in ISOLATION:
        bad(f"{path}: isolation '{vals['isolation']}' not allowed")
    if "maxTurns" in vals and not re.fullmatch(r"[1-9][0-9]{0,2}", vals["maxTurns"]):
        bad(f"{path}: maxTurns '{vals['maxTurns']}' is not an integer 1..999")
    if vals["disallowedTools"] != "Skill":
        bad(f"{path}: disallowedTools must be exactly 'Skill'")
    if "skills" in vals:
        m = re.fullmatch(r"\[([a-z0-9, -]*)\]", vals["skills"])
        if not m:
            bad(f"{path}: skills must be a one-line [a, b] list")
        else:
            names = [s.strip() for s in m.group(1).split(",") if s.strip()]
            if len(names) > MAX_SKILLS:
                bad(f"{path} preloads {len(names)} skills (max {MAX_SKILLS})")
            for s in names:
                if not re.fullmatch(r"[a-z0-9-]+", s) or not os.path.isfile(
                    os.path.join(skills_root, s, "SKILL.md")
                ):
                    bad(f"{path}: skill '{s}' is not a vendored skill")
    toks = [t.strip() for t in vals["tools"].split(",")]
    if len(set(toks)) != len(toks):
        bad(f"{path}: duplicate tool in tools list")
    for t in toks:
        if t == "":
            bad(f"{path}: empty tool entry")
        elif reviewer:
            if t not in REVIEWER_TOOLS:
                bad(f"{path}: reviewer tool '{t}' (reviewers: read-only + cargo test/clippy + git read)")
        elif t in PLAIN_TOOLS_IMPLEMENTER:
            pass
        elif t == "Bash":
            if not any(UNRESTRICTED_BASH_RE.match(c) for c in comments):
                bad(f"{path}: unrestricted Bash without a '# unrestricted-bash: <reason>' line")
        else:
            m = re.fullmatch(r"Bash\(([^():*]+):\*\)", t)
            if not m:
                bad(f"{path}: tool '{t}' not in the allowlist")
            elif m.group(1) not in BASH_SCOPES_IMPLEMENTER:
                bad(f"{path}: Bash scope '{m.group(1)}' not in the scope allowlist")
    yaml_cross_check(path, pairs)


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else "."
    skills_root = os.path.join(root, "agent-team", "skills")
    agents_root = os.path.join(root, "agent-team", "agents")
    for d in sorted(os.listdir(skills_root)):
        p = os.path.join(skills_root, d)
        if not os.path.isdir(p) or os.path.islink(p):
            bad(f"{p}: skills/ may only contain skill directories")
            continue
        check_skill(os.path.join(p, "SKILL.md"), skills_root)
    for f in sorted(os.listdir(agents_root)):
        p = os.path.join(agents_root, f)
        if not f.endswith(".md") or not os.path.isfile(p) or os.path.islink(p):
            bad(f"{p}: agents/ may only contain regular *.md files")
            continue
        check_agent(p, skills_root)
    return 1 if BAD else 0


if __name__ == "__main__":
    sys.exit(main())
