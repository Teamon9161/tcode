# Design

The visual system for `crates/tcode-app`. Strategy and audience live in
[PRODUCT.md](PRODUCT.md); this file is how it looks and how to change it.

## Theme

**Porcelain** — the default and, today, the only theme.

The scene it was composed for: an architect's white worktop. Tracing paper,
graphite lines, and one bottle of olive ink that only comes out to mark the
thing that matters. The surface contributes no mood of its own — it is literal
white — and every trace of character is carried by the ink color, the type and
the spacing rhythm. That is deliberate: a tinted "warm paper" background is the
single most common way a light interface announces that nobody chose it.

Color strategy: **Restrained**. Achromatic neutrals plus one brand hue that
never appears decoratively. Chroma on this screen always means state.

## Color

OKLCH throughout. Every value below is a token; nothing is written literally in
a component.

### Surfaces

| Token | Value | Use |
|---|---|---|
| `--bg` | `oklch(1 0 0)` | Content: transcript, launchpad body, dialogs |
| `--chrome` | `oklch(0.972 0.004 130)` | Second layer: title bar, rails, side panels |
| `--sunken` | `oklch(0.955 0.006 130)` | Insets: inputs, code wells, tool output |
| `--line` | `oklch(0.905 0.005 130)` | Hairline separators |
| `--line-strong` | `oklch(0.845 0.008 130)` | Borders that carry structure |

The neutrals carry `chroma 0.004–0.008` at the brand's own hue, not a default
warm tint. Hue 130 at that chroma reads as cool paper grey and stays clear of
the cream/sand band that light AI interfaces settle into.

### Text

| Token | Value | Contrast on `--bg` | Use |
|---|---|---|---|
| `--ink` | `oklch(0.24 0.012 130)` | 16.4:1 | Headings, primary text, values |
| `--body` | `oklch(0.38 0.01 130)` | 10.0:1 | Running prose, transcript body |
| `--muted` | `oklch(0.53 0.012 130)` | 5.3:1 | Labels, metadata, secondary |
| `--faint` | `oklch(0.655 0.01 130)` | 3.2:1 | Non-essential only; never prose |

`--muted` holds 4.6:1 even on `--sunken`, the darkest surface it lands on. That
is why it is 0.53 and not the prettier 0.60 — a placeholder or a timestamp is
still text somebody reads.

### State

The whole point of the palette. Three signals, each with a solid (dots, bars,
fills) and a wash + text pair (pills, callouts).

| State | Solid | Text | Wash | Meaning |
|---|---|---|---|---|
| running | `--brand` `oklch(0.47 0.125 132)` | same | `oklch(0.96 0.035 132)` | An agent is working |
| needs you | `--amber` `oklch(0.665 0.152 60)` | `oklch(0.47 0.11 58)` | `oklch(0.955 0.03 72)` | Parked on an approval |
| failed | `--danger` `oklch(0.545 0.2 27)` | `oklch(0.49 0.19 27)` | `oklch(0.958 0.018 28)` | The turn errored |

Idle and finished sessions are achromatic. That is what makes the colored ones
findable without reading.

`--brand` doubles as identity: the mark, the focus ring, the current selection,
links. Because it is the same olive as "running", the app's own color and its
liveliest state are one thing — intentional, not a shortage of colors.

Status is never hue-only: every state also carries a distinct glyph (filled dot
pulsing / hollow ring / cross) and a word.

### Filled controls

Primary buttons are **ink-filled with white text** (16.4:1), not brand-filled.
The olive is reserved for state, so spending it on every submit button would
break the rule the palette exists to enforce. `--brand` filled with white text
is available (6.5:1) and used only where the action *is* the state — "resume",
"run".

## Typography

Two families, paired on a real contrast axis rather than two similar sans.

- **Instrument Sans** (variable, 400–700) — interface. A slightly narrow modern
  grotesque; carries labels, buttons, headings and prose in one family, as
  product UI should.
- **IBM Plex Mono** (400/500/600) — anything that is literally machine text:
  paths, tool names, identifiers, code, diffs, model ids. Its engineered
  humanist shapes read as a technical document, not a terminal.

Both are bundled via `@fontsource*` and served locally. The webview has no
network entitlement for fonts and must not gain one.

Fixed rem scale, ratio ~1.15 — not fluid. This is product UI at a consistent
DPI; a heading that shrinks inside a narrow rail looks worse, not responsive.

| Token | Size / line-height | Use |
|---|---|---|
| `--text-2xs` | 11px / 1.4 | Pill labels, counts |
| `--text-xs` | 12px / 1.5 | Metadata, path lines |
| `--text-sm` | 13px / 1.55 | Controls, rail items, tool cards |
| `--text-base` | 14px / 1.65 | Transcript body |
| `--text-md` | 16px / 1.4 | Card titles, section headings |
| `--text-lg` | 20px / 1.3 | Launchpad heading |
| `--text-xl` | 26px / 1.2 | Greeting |

Prose caps at 72ch. Monospace blocks may run wider.

## Space & shape

An 8px base with a 4px half-step. `--density` scales the whole rhythm, so a
theme can ship compact or roomy without touching a component.

Radii climb with the element's size (`--r-xs` 4px … `--r-lg` 12px) so a pill
inside a card never looks flatter than its container. Nested cards are banned
outright; a panel inside a panel gets a hairline, not a second border and
radius.

Elevation is two shadows only — a resting one for cards and a lifted one for
dialogs and popovers — both tinted with the ink hue rather than pure black.

## Motion

150–220ms, `ease-out-quart`. Motion reports state and nothing else: a card
lifting on hover, a pill's pulse while a turn runs, a panel sliding in. No
entrance choreography on load — the app opens into a task.

The running pulse is the one continuous animation in the product, and it is
tied to a real fact (a turn in flight). Under `prefers-reduced-motion` it
becomes a static filled dot; nothing else in the interface animates at all.

## Component vocabulary

One shape per job, used everywhere:

- **Button** — three intents (primary ink-filled, secondary outlined on chrome,
  ghost) × one size scale. Every one ships default / hover / focus-visible /
  active / disabled / loading.
- **Status pill** — dot + word, wash background. The only place state color
  appears as a fill.
- **Card** — used for sessions on the launchpad, where "a discrete resumable
  thing" genuinely is the affordance. Not used for lists, not nested.
- **Row** — the default list affordance: hairline-separated, hover-tinted, full
  width. Projects, files and sessions in the rail are rows, not cards.
- **Tool call** — a collapsed hairline block, expandable to full output. Header
  is mono; body is mono in a `--sunken` well.
- **Empty state** — teaches the surface (what this panel will hold and how to
  put something in it), never "nothing here".

## Theme packs

A theme is **one CSS file** that assigns the token contract. `base.css` holds
structure and never a literal color, size or radius; `themes/porcelain.css`
holds the values. Swapping the import swaps the entire look, including
typography, density, radii and shadows — not just the palette.

Rules that keep this true:

1. A component may only reference `var(--token)`. A literal color, px radius or
   font stack in a component file is a bug.
2. Any new visual constant becomes a token in `base.css`'s contract block with
   a documented fallback, so an older theme file still renders.
3. Semantic tokens are what components use (`--surface-panel`, `--text-muted`).
   Raw scale tokens (`--olive-50`) never appear outside a theme file.
4. Themes may override the type stack and `--density`; they may not change the
   token *names*. The names are the contract.
