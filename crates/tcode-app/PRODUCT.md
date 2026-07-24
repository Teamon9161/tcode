# Product

Scope note: this file describes the **desktop app** (`crates/tcode-app`), not the
tcode workspace as a whole. The terminal frontends have no visual design layer
to capture; this one does.

## Register

product

## Platform

web

## Users

One person: the author, who also wrote the agent harness underneath. He arrives
already fluent in providers, models, permission modes and the tool vocabulary,
so nothing has to explain what a session or an approval is. What he does need is
a place to keep several agent tasks in flight across several repositories at
once — start one in `tcode`, leave it running, start another in `duck_ext`,
come back when one of them needs a human. The job to be done is not "talk to a
model"; the terminal already does that well. It is **holding parallel delegated
work without losing the thread of any of it**.

## Product Purpose

A desktop home for tcode sessions. It exists because a terminal shows exactly
one conversation at a time, and the thing that actually costs attention is the
gap between "I asked for it" and "it needs me again" — a gap the terminal can
only fill by being stared at. The app makes that gap visible at a glance and
navigable in one click.

Success is a boring, specific thing: three tasks running in three folders, and
the app answers "which one needs me right now?" without being read, only
glanced at.

## Positioning

Every screen answers the same question first: **what is running, and what is
waiting for me?** Anything that does not help answer it is secondary furniture.

## Brand Personality

Quiet, precise, made-by-hand. The voice is a good tool's voice: lowercase where
it is a label, sentence case where it is a sentence, no exclamation, no
encouragement, no personality performance. Errors state what happened and what
to do, in that order.

It should feel like a well-made desktop application that happens to drive an
agent — not like a terminal that grew a window.

## Anti-references

**The terminal look.** No phosphor palette, no monospace-everything, no
box-drawing chrome, no blinking block cursor as decoration, no "> " prompts in
the UI layer. Monospace is for things that are literally code, paths and
identifiers; it is not the interface's voice.

## Design Principles

- **Color means something is happening.** The interface is achromatic almost
  everywhere, so the one place chroma appears is unambiguous: a session that is
  running, or one that is parked waiting for a human. Decoration never borrows
  the status palette.
- **Status is legible from across the room.** Session state is carried by
  position, weight and one color, not by a word the user must read.
- **The tool disappears into the task.** Standard affordances, standard
  shortcuts, no invented controls for things that already have a convention.
  Familiarity is the feature.
- **Nothing is hard-coded.** Every color, radius, type size and density value
  is a token, so the entire look is replaceable by dropping in one file. The
  default theme is one implementation of the contract, not the contract.
- **Density without noise.** This is an information-dense surface for one
  expert. Earn density with alignment and whitespace rhythm rather than with
  rules, borders and boxes.

## Accessibility & Inclusion

No external compliance target — one user, one machine. Two floors are kept
anyway, because they are comfort issues over a long working day rather than
compliance ones: body text holds ≥4.5:1 against its surface (large/secondary
text ≥3:1), and every animation has a `prefers-reduced-motion` alternative.
Status is never carried by hue alone; a shape or label always co-signs it.
