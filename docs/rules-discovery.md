# Native rule discovery

## Goal

`tcode` provides native, path-scoped instruction discovery without making
Claude Code's configuration tree an implicit input. It accepts the Markdown
rule-file format Claude Code uses, so rule content can be copied into tcode,
but discovers only tcode-owned locations by default.

## Rule locations

The built-in rule root is:

```text
~/.tcode/rules/**/*.md
<project>/.tcode/rules/**/*.md
```

User rules load before project rules, so project rules have higher instruction
priority. `tcode` does **not** scan `.claude/rules/` by default. A user who
wants to reuse Claude Code rules copies the desired files into `.tcode/rules`.

A rule file uses YAML frontmatter compatible with Claude Code's `paths`
format:

```md
---
paths:
  - "src/**/*.py"
  - "pyproject.toml"
---

# Python conventions

- Use the repository's established Python toolchain.
- Update tests when behavior changes.
```

Patterns are matched against slash-normalized paths relative to the project
root. A rule with `paths` is discovered when a tool targets a matching path. A
rule without `paths` is unconditional and loads at startup. Frontmatter is
metadata and is not injected into the model context; only the Markdown body is
an instruction. Rules with invalid frontmatter or an invalid `paths` value are
ignored.

Newly discovered rules use the existing append-only ledger path. They remain
active for the session and are restored after resume or compaction. A matching
rule discovered before a mutation blocks that mutation batch so the model can
apply the new instruction on a retry.

## Built-in ancestor instructions

The existing directory-scoped instruction behavior is a second discoverer in
the same framework, rather than a special loop hard-coded in `memory.rs`. For
each directory from project root to the target directory, it chooses the first
existing candidate:

```text
.tcode/AGENTS.md
AGENTS.md
CLAUDE.md
```

These are the built-in defaults. They retain their existing lazy discovery,
per-directory precedence, scope, budget, and ledger-marker behavior.

## Configuration

Instruction discovery configuration is loaded using tcode's normal precedence:
embedded defaults, selected user configuration, then project
`.tcode/config.toml`. The project layer may replace the candidate list because
it is defining that repository's instruction convention.

```toml
[instructions]
# Omit this field to use the built-in order above. An explicit empty list turns
# off ancestor-candidate discovery while path-scoped .tcode/rules still works.
directory_candidates = [
  ".tcode/AGENTS.md",
  "AGENTS.md",
  "CLAUDE.md",
]
```

Each candidate must be a project-relative file path. Absolute paths and paths
that escape a directory through `..` are rejected. The configured list replaces
rather than concatenates with a lower-precedence layer, preserving a single,
predictable order per directory.

The rule root and the rule-file format are intentionally not configurable in
this first version. Keeping the discovery root under `.tcode/` prevents a
checked-out project from silently importing another tool's instruction tree.

## Internal shape

Both mechanisms share discovery, de-duplication, source markers, instruction
budgeting, append-only injection, compaction reinjection, and session restore.
They differ only in how a source is selected for a target path:

| Discoverer | Selection |
| --- | --- |
| Ancestor candidates | Walk from project root to target directory and choose the first configured candidate per directory. |
| Native path rules | Recursively enumerate `.tcode/rules/**/*.md`, parse `paths`, and select rules whose glob matches the target's project-relative path. |

A discovered source carries its logical affected targets. For an ancestor
instruction this is its governing directory. For a path rule it is the matching
target path, not the physical `.tcode/rules/` directory. This preserves the
existing pre-mutation rule gate without overblocking unrelated changes.
