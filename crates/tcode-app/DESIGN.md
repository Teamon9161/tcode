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
| `--bg` | `oklch(1 0 0)` | Content: transcript, panes, dialogs |
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
| `--text-lg` | 20px / 1.3 | Section headings in a panel |
| `--text-xl` | 26px / 1.2 | The empty field's heading |

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

`--rail-w` (232px) is the second such token, for the second pair that has to
agree: the rail's column, and the finder in the title bar above it. The finder
sits over the rail's column because that is the column it searches, so its width
is `--rail-w` minus the fold toggle beside it — written once. Repeated as a
figure in both places it drifts by two pixels, which does not read as a
different width, it reads as a mistake.

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

The running indicator is the one continuously animated thing in the product,
and it is tied to a real fact (a turn in flight). It is one object with two
parts — a pulsing dot, and a soft highlight sweeping the line beside it — not
two effects that happened to accumulate; nothing else in the interface animates
at all, and a second continuous animation anywhere else is a bug.

The sweep is a band of `--brand` at low alpha travelling the row at constant
speed, then holding past the right edge for a beat so two passes read as two
events rather than a loop. The geometry is the TUI's (`theme.rs::shimmer_color`
— one soft band, constant speed, a dwell between passes), because the two
frontends should not report the same fact with two different rhythms. Constant
speed rather than eased: an eased sweep reads as something arriving, and nothing
arrives here.

**It travels the surface, never the glyphs.** `background-clip: text` over a
gradient is banned outright, and it would also have to overwrite the phase's own
colour — where the whole idea, in the terminal and here, is that a live line is
lifted without losing its identity. So the text holds one solid colour and the
sheet under it moves.

Under `prefers-reduced-motion` the dot resolves to a static filled dot and the
band comes to rest off the edge. Nothing is lost: the wash, the brand-coloured
text and the phase's own words all still say a turn is running, which is the
rule that motion may not be the only carrier of a state.

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
- **Card** — nothing uses one. It was the launchpad's affordance for an open
  session, and when that screen folded into the rail the cards went with it:
  the rail is a list, and a list of conversations is rows. Kept in the
  vocabulary as the shape a *discrete resumable thing* would take if one ever
  needed drawing, and as a reminder that it is not the answer to "several of
  something on a surface".
- **Row** — the default list affordance: hairline-separated, hover-tinted, full
  width. Projects, files and sessions in the rail are rows, not cards.

  The rail's rows sit under a **group heading**, which is the folder. That is not
  organisation for its own sake: a conversation is named after its folder, so two
  in one folder were two identical rows and the list could account for both
  without saying which was which. The heading answers *where*, the row answers
  *what for* — the first thing the conversation was asked to do, over what it is
  doing now. Folding a group keeps its count, and keeps "needs you" in words,
  because the one fact this rail exists to publish must not be foldable away.
- **Rail** — the app's only navigation surface, and the reason it has only one
  screen. There used to be a **launchpad** in front of the window: a full page of
  open sessions as cards and every project as a row, which every conversation was
  reached *through* and which the title bar kept a back arrow for. Its "Open"
  section was the rail drawn a second time, so the whole screen was buying two
  things — folders with nothing open in them, and each folder's earlier
  conversations — at the price of a navigation mode. Both are list-shaped. They
  moved into the rail and the screen went.

  So a **group heading is the project**, not "a folder that happens to hold a
  live conversation", and the column has a head and two bands:

  - **live** — projects with a conversation open, in the order the reader
    arranged, expanded by default. Nothing may push this down the column; it is
    the product's whole question.
   - **`Recent`** — folders visited and closed, newest first, collapsed by
    default and **capped**. The column scrolls, so the cap is not about room —
    it is about the *first screen*: forty folders above the fold means "what
    needs me" arrives under a list of where you have been. The rest are one
    click below (`N more`, which lifts the cap for the sitting and is not
    remembered) or one search away.

  One rule covers both: the disclosure means *show me more of this project*, and
  the two resting states come from what a project has rather than from which band
  it is in. Opening a group shows its live conversations and, behind one more
  row (`Earlier · 14`), the ones it can go back to. That row is not a fold being
  coy — building those previews replays every log in the folder, so the count is
  stated before the click, and a project with nothing live skips the row because
  history is all it has.

  **A conversation appears once.** A live session and the log it is writing to are
  the same conversation, and the stored row for it says `open` and does nothing —
  resuming it would put a second ledger on one file. That is what `log_id` on the
  wire is for.

  A project's own acts are one hover `+` (a conversation here) and one `⋯`. The
  menu is where reordering went: two permanent arrow buttons were taking width in
  the one list that has none to spare, for something you do once and then rely on
  for weeks. Alt+arrow still works — a menu is where a rare act is *found*, not
  the only way to do it — and the items differ by band, because arranging is
  meaningless where the order is a timestamp.
- **Finder** — `Ctrl`+`P`, or the field-shaped button in the title bar. It
  searches open conversations and every folder, which is the pair the rail shows;
  what it deliberately does not search is stored conversations, because a preview
  costs a log replay and searching them means replaying every log in every
  project on each keystroke. Matching is plain case-insensitive substring, not
  fuzzy: the corpus is tens of items, and what the plain version buys is that no
  results means the thing is not there rather than that you spelled it in a way
  the scorer disliked.

  It sits in the title bar rather than in the rail, and that is the one placement
  decision worth keeping: finding a conversation is the way *back* to one, so it
  must not live inside the thing you folded away. Its width reads `--rail-w` so
  it ends on the rail's own edge. `New conversation` went the other way, into the
  rail's head, because it is the list's own act — it adds to what is below it.
  It is a plain row, not a filled button: this column is scanned for what is
  running, chroma and weight mean state here, and a permanent control is never
  state.
- **Trace row** — every step in the transcript, whatever kind: one call, a run of
  reads, a run of edits, a concurrent batch, a delegated sub-agent. Chevron,
  label, state; expandable, and its contents indent beneath it. No border, no
  background, no rule between steps — the column's rhythm separates them, and
  consecutive steps sit closer to each other than to the prose around them. Cards
  were tried and are wrong here for a structural reason, not a stylistic one:
  these nest (a group holds calls, a run holds a whole transcript) and nested
  cards are banned. Rows nest by indentation for as deep as it goes.

  **The disclosure is at the row's end, dim until the row is pointed at, on every
  kind of row.** It used to lead on a group and on a run and trail on a single
  call, which gave one column of steps two left edges — a group's label sat a
  glyph right of a call's name, exactly the raggedness one row shape exists to
  remove. A run's chevron used to be permanent as well, on the argument that a
  folded conversation must not hide the thing that opens it; moving it to the end
  retired that, because a permanent control on one row kind out of three is the
  inconsistency a reader actually notices. On a group the whole row is still the
  target and the chevron is only the hint.

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
  it and a tool name never does. A row does **not** sweep: the running indicator
  at the foot of the transcript is the app's one animated object and already
  reports this fact, and a column of five rows each sweeping on its own clock is
  motion for its own sake — the wash arriving and settling is the whole story a
  row has to tell.

  **A step that failed draws no body.** Red and green in this app mean "this is
  what happened to the file", and a rejected edit changed nothing — so the diff
  comes off and the row keeps its `failed` word and one line of error. The
  intended change is still reachable, in its own pane.

  **A step that succeeded draws no output either, when its body already showed
  the result.** `edit` returns its snippet so the *model* need not re-read the
  file; under a diff that is the same change again in a worse notation, for a
  reader who has already seen it. Core publishes the judgement as
  `hide_success_result` and both frontends honour it.

  A delegated run is one row, not two. The `agent` call that started it and the
  run itself are two records of the same step, so the run's row carries the kind,
  the model, the call count, the status and — opened — the report the call came
  back with. Drawn as two, the step took two lines and the first said
  `agent · agent(explore)`: the tool's name twice.
- **Empty state** — teaches the surface (what this panel will hold and how to
  put something in it), never "nothing here". It sits on the column's own left
  edge, not centred: it is the first thing in the conversation, and the
  conversation has one axis.

  The **empty field** — the window with no pane on it — is one of these rather
  than a screen, which is exactly what the launchpad got wrong. It draws no
  sheet, and it says *no conversation on screen* rather than *nothing open*:
  conversations can be running in this window with no pane showing one, and a
  heading claiming otherwise would be the window contradicting the rail beside
  it.

  It is also **the one empty state that is centred**, and the exception is worth
  stating because the left-edge rule above is otherwise absolute. That rule
  exists to keep an empty conversation on the axis it shares with the composer
  and the transcript. Here there is no composer and no transcript — nothing to
  share an axis *with* — so obeying it put a small block in the corner of a wide
  empty window, aligned to nothing. The heading takes `--text-xl` for the same
  reason: with the window to itself, `--text-lg` read as a caption adrift in it.
- **Pane** — a sheet of `--bg` at `--r-sm`, holding a thin header over a body.
  Every pane wears the same frame whatever is inside it, which is what makes a
  diff and a conversation read as two of the same thing rather than a panel
  bolted to the side of the app. The header carries the pane's identity on the
  left and its actions on the right; it never carries window-level controls.
  Focus is a 1px `--brand` ring in the gutter, drawn only when more than one
  pane exists — with one pane, "which is current" is not a question anyone is
  asking, and answering it anyway spends the state colour on nothing.

  **"It never carries window-level controls" was aspirational for a while.** The
  header held six icons, two of which — the browser and the terminals — are the
  window's: there is one of each per window, so a split view drew two browser
  buttons that toggled the same browser, and maximised, the six sat in the
  corner of a product whose principle is that density is *earned*. They moved to
  the title bar's left, beside the rail's toggle and the finder, which is now
  one coherent group: **which surfaces this window is showing.** Expanding a
  pane went conditional in the same pass — it is relative to neighbours, so with
  one pane it was a control whose job you had to guess. Three icons remain, and
  all three are this conversation's: its files, its tree, and hiding it.
- **Title bar** — native window chrome owns the title and the minimize, maximize,
  and close controls. The app toolbar beneath it is the thinnest app-owned band:
  the rail's fold toggle, the finder, the browser and the terminals on the left,
  and display controls on the right. It carries no conversation
  title, because once the window can hold several conversations, naming one at
  app level is a second and sometimes-wrong answer to a question each pane's
  own header already answers. Native chrome is deliberate: an embedded browser
  is a child WebView above HTML content, while the platform caption is outside
  that hit-test region.

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
- **Running indicator** — the last line of a live transcript: a pulsing dot,
  where the turn is right now, and the sweep. It says the phase rather than the
  fact — `thinking`, `writing`, `Run · cargo test -p tcode-core`, `retrying
  (2/5)`, `sub-agent working` — because "is something happening" is already
  answered by the dot, by the rail and by the pane's own status, while "which of
  those is it doing" is answered nowhere else. It said the word `working` for
  every second of every turn, which was the least it could have said and the
  most it could have been wrong about.

  The words are `activity.ts`, derived from the event stream, and they are the
  TUI's `state_label` verbatim. Not a wire contract — nothing breaks if they
  drift — but a person who runs both should not have to learn a second name for
  the same state, and each of those words was already chosen once.

  It is the same string the rail's second line carries for this conversation:
  one question asked from two distances, so one answer. And it hugs its own
  content rather than spanning the measure, so the sweep travels the label
  instead of crossing an empty column — which is the width the TUI shimmers,
  for the same reason.

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
- **Queue strip** — what you said while it was working, above the composer and
  below the conversation, in the order it will be delivered. Typing during a turn
  is the most ordinary thing there is, and the prompt has to go *somewhere*; the
  one place it must not go is invisibly into a queue, because a message that was
  accepted and is not on screen is a message you will send twice. Rows are
  `--sunken` — text on its way in, like the composer's own field — not trace rows,
  because none of this has happened. Each can be taken back on hover. "Stop and
  send now" is the only control here that destroys something (a turn in flight),
  so it is worded as what it does and drawn as an outline rather than as the
  row's default action. When core delivers one at a safe boundary it becomes an
  ordinary part of the transcript and leaves.
- **Rewind** — going back to an earlier prompt. The control sits on the message,
  on hover, because the message *is* the checkpoint: a picker listing prompts by
  their first line would be a second copy of the conversation to read. It does
  not act on the click — this is the only operation in the app that destroys
  conversation and the only one that can undo work on disk — so it opens a docked
  question on the amber wash, above the composer and below the messages it is
  about. Not a modal, for the same reason the approval dock is not (the other
  panes are other conversations), and not a dialog over the transcript, because
  the messages being dropped are the thing to look at while deciding.

  The two halves are asked separately and only one is offered by default:
  dropping messages is recoverable by retyping, while rolling files back throws
  away edits that may have been made by hand since. The file switch starts off,
  and is absent entirely when that era changed nothing — an option that never
  applies is a decision nobody should have to read. The count is stated in words
  because it is the part no click takes back.
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

## The terminal

The one surface in this app that *is* a terminal, in an app whose first
anti-reference is the terminal look. Both hold, because the terminal here is a
**document lying on this surface** rather than a window from somewhere else: it
runs on the app's paper, in the app's mono face, at the app's density, and
everything around it — the strip, the tabs, the labels — stays in the UI face.
What it is not allowed to bring with it is the phosphor palette and the black
rectangle.

**The palette is the theme's, all sixteen slots** (`--term-*` in `base.css`,
values in the theme pack). A program writing into a PTY addresses ANSI colours
by number — `ls`, `git`, every prompt theme — so they are tokens like everything
else, and `termHost.ts` resolves them to sRGB for xterm through the same
converter the artifact sandbox uses (`color.ts`).

Porcelain renders them light, which is the whole difficulty: the sixteen were
designed against black, and a program asking for "bright white" means *shout*,
not *disappear*. So lightness is spent on the background and chroma carries the
distinctions — the bright half gains saturation and stays dark enough to read
(4.5:1 or better on `--term-bg`, except ANSI 7, the dim grey at 3.5:1 that
programs use for rules rather than for text). Green is the olive, red is the
failure red, yellow is the amber that means "parked on you": the three the app
already owns keep their identity, so a `git status` reads as this app's palette
rather than as three borrowed colours.

The surface itself is `--term-bg`, one step off the page — the same "inset into
a surface" move `--sunken` makes for code wells — with the padding a printed
listing has. A first column flush against the pane's edge is the single most
terminal-emulator thing a layout can do.

**Tabs are segments of one strip, divided by a hairline.** Two panes have tabs
— the terminals and the browser — and they wear the same strip, written once
(`.tab*`). It is a pane header like any other: the hairline every `.pane-head`
draws, 32px, on `--bg` for the terminals and on `--chrome` for the browser,
whose body is not this app's surface at all but a page from somewhere else.
Tabs abut — no gaps, no backgrounds, no radii — with a 1px `--line` between
them, which is the separator this app already uses wherever two regions share a
tone. The current one is `--ink` at medium weight with a 2px `--brand` bar
across its full width at the bottom edge: the brand spent on "the current
selection", one of the three jobs it has, and unmistakable for status, because
status is a dot in a gutter and this is a rule under a segment.

Two earlier versions were wrong in opposite directions and both are worth
remembering. The first gave every tab a tinted, top-rounded rectangle — a row of
little cards, exactly what "earn density with alignment and whitespace rhythm
rather than with rules, borders and boxes" rules out, and the only thing in the
window that looked imported from another application. The second dropped the
boxes but tinted a tab on hover, to say which tab the close control at its right
edge belonged to; that is a box again, appearing only when you look at it, which
is worse. The hairline answers the same question permanently and costs nothing.

There was also a plain defect underneath both: this app has no global `button`
reset — every component states its own — and the tab's label and close control
never did, so they carried the platform's grey fill and a 2px outset bevel. Half
of "the tabs look ugly" was that, not the design.

**The close control appears on hover, and its space is held open.** Same rule as
the transcript's disclosures — a cross on every tab at rest is six crosses
competing with the six names the strip exists to show — with the addition that
the name's right padding reserves the slot whether or not it is filled, so the
strip never re-flows under the pointer. The cross is a mark rather than a
button: no plate, no radius, no fill on hover, just ink getting darker.

`Mod` + `J` is three states rather than a switch: closed opens it and puts the
cursor in it, open-but-elsewhere focuses it, focused hides it. The middle state
is the common one by a long way, and a plain toggle answers it by taking the
terminal away. Hiding is not closing — the shells and anything running in them
are untouched, so the next `Mod` + `J` shows the same scrollback with the same
dev server still going.

Inside a terminal the app gives the keyboard back. `Ctrl+C`, `Ctrl+D`,
`Ctrl+R`, `Ctrl+W`, `Ctrl+U` are the shell's, and an app that keeps them is an
app you cannot work in; what stays is `Mod` + `J`, the `Mod` + `Alt` pane moves,
and `Mod` + `Shift` + `T` / `W` for tabs — the same pair the browser's strip
takes, because two tab strips that take different keys are two things to learn.
The cost is honest and worth naming:
`Ctrl+J` can no longer be sent to a shell as a bare line feed. It buys the one
key that gets you back out.

## Keyboard

The window is arranged from the keyboard, and every binding carries `Mod`
(Ctrl, or Cmd on a Mac). That is not decoration: the hand that works this app is
in a composer nearly all the time, so a bare key is text.

| Key | Does |
|---|---|
| `Mod` + `P` | find a conversation or a folder |
| `Mod` + `1…9` | show that conversation in this pane |
| `Mod` + `Shift` + `1…9` | open it beside this one |
| `Mod` + `Alt` + `← ↑ ↓ →` | move focus to the pane that way |
| `Mod` + `W` | close this pane — the conversation keeps running |
| `Mod` + `Alt` + `R` | turn this split from side-by-side to stacked |
| `Mod` + `J` | show, focus, or hide the terminals |
| `Mod` + `Shift` + `T` / `W` | new tab / close this one, in the terminals or the browser |
| `Esc` | close the pane you are looking into |

Directional focus is decided by the panes' boxes, not by the tree: `row(a,
col(b, c))` cannot say whether → from `a` means `b` or `c`, because the answer
is about where the eye is. Ties break toward straight ahead — drift across the
axis costs double — so from a tall pane the neighbour level with your eye wins
over one that is technically nearer.

`Mod`+`P` and not `Mod`+`K`: this goes to a *place*, and the app's other jump —
`Mod`+`1…9` — is the same verb with the list already memorised. `Mod`+`K` would
promise a command palette this app does not have.

They are listed on the empty field and on an empty conversation, which are the
two screens with room for them and the two moments nobody is mid-task. A shortcut
nothing ever mentions is a shortcut nobody uses. The empty field lists only the
keys that move you *between* conversations; the pane verbs belong to a window
that has panes in it.

## Responsive

Structural, not fluid (PRODUCT.md § Design Principles): below 900px the pane
tree stops being drawn and only the current pane is, and the rail keeps its
status dots and drops everything that needs width to read. Nothing shrinks; one
thing is chosen. The finder stays in the title bar at that width, which is what
keeps every conversation reachable *by name* once the rail is a column of
anonymous dots — it is the only way back, so it is the last thing to go.

**Every responsive rule lives at the end of `app.css`, under the components it
overrides.** A media query adds no specificity, so a `display: none` inside one
is beaten by any same-specificity `display:` that appears later in the file.
While this block sat near the top with the shells, every component defined below
it quietly won: half the rail went on drawing inside a 52px column and spilled
across the panes — `needs you` lying over the conversation, a session count in
the field. It reads as an overflow bug and is a cascade-order one, which is why
it survived being looked at. The narrow rail also sets `overflow: hidden` on its
own boxes, so forgetting to list a new element costs a visible label rather than
text across the window.

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
