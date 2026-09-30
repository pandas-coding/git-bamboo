---
title: Second consumer of a protocol should trigger shared extraction, not a copy
date: 2026-09-30
category: anti-pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md
tags: [vscode-extension, architecture]
---

# Second consumer of a protocol should trigger shared extraction, not a copy

## Context
Phase 2 of UX polish added webview context menus to the Commit view. Phase 1 had already landed an identical park/take/clear TTL protocol for the graph's context menus in `graphSession.ts`. The plan said "reuse the Phase 1 utility if it landed, otherwise implement the same logic".

## Insight
When a plan offers "reuse the existing utility OR implement the same logic", implementers default to copying. The Commit view got a hand-rolled re-implementation of the graph's context-target machinery (TTL constant, timer resets, three copies of a timer-clearing snippet), which review flagged as a warning — two copies of a subtle protocol drift independently.

## Evidence
Commit `9253e15` duplicated the pattern; the simplicity reviewer flagged it against the plan's own reuse requirement; commit `74e4ee8` had to retrofit `src/contextTargetSlot.ts` (a ~70-line generic slot helper) and rewire both call sites — more work than extracting at implementation time would have been.

## Recommendation
The moment a second consumer of a protocol appears (park/take/TTL, debounce/coalesce, message dispatch), extract the shared module in the same change — "implement the same logic" in a plan is a smell, not a license to copy. When writing plans, phrase this as "extract into a shared module and use it from both views".
