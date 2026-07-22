---
name: tcode-config
description: Configure tcode profiles, models, sub-agents, permissions, limits, MCP servers, and skills
---

# Configure tcode

Use this skill whenever the user asks how to configure tcode, add or switch a
provider/model, pin a sub-agent model, create an agent definition, tune
permissions or limits, connect MCP, or add a skill.

## Configuration locations and precedence

Use these locations deliberately:

- `~/.tcode/config.toml` is the default user-level configuration. Pass
  `--config <PATH>` (or `-C <PATH>`) to select a different personal config
  file for this run; all setup and runtime choices then use that same file.
  Put API-key environment-variable names and personal provider profiles here.
- `[tcode_state]` in the selected user config is runtime state written by
  `/model`, `/agents`, `/suggest`, voice controls, and other UI choices.
  tcode changes only this table through a comment-preserving `toml_edit`
  read-modify-write; do not use it for normal handwritten defaults. The single
  exception is `/model save`, which adds or replaces one `[presets.<name>]`
  table the same way.
- `.tcode/config.toml` is the project overlay. It is appropriate for
  project-specific profiles, sub-agent pins, permissions, hooks, and MCP
  servers. Do not put credentials in a repository configuration file.
- Built-in defaults load first, then the selected user file, then the project
  overlay. Profile entries merge by name; a model entry with the same `name`
  replaces its catalog entry. Project configuration cannot set `[tcode_state]`.

For the initial selection, CLI `--profile` / `--model` win over saved runtime
state; `[tcode_state]` wins over `config.toml` defaults. Start a one-off run
with, for example:

```powershell
tcode --config C:\\path\\to\\work.toml --profile anthropic --model claude-sonnet-5
```

`TCODE_HOME` relocates the whole of that tree. When the variable is set and
non-empty, `~/.tcode/config.toml`, `~/.tcode/agents/`, `~/.tcode/skills/`,
`~/.tcode/AGENTS.md` and `~/.tcode/projects/<project-id>/` (sessions,
checkpoints, plans, task traces, scratchpad, auto memory) all resolve beneath
it instead of the user's home directory; unset, the home directory is used.
It is read per lookup, so it belongs in the environment before tcode starts —
a portable install on a stick, a sandboxed or CI run, or a test suite that
must not touch a developer's real state. Two things deliberately stay outside
it: paths belonging to *other* tools (`~/.codex`, `~/.claude`, read when
importing their sessions) and the voice model cache
(`~/.tcode/voice/models`, which has its own `--model-dir`).

The default config migrates a legacy `~/.tcode/state.toml` once if it has no
`[tcode_state]`; a custom `--config` file never reads that legacy state. Before
editing any configuration, read the applicable existing TOML. Preserve
unrelated profiles and user comments. If a saved `/model` or `/agents` choice
appears to defeat the new default, explain that it lives in `[tcode_state]` and
use the interactive picker to change it rather than deleting state blindly.

## Profiles and models

Choose a default profile and define profiles under `[profiles.<name>]`.
Providers are `anthropic`, `openai` (any OpenAI-compatible Chat Completions
endpoint), and `codex` (a ChatGPT subscription through the Codex backend).
Prefer `api_key_env` to `api_key` so secrets stay out of TOML.

```toml
default_profile = "work"

[profiles.work]
provider = "openai"
api_key_env = "WORK_API_KEY"
base_url = "https://api.example.com/v1" # omit for OpenAI's default endpoint

[[profiles.work.models]]
name = "example-coder"
label = "Example Coder"
context_window = 200000
max_tokens = 16000
vision = true
efforts = ["low", "medium", "high"]
default_effort = "medium"
```

`model = "example-coder"` is a shorthand for a single selectable model.
Use `models` when model metadata matters: `context_window`, `max_tokens`,
`vision`, and valid `efforts`. Do not invent a context window or effort level;
leave unknown metadata unset.

Use `/model` during a session to list and select a model (and optional effort):

```text
/model
/model example-coder high
```

The builtin catalog already contains profiles such as `anthropic`, `openai`,
`codex`, `deepseek`, and `openrouter`; normally only add an environment variable
or override/add the models actually used. The Codex profile uses its local login
and runtime model catalog, not an API key.

Every field of a profile is optional in any single file, because a file is a
patch, not a whole profile. To add a key to a builtin profile, write only that
key — `provider`, `base_url` and `models` keep coming from the layer below:

```toml
[profiles.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
```

The three layers merge builtin catalog → selected user config →
`.tcode/config.toml`, scalar fields overriding and `models` merging by `name`.
Requirements are checked on the merged result, and only for the profile actually
selected: a profile no layer ever gave a `provider` fails with an error naming
it when it is chosen, while leaving it in the file does not stop a session on
another profile. So a brand-new profile (one not in the catalog) must declare
`provider` itself — only profiles that exist in the catalog can be patched with
`api_key` alone.

## Watchdog and retries

`[watchdog]` controls provider request recovery. The defaults are intentionally
conservative: avoid reducing them merely because a healthy request is faster.

```toml
[watchdog]
idle_timeout_secs = 30
connect_timeout_secs = 60
max_retries = 5
initial_backoff_ms = 1000
max_backoff_ms = 30000
```

`idle_timeout_secs` is the maximum silence between streamed response chunks.
`connect_timeout_secs` is the maximum wait for response headers / first byte,
not just TCP connection setup; slow reasoning models can legitimately need much
of this time. `max_retries` is the retry limit. Backoff starts at
`initial_backoff_ms`, doubles for each retry, and is capped by
`max_backoff_ms`.

## Sub-agent model pins

`[agents.<kind>]` pins a delegated agent or auxiliary role to a model. Omitted
fields inherit the current main model. A pin can name a profile, a model, and
an effort; a bare model is resolved against the profile that offers it when
possible.

```toml
[agents.explore]
profile = "openai"
model = "gpt-5.6-luna"
effort = "low"

[agents.plan]
profile = "anthropic"
model = "claude-sonnet-5"

[agents.fetch]
enabled = true # opt in to web_fetch(prompt = "...") using the main model
```

A pin may also be written as a single string instead of a table: `"off"` (same
as `enabled = false`), `"inherit"` (same as `enabled = true`), or a bare model
name (same as `model = "..."`). This is the readable form for a long list:

```toml
[agents]
explore = "gpt-5.6-luna"
suggest = "inherit"
fetch = "off"
```

`/agents` lists and changes these assignments interactively; those choices are
persisted in `[tcode_state]` in the selected config and override both
`[agents.*]` and the active preset. Builtin task kinds are
`explore`, `plan`, `general`, and `orchestrator` (a tool-less coordinator that
only delegates to the other kinds; pin it to an inexpensive model). Auxiliary
roles include `auto`, `suggest`,
`vision`, and opt-in `fetch` (shown as `web-fetch` in the picker). Keep `auto`
and `suggest` on a small, inexpensive model if explicitly pinning them: they
are convenience requests, not the main coding session.

## Presets: named model line-ups

`[presets.<name>]` is a whole line-up under one name — the main model plus what
every sub-agent and role runs on — switched as a unit. `[profiles.*]` says how
to *reach* a provider; a preset says which models to *use*. Switching provider
family is then one choice rather than re-pinning every role.

```toml
[presets.gpt]
label = "GPT"           # optional display name; defaults to the key
profile = "openai"      # main model's profile (recommended when several
model = "gpt-5.6-terra" # profiles offer the same model id)
effort = "high"

[presets.gpt.agents]    # same fields and shorthands as [agents.*]
explore = "gpt-5.6-luna"
plan = { profile = "anthropic", model = "claude-sonnet-5" }
suggest = "inherit"
fetch = "off"
```

Fields: `label`, `profile`, `model`, `effort` are all optional strings;
`agents` is a table of role assignments. Preset names must be letters, digits,
`-` or `_`. Presets may be defined in the user config or a project
`.tcode/config.toml` (a project preset cannot assign `auto`, the safety
classifier). No preset is active by default.

**Resolution order**, lowest to highest: `[agents.*]` from the config files →
the active `[presets.<name>]` → the ad-hoc picks in `[tcode_state]`. The main
model resolves the same way, with CLI `--profile` / `--model` above all.

**Switching is whole-line-up.** `/model preset <name>` (or the presets section
of the `/model` panel) writes `preset = "<name>"` into `[tcode_state]` and
**clears the ad-hoc pins and saved main model there**, so the preset fully
describes what runs. Tweaks made after a switch belong to that visit; to keep
them, save them:

`/model save <name>` captures the running line-up as `[presets.<name>]` and
switches to it. This is the one place tcode writes outside `[tcode_state]`; it
adds or replaces that single table and leaves the rest of the document,
including comments, untouched.

## Custom agent definitions

Create one Markdown file per agent at `.tcode/agents/<name>.md` for a project
or `~/.tcode/agents/<name>.md` for personal reuse. Project definitions take
precedence. The filename supplies `name` when omitted; names must match
`^[a-z0-9][a-z0-9_-]{0,47}$`. `explore`, `plan`, `general`, and `orchestrator`
are reserved builtin names and cannot be overridden.

The YAML frontmatter controls capability and defaults; the Markdown body is the
agent's system prompt:

```markdown
---
name: reviewer
description: Inspect a change and return evidence-backed review notes
readonly: true
tools: [read, grep, glob]
agents: [explore]
profile: openai
model: gpt-5.6-luna
effort: low
maxTurns: 40
gatesOutput: false
max_exchanges: 0
---

Review the requested change. Cite files and lines, distinguish facts from
inferences, and return a concise report.
```

Key rules:

- `description` and a non-empty Markdown body are required.
- `tools` is an allowlist; `disallowedTools` is a denylist. They are mutually
  exclusive. Selectors also support `mcp__*` and `mcp__<server>__*`.
- `readonly: true` is a hard ceiling: mutating tools are removed even if they
  appear in `tools`. It is stronger than any permission mode, because the tool
  is absent rather than gated — there is no approval that could re-enable it.
  It also makes the agent spawnable without approval and lets sibling runs go
  in parallel. Omit it when the agent legitimately needs to change things: a
  sub-agent inherits the caller's permission mode and rules, so its actions
  reach the same approval path the parent's own would have.
- `agents` is the allowlist of nested task kinds; `disallowedAgents` is the
  denylist form: every registered kind except those listed and the agent
  itself, so it automatically covers kinds defined later (the builtin
  `orchestrator` uses `disallowedAgents: []` to coordinate all agents,
  including custom ones). The two forms are mutually exclusive. Omit both to
  make a leaf agent.
- `maxTurns` is a positive integer limiting model round-trips for that task.
  `max_steps` is legacy and should not be used.
- `gatesOutput` defaults to `true`. Set it to `false` only when the parent needs
  the complete final report without a blob read-back. It bypasses only the
  parent-facing final-report blob gate; the sub-agent's internal tool outputs
  still use its normal output budget.
- `max_exchanges: 0` makes the task one-shot; a positive value permits that many
  follow-up exchanges on its live session.

Frontmatter `profile` / `model` / `effort` supplies a default pin. Explicit
`[agents.<name>]` configuration and `/agents` selections take precedence.

## Permissions and limits

```toml
[permissions]
mode = "default" # plan | default | accept-edits | auto | unsafe
allow = ["run(cargo test *)"]
ask = ["shell(rm *)"]
deny = ["shell(rm -rf *)"]

[limits]
auto_compact = true
auto_compact_percent = 85
tool_output_tokens = 8000
max_steps_per_turn = 500
shell_output_filters = true

[ui]
suggest_next = true
show_reasoning = false

[voice]
enabled = false
key = "space"
language = "auto"
```

Rules use the descriptors shown in approval prompts, with `*` as the wildcard.
`deny` and `ask` override broad allows. `accept-edits` auto-approves file edits
only; shell and other non-edit actions still require approval. `unsafe` bypasses
routine prompts but deny rules still apply. Prefer `default` or `accept-edits`
for normal work.

`auto_compact` enables automatic history compaction; set it to `false` only if
you intentionally manage compaction with `/compact`. `auto_compact_percent` is
the context occupancy threshold and is clamped to `1..=100`. `tool_output_tokens`
caps ordinary tool-output context; large output becomes a scratch blob that the
agent can page or read. Do not raise it merely to avoid a single follow-up: use
an agent definition's targeted `gatesOutput: false` only for a final report the
parent genuinely needs whole. `max_steps_per_turn` limits the main agent's model
round-trips; a custom agent's `maxTurns` is separate. `shell_output_filters`
(default `true`) turns the declarative shell output filters on or off; see
"Shell output filters" below. Like every other `[limits]` field it is read from
the **user's** config only — a project's `.tcode/config.toml` cannot re-enable
filtering you turned off.

`ui.suggest_next` controls the post-turn next-prompt guess and costs one small
auxiliary request per turn. `ui.show_reasoning` only displays provider reasoning
summaries; it does not change provider behaviour.

## Voice input

`[voice]` configures push-to-talk dictation (`/voice`). It is TUI-only: hold the
key, speak, release, and the transcript is inserted at the cursor. Nothing is
ever sent automatically. Recognition runs locally in a separate process
(`tcode-voiced`) that tcode starts on demand; no audio leaves the machine.

Dictation goes wherever the text cursor is — the prompt box, an approval note
(`tab`), a question's note, or a plan comment. The pickers have no text field
and are left alone.

The first `/voice on` downloads the backend (~21MB) for the running tcode
version, then the chosen model on first use. Voice is available on Linux
(x86_64, aarch64 — glibc, and needs `libasound2` installed), macOS (both
architectures) and Windows x86_64. Windows on ARM has no speech library
published upstream, so `/voice` says so there instead of offering a download.

| Field | Type / values | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Whether voice comes up with the session. The `/voice` toggle is remembered in `[tcode_state]` in the selected config and **overrides this**, exactly like `/suggest` over `ui.suggest_next`. |
| `key` | `"space"` \| `"ctrl+space"` \| `"f1"`–`"f12"` | `"space"` | The push-to-talk key. `/voice key <name>` changes it and is remembered in `[tcode_state]` in the selected config, which **overrides this** — same precedence as the permission mode. With `"space"`, typing spaces still types spaces: only a burst of presses at key-repeat speed is read as a hold, and only at a word boundary — the start of a line, or right after a space. So dictating into a half-written prompt means typing one space and then holding the next. `"ctrl+space"` is often unavailable — Microsoft Pinyin and other IMEs take it for 中/英 and it never reaches the terminal. A function key always works and needs no telling apart from typing; use one if a slow key-repeat setting makes `"space"` unreliable. `/voice keys` echoes what actually arrives. |
| `model` | preset name, or `""` | `""` | Which recogniser to download and run. `""` means the sidecar's own default (currently `zh-en`). **`/voice model` opens a picker** listing what the installed sidecar supports; it is remembered in `[tcode_state]` in the selected config, which **overrides this**. Each model lives in its own directory, so switching back does not download again. |
| `hotwords` | list of strings | `[]` | Words and phrases recognition is biased towards — library names, project jargon, anything a general model has no reason to expect. `/voice words <w>...` adds, `-<word>` removes, and the result is remembered in `[tcode_state]` in the selected config, which **overrides this**. Only `zh-en` and `qwen3` can use them; see below. |
| `language` | `"auto"`, `"zh"`, `"en"`, `"ja"`, `"ko"`, `"yue"` | `"auto"` | Only `sense-voice` has a language switch; the other models are bilingual by construction and ignore this. |
| `max_seconds` | integer | `60` | A hold that never ends stops here rather than filling memory. |
| `device` | string | `""` | Input device name; empty means the system default. |
| `command` | path | `""` | The `tcode-voiced` executable. Empty means the one downloaded for this tcode version, then a hand-built `~/.tcode/voice/tcode-voiced[.exe]`. |
| `model_dir` | path | `""` | Where model files live. Empty means `~/.tcode/voice/models`. |
| `download_base` | URL | `""` | Base URL for the model archive, for mirrors. Empty means the upstream release. |

```toml
[voice]
enabled = false      # /voice turns it on; this is only the starting value
key = "space"
model = "zh-en"
max_seconds = 60
```

Scope: user config. A project's `.tcode/config.toml` has no reason to bind a
key on someone else's keyboard or point at a binary on their disk.

### Choosing a model

`/voice model` opens a picker; `/voice model <name>` sets one directly. The list
comes from the installed `tcode-voiced` binary, so it always matches what that
build can actually load — the table below is what the current one ships with.

The model is downloaded on first use, into `model_dir`. Bigger models are more
accurate and slower; because a take is transcribed while you wait, speed is a
real cost, not a benchmark number.

| `model` | Download | Languages | Hotwords | Notes |
|---|---|---|---|---|
| `zh-en` | 136MB | zh, en | yes | Trained for Chinese/English code-switching, with punctuation and casing. The default. |
| `sense-voice` | 163MB | zh, en, ja, ko, yue | no | Fastest, and the only one with a `language` switch. Handles English embedded in Chinese poorly. |
| `qwen3` | 879MB | zh, en, and more | yes | The most accurate, and by far the slowest: it decodes with a 0.6B language model on the CPU. |

### Hotwords

`/voice words` shows the list; `/voice words tokio serde` adds; `/voice words
-serde` removes. The words are applied when the recogniser is built, so changing
them restarts the sidecar — the same as changing models.

Only the two models marked above can use them, and they do it differently:
`zh-en` re-scores its search towards the given phrases (which also switches it
to beam search, so it is a little slower while a list is set), while `qwen3`
takes them as context for its language model. `sense-voice` has no mechanism for
this and ignores the list; `/voice words` still records it, so it takes effect
if you switch models.

Hotwords help most with names a general model has no reason to expect — library
names, filenames, people, internal jargon. They cannot fix speech the model
never heard clearly in the first place.

If English technical terms inside Chinese speech come out wrong, that is what
`zh-en` and above are for; `sense-voice` will not get better by changing
`language`.

Hold-to-talk works out how the key is being held from what the terminal
actually does, in three tiers: a real key release where one is available
(tcode requests them — win32-input-mode on Windows, the kitty keyboard protocol
elsewhere); otherwise the OS key *repeat*, which survives a pseudoconsole even
when key-up does not, with the end of the hold being the gap after the last
repeat; and if neither signal exists, the key becomes press-to-start /
press-again-to-stop. Only the last of those changes the gesture, and it says so
in the transcript. The hint row always states the gesture actually in effect,
so it never has to be guessed.

## Auto Mode policy

`[auto_mode]` is global-only safety configuration: project overlays cannot
loosen it. Its values are natural-language rules supplied to the safety
classifier, not descriptor patterns:

```toml
[auto_mode]
hard_deny = ["Never deploy or publish anything."]
soft_deny = ["Avoid modifying CI configuration unless the user explicitly asks."]
allow = ["Creating files inside the session scratch directory is allowed."]
trusted_read_hosts = ["api.github.com", "raw.githubusercontent.com"]
```

`hard_deny` rules cannot be overridden. `soft_deny` rules may be overridden by
specific user intent; `allow` adds exceptions to those soft denials.
`trusted_read_hosts` contains exact host names for tool-declared anonymous HTTPS
read targets only. It does not permit shell, bash, arbitrary URLs, credentials,
or non-default ports. Keep this list small and global because it influences
Auto Mode safety decisions.

## Hooks

Hooks run an external command around matching tool calls in the project working
directory. Each hook receives a JSON object on stdin with `event`, `tool`,
`input`, `output`, and `cwd`; stdout is discarded and stderr is reported.

```toml
[[hooks]]
event = "post_tool_use" # pre_tool_use | post_tool_use
matcher = "edit|write" # `*` wildcard and `|` alternatives
command = "cargo fmt"
timeout_secs = 30
```

`pre_tool_use` runs before the tool. An exit code of `2` blocks that call, using
stderr as the reason; other non-zero exits are reported but do not block it.
`post_tool_use` runs after the tool and reports any failure. `timeout_secs`
defaults to `30`; do not use a hook to perform long-running work.

## MCP and skills

Configure stdio MCP servers as follows:

```toml
[mcp_servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { LOG_LEVEL = "info" }
```

Use the server's documented command and arguments. `env` values are passed
literally; tcode does not expand `${...}` there. Prefer launching through a
small wrapper or setting the real environment in the parent process when a
server needs secrets. MCP tools register as `mcp__github__<tool>` and can be
selected in agent tool policies.

For a project skill, add `.tcode/skills/<name>/SKILL.md`; for a personal one,
use `~/.tcode/skills/<name>/SKILL.md`. A skill begins with `name` and
description frontmatter, then contains instructions loaded on demand. Filesystem
skills override a builtin skill with the same name.

## Shell output filters

Successful `shell`/`bash` output passes through a filter chain that removes
predictable noise (progress counters, install banners, per-crate "Compiling"
lines). Failures are never filtered — a diagnostic always reaches the model
whole — and neither is `output_mode = "final"` or `run_in_background`, which
already park their output elsewhere.

Filters live in `filters.toml`, looked up in this order, first match wins:

1. `<project>/.tcode/filters.toml`
2. `~/.tcode/filters.toml`
3. built-in (`cargo-build`, `cargo-test`, `git-status`, `git-transfer`,
   `npm-install`, `pip-install`, `pytest`, `go-test`)

There is deliberately no `git diff` filter. A diff is information-dense: its
only pure noise is the `index`/`---`/`+++` headers, about 4% of the text.
Anything beyond that means dropping context lines, and large diffs are already
handled better by the output gate, which keeps a per-file summary and saves the
full text to a file.

A filter whose name matches one from a lower level **replaces** it; tcode warns
at startup when that happens. The project file follows `/cd`: changing
directories re-reads the new directory's filters and reports any problem with
them.

```toml
[filters.my-tool]
description = "Drop my-tool's progress lines"
match_command = "\\bmy-tool\\s+build\\b"   # regex over the whole command string
exclude_command = "(^|\\s)--verbose(\\s|$)" # skip when this also matches
strip_ansi = true
strip_lines_matching = ["^\\s*$", "^Downloading "]
max_lines = 40
on_empty = "my-tool: ok"

[[tests.my-tool]]
name = "progress goes, the result stays"
input = """
Downloading thing
Built 3 targets
"""
expected = "Built 3 targets"
```

| Field | Type | Meaning |
|---|---|---|
| `description` | string | Documentation only |
| `match_command` | regex | Required. Matched against the full command string — do not anchor with `^`, real commands are compound (`cd x && cargo build`) |
| `exclude_command` | regex | Skip the filter when this also matches. Stands in for a negative lookahead, which the regex engine does not support |
| `strip_ansi` | bool | Remove ANSI escapes first (default `false`) |
| `replace` | `[{pattern, replacement}]` | Line-by-line substitutions, chained in order; `$1` refers to a capture |
| `match_output` | `[{pattern, message, unless}]` | Collapse the whole output to `message` when `pattern` matches; skipped if `unless` also matches. First rule wins |
| `strip_lines_matching` | regex[] | Drop matching lines |
| `keep_lines_matching` | regex[] | Keep only matching lines. Mutually exclusive with `strip_lines_matching` |
| `truncate_lines_at` | int | Cut each line to N characters |
| `tail_lines` | int | Keep the last N lines |
| `max_lines` | int | Keep the first N lines |
| `on_empty` | string | Text to emit when nothing survived |

Pipeline order: `strip_ansi` → `replace` → `match_output` →
`strip_lines_matching`/`keep_lines_matching` → `truncate_lines_at` →
`tail_lines` → `max_lines` → `on_empty`.

Every `[[tests.<name>]]` case runs the pipeline against `input` and compares it
to `expected`, ignoring `match_command` — so a test states what the rules do.
Write at least one per filter; the built-in set is required to have them.

Two guarantees are worth relying on. Filtering never loses anything: when an
output is shortened the harness saves the untouched text and appends
`[filtered by <filter>: full output at <path>]`, which the agent can `read` or
`grep`. The line names the rule rather than counting the removed lines: a large
removal is usually progress spam, so a count invites re-reading exactly what
the filter saved, while the rule's name says what kind of thing went. And
filtering never costs more than it saves: a result that is not smaller than the
original is discarded and the original is sent.

Unknown fields are an error rather than a silently inert rule. A `filters.toml`
that fails to parse costs only its own filters, with a warning; the rest of the
chain keeps working.

## Safe configuration workflow

1. Identify whether the change is personal/global or project-specific.
2. Read the active `config.toml` and any relevant agent or skill file first.
3. Make the smallest TOML or Markdown change that expresses the requested
   behavior. Never copy credentials into a repository.
4. Validate syntax with the project's available checks, then restart tcode or
   start a fresh session to observe configuration loading.
5. For model and agent choices, use `/model` and `/agents` to confirm the
   effective selection; report if saved state overrides the file.
