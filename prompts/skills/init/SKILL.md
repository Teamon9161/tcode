---
name: init
description: Analyze the codebase and create or improve the project instructions file (AGENTS.md)
---

# Initialize project instructions

Analyze this codebase and write (or improve) the instructions file future agent
sessions load automatically. The goal is a file that saves a future session
real turns — not a tour of the codebase.

## What to add

1. Commands that will be commonly used: how to build, lint, and run tests,
   including how to run a single test.
2. High-level code architecture and structure so future instances can be
   productive quickly — the "big picture" that requires reading multiple files
   to understand, not what any single file already says about itself.
3. Conventions that are not obvious from reading one file in isolation: naming
   patterns, error-handling style, where a given kind of change belongs, things
   that look like they'd work but are actually wrong here.

## Where it goes

Check, in order, for an existing instructions file: `.tcode/AGENTS.md`,
`AGENTS.md`, `CLAUDE.md`. If one exists, suggest improvements to it in place —
keep what is still accurate, fix what is stale, add what is missing, and do not
repeat yourself. If none exists, create `AGENTS.md` at the project root (the
cross-tool convention) and prefix it with:

```
# AGENTS.md
This file provides guidance to tcode and other coding agents when working
with code in this repository.
```

## Usage notes

- Skim representative source files, README, and any existing instructions
  file (`.tcode/AGENTS.md`/`AGENTS.md`/`CLAUDE.md`), plus `.cursor/rules/` or
  `.cursorrules` and `.github/copilot-instructions.md` if present — fold their
  important parts in rather than re-discovering the same ground. Do not
  enumerate every directory; a minute of reading representative files beats
  exhaustive traversal.
- Do not repeat yourself, and do not include obvious instructions that apply
  to any project regardless of what it does — "provide helpful error messages
  to users", "write unit tests for all new utilities", "never include
  sensitive information (API keys, tokens) in code or commits", and the like.
- Avoid listing every component or file — anything a `find`/`ls` discovers in
  seconds does not belong here.
- Don't include generic development practices.
- Do not invent sections like "Common Development Tasks", "Tips for
  Development", or "Support and Documentation" unless the codebase's own files
  (README, existing instructions, etc.) already say those things — a plausible
  section a future session cannot trust is worse than no section.
- Keep it to a few dozen lines. A wall of text is not read; it is skipped.
