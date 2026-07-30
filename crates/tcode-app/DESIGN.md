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

**Two surfaces, and the step between them is the boundary.** The window itself
— title bar, rail, and the field the panes lie on — is `--chrome`. Every pane is
`--bg`, and inside one the surface runs unbroken from under the pane's header to
the bottom edge: transcript, approval dock and composer are the same sheet, not
three panels stacked with rules between them. A hairline is only drawn where two
regions share a tone (rows within a list, a tool call against the transcript, a
pane's header against its own body). Adding one where the tone already changed is
what makes a window look assembled out of parts, and it is the thing this app was
most often accused of.

**The gutter is the separator, and it is not drawn.** Panes are separated by
`--gutter` of the window's own chrome showing through — no rules, no borders, no
shadows. That gap is also the resize handle and the only place a focused pane's
ring is allowed to land, so it costs nothing and touches nothing. It is the one
structural idea the split view rests on: a pane is a sheet laid on the window,
not a region carved out of it.

`--sunken` marks what is inset into a surface: the composer's field, code
wells, tool output. The composer's field returns to `--bg` on focus, so the
control opens up rather than gaining a second ring.

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

`--gutter` (4px) is separate from that rhythm on purpose: it is not spacing
between things on a surface, it is the width of the window showing between two
sheets. It stays constant when density changes, because a compact theme wants
tighter padding, not thinner seams.

`--measure` (760px) is the conversation's width, shared by the transcript, the
approval dock and the composer. It is one token because those three have to
agree: the dock once carried its own wider number and sat visibly off-axis from
the very change it was asking about. Anything that lines up with the
conversation reads the token instead of repeating the figure.

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
  active / disabled / loading. Disabled restates its colours (`--sunken` fill,
  `--faint` text) rather than dropping opacity: a filled ink button at 40%
  shows the surface through it and reads as mud.
- **Key affordance** — where the real control is a keystroke, show the key
  rather than inventing a button beside it. The composer's send is a return
  glyph inside the field's own box, `--faint` with nothing to send and `--ink`
  once there is; the state is the same mark getting darker. A filled circle
  floating next to the field was a second control competing with the one thing
  on the screen you actually type into.
- **Status pill** — dot + word, wash background. The only place state color
  appears as a fill.
- **Card** — used for sessions on the launchpad, where "a discrete resumable
  thing" genuinely is the affordance. Not used for lists, not nested.
- **Row** — the default list affordance: hairline-separated, hover-tinted, full
  width. Projects, files and sessions in the rail are rows, not cards.

  The rail's rows sit under a **group heading**, which is the folder. That is not
  organisation for its own sake: a conversation is named after its folder, so two
  in one folder were two identical rows and the list could account for both
  without saying which was which. The heading answers *where*, the row answers
  *what for* — the first thing the conversation was asked to do, over what it is
  doing now. Folding a group keeps its count, and keeps "needs you" in words,
  because the one fact this rail exists to publish must not be foldable away.
  Reordering is Alt+arrow plus two buttons on hover, the same vocabulary the plan
  editor uses for the same act.
- **Trace row** — every step in the transcript, whatever kind: one call, a run of
  reads, a run of edits, a concurrent batch, a delegated sub-agent. Chevron,
  label, state; expandable, and its contents indent beneath it. No border, no
  background, no rule between steps — the column's rhythm separates them, and
  consecutive steps sit closer to each other than to the prose around them. Cards
  were tried and are wrong here for a structural reason, not a stylistic one:
  these nest (a group holds calls, a run holds a whole transcript) and nested
  cards are banned. Rows nest by indentation for as deep as it goes.

  Every row is a *thing that happened*, and that is what the shape means. Model
  reasoning used to be one of them and is no longer: it is prose (`--muted`, one
  size down, its own faint tag), shown when the reader asks for it and absent when
  they do not. As a folded row it was indistinguishable from a step the agent took,
  so the column stopped answering "what happened" at a glance — the eye had to
  open rows to find out which ones counted.

  The row's label is our words in the UI face ("Run 2 commands"); monospace is
  reserved for what is literally machine text — the tool's name, the command,
  the path. That distinction is what keeps the trace from reading as a terminal,
  which is the product's stated anti-reference. The tool's name comes from core's
  `display_name()`, so `Read`, `Run` and `Agent` are the same words the terminal
  uses and the app has one casing rule rather than two side by side.

  **Running is the one thing a row may colour.** The step in flight takes
  `--brand-wash` and its name `--brand-text`, and settles back over `--dur-slow`
  when the result lands, so a call finishing is something you see rather than a
  repaint. That is the palette rule exactly: chroma is state, so *running* earns
  it and a tool name never does. The pulsing dot beside it is the app's one
  continuous animation and already reports the same fact; a shimmer or a sweep
  here would be motion for its own sake.

  A delegated run is one row, not two. The `agent` call that started it and the
  run itself are two records of the same step, so the run's row carries the kind,
  the model, the call count, the status and — opened — the report the call came
  back with. Drawn as two, the step took two lines and the first said
  `agent · agent(explore)`: the tool's name twice.
- **Empty state** — teaches the surface (what this panel will hold and how to
  put something in it), never "nothing here". It sits on the column's own left
  edge, not centred: it is the first thing in the conversation, and the
  conversation has one axis.
- **Pane** — a sheet of `--bg` at `--r-sm`, holding a thin header over a body.
  Every pane wears the same frame whatever is inside it, which is what makes a
  diff and a conversation read as two of the same thing rather than a panel
  bolted to the side of the app. The header carries the pane's identity on the
  left and its actions on the right; it never carries window-level controls.
  Focus is a 1px `--brand` ring in the gutter, drawn only when more than one
  pane exists — with one pane, "which is current" is not a question anyone is
  asking, and answering it anyway spends the state colour on nothing.
- **Title bar** — the app draws it (`decorations: false`). The thinnest band in
  the window: back, the rail's fold toggle, and the window buttons. Nothing
  else, and the test for what may join them is whether the thing belongs to the
  *window* — the rail does, which is why its toggle is here and the files toggle
  is not. It deliberately carries no title: once the window can hold several
  conversations, naming one of them at window level is a second and
  sometimes-wrong answer to a question each pane's own header already answers.
  No product mark either — the window is already named by the OS.

  The fold toggle is drawn plain in both states. A pressed treatment would spend
  the brand colour saying "the rail is open" while the rail itself is the
  largest object on screen saying the same thing.

  The display switches sit here too, at the right, and they pass the same test:
  what the window *draws* is the window's, and with two conversations on screen
  "show reasoning" cannot mean one thing in the left pane and another in the
  right. They are a dropdown of one-line switches on the shared `.seated` frame,
  and picking does not dismiss it — the point of a switch is watching what it
  did. Nothing in it changes what an agent does; that lives in the config file,
  and holding that line is what keeps this menu from becoming a preferences
  dialog.
- **Scrollbars** — the platform's own, thin, and invisible until the pointer is
  over the region that scrolls. A permanent track down a pane's edge reads as a
  divider nobody drew.

  Where a scrolling region carries the conversation's axis it reserves the gutter
  on *both* edges (`scrollbar-gutter: stable both-edges`). A scrollbar that takes
  width takes it from one side, which put the transcript's centred column half a
  scrollbar to the left of the composer and the strip below it — neither of which
  scrolls. That is an axis rule wearing a scrollbar's clothes.
- **Progress strip** — one line above the composer, on the same sheet and the
  same `--measure` axis, saying where a multi-phase task stands: the word `plan`,
  the plan's name, the phase it is on, `3/7`, and a completed-fraction meter along
  its bottom edge. Collapsed by default — the phase you are on is the whole answer
  most of the time — and expanding it lists the phases, with prose only under the
  one running. Absent entirely when there is no plan; an empty strip over every
  composer is furniture. The meter is the only place a bar carries chroma, and it
  earns it the same way a status dot does: it is state, and only while a turn is
  actually running.

  Three details are load-bearing. **One box holds the axis** (`.strip-body`) and
  everything is inside it; handing `--measure` to each child instead is a rule any
  child can lose, and one did — the phase list went to the pane's left edge while
  the line above it stayed on the composer's. **The meter runs inside a visible
  track** of that same width: a bar with no visible end is a length nobody can read
  a proportion out of, and this one used to be a fraction of the *pane*, which is
  why it read as a green line of arbitrary length. **It says the word `plan`**: a
  title, a phase and a fraction with nothing naming what they belong to is legible
  only to somebody who already knows the app has plans in it.
- **Composer strip** — the row under the field: `plan first` and the permission
  mode on the left, the usage ring and the model on the right. It reads as one
  sentence — what this message is, what it costs, what answers it — and as a
  caption rather than a toolbar, which is a measured thing and not a mood. Every
  control on it is the same 22px box whether or not it draws a border (`.chip`
  reserves a transparent one, so the single outlined switch cannot be two pixels
  taller than its neighbours), and the row's 4px inset puts the first label on
  the field's own text column. It was a bordered pill standing beside bare
  labels in a band a third taller than anything in it, which is what "not
  particularly tidy" turned out to mean.

  A pane can be a third of the window, so what goes first is chosen rather than
  clipped: the subscription figure (a promotion of what the panel already
  holds), then the model's effort, then the model's name truncates, then the
  group drops to a second line. No control is ever unreachable.
- **Usage ring** — the context window as a 13px donut on the strip, with its
  percentage beside it, opening a panel of the whole account. A ring rather than
  a bar because the strip has one line and no width to spare, and because a
  circle reads as a proportion at that size where a 12px bar reads as a smudge.

  Two budgets meet here and the design keeps them apart everywhere: the
  **context window** is this conversation's and a compaction empties it; a
  **subscription window** (5h, weekly) is the account's and only the clock
  refills it. They never average into one "usage" number — that is how a meter
  ends up saying you are fine at the moment you are out of hours — and the
  subscription's figure appears on the strip only once it is tight enough to
  change what you would do next, named by its window so it cannot be misread as
  a second reading of the first.

  Every meter here is achromatic until it matters. That is the palette rule, not
  a shade of the terminal's: chroma means state, and a third of a window used is
  the number being unremarkable. Amber and red arrive at the thresholds the TUI
  warns at, so the two frontends never disagree about when to look up, and every
  level carries a figure so it is never hue alone. An unconfirmed figure — a
  resumed log, a fresh compaction — is prefixed `≈` rather than rounded into a
  fact.
- **Folder chip** — the pane header's identity and its folder picker, in one
  control. The name and the path were the same fact twice (a conversation is
  named after its folder), and the path was already answering "which folder" —
  making it the control for it too is what let the rail stop carrying a button
  that started a different kind of thing than everything else in it. It sits in
  the pane rather than the title bar because with the window split there are two
  folders on screen and "the current folder" is not a question the window can
  answer. Picking never moves a conversation: a session's folder is fixed when
  it opens, so every item starts one, which is what the menu's heading says.
- **Anchored panel** — the model panel, and the shape to reuse for any control
  that carries several dials at once rather than one list of answers. Three bands
  inside one popover: a chrome band of view switches, the content surface holding
  a scrolling list of rows, and a chrome band of pinned dials at the bottom. The
  tone step between chrome and content is the only separator — no rules — which
  is the same boundary the window draws against a pane, so the scrolling region
  needs no border to announce itself.

  Two things about it are load-bearing rather than stylistic. **What is used most
  is nearest the pointer**: the panel opens upward from its chip, so its bottom
  edge is where the hand already is, and that is where the dials live while the
  long list scrolls above them. And **a panel does not close on a pick** — a menu
  answers one question and dismisses itself, but a surface holding four kinds of
  setting cannot close after one of them and stay predictable; the pick is
  confirmed on the spot by the mark moving instead. It is portalled to the body
  and positioned from its trigger's measured box: a fixed popover inside a pane
  is clipped by it, and a field inside the composer's form submits the message on
  Enter.

  The frame is `.seated` and the measuring is `seat.ts`, both shared with the
  usage panel and the folder menu. Only where a popover opens from and how wide
  it is belongs to each of them — three popovers that lift differently read as
  three different applications, and three copies of "measure the trigger,
  re-measure on resize, dismiss on Escape or a click outside" is three places
  for one of those to go missing.

  Where a row's value is an identifier — a model id, a profile, a role's pin —
  it is set in the mono face, for the same reason a path in the transcript is.
  Grouping is a heading (the profile) rather than a repeated line under every
  row.
- **Editable document** — the plan review's field vocabulary, and the one place
  in the app where text is edited in place. A title or a paragraph is a
  transparent `<textarea>` that grows with its content: `--sunken` on hover, a
  `--focus-ring` border and `--bg` on focus, and no border at rest, so the plan
  reads as a plan rather than as a form. Row controls (reorder, remove, add)
  appear on hover or focus-within — four icons beside every row would compete
  with the text they act on. Never `contenteditable`: model text goes in these
  fields, and a textarea has no path from that to markup (see rule 10).
- **Anchored comment** — a quoted passage and a note, drawn under the field it is
  about, with the quote standing in for a highlight. The highlight is deliberately
  not drawn: it would have to be an overlay mirroring a textarea, and an anchor
  tied to character offsets stops meaning anything the moment the reviewer edits
  that paragraph — which is what they are there to do. The quote is also exactly
  what the model receives, so what is on screen is what is sent.

## Keyboard

The window is arranged from the keyboard, and every binding carries `Mod`
(Ctrl, or Cmd on a Mac). That is not decoration: the hand that works this app is
in a composer nearly all the time, so a bare key is text.

| Key | Does |
|---|---|
| `Mod` + `1…9` | show that conversation in this pane |
| `Mod` + `Shift` + `1…9` | open it beside this one |
| `Mod` + `Alt` + `← ↑ ↓ →` | move focus to the pane that way |
| `Mod` + `W` | close this pane — the conversation keeps running |
| `Mod` + `Alt` + `R` | turn this split from side-by-side to stacked |
| `Esc` | close the pane you are looking into |

Directional focus is decided by the panes' boxes, not by the tree: `row(a,
col(b, c))` cannot say whether → from `a` means `b` or `c`, because the answer
is about where the eye is. Ties break toward straight ahead — drift across the
axis costs double — so from a tall pane the neighbour level with your eye wins
over one that is technically nearer.

They are listed on the empty conversation, which is the one screen with room
for them and the one moment nobody is mid-task. A shortcut nothing ever mentions
is a shortcut nobody uses.

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
