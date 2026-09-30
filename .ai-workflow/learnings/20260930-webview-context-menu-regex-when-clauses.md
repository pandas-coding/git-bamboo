---
title: Regex when-clauses collapse per-bucket webview/context menu entries
date: 2026-09-30
category: pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md
tags: [vscode-extension, ui]
---

# Regex when-clauses collapse per-bucket webview/context menu entries

## Context
The Commit view's file rows need different context-menu entries per bucket (staged rows get Unstage, others get Stage) via `data-vscode-context` → `webviewSection`.

## Insight
`when` clauses support `=~` regex matching, so N buckets need only one menu contribution per command instead of one per (command × bucket): `webviewSection =~ /^file-(staged|changes|untracked)$/`, with a narrower regex for stage (`changes|untracked`) and plain `==` for unstage. Pair it with a kebab-case section value built as a template literal (`file-${bucket}`) — no per-bucket capitalization code in the webview.

## Evidence
Phase 2 shipped nine menu entries (three commands × three buckets); the simplicity reviewer flagged the duplication. Commit `74e4ee8` collapsed them to four entries (one per command, unstage uses `==`), dropping the `charAt(0).toUpperCase()…slice(1)` dance from `commit.js`.

## Recommendation
Whenever menu visibility keys off a family of section names, reach for `=~` first; enumerate entries explicitly only when the regex would be unreadable.
