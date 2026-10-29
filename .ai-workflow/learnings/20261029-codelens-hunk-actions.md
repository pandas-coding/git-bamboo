---
title: CodeLens on the diff's new side for hunk actions (not gutter decorations)
date: 2026-10-29
category: decision
plans:
  - .ai-workflow/plans/20261025-network-ops-enhancements-phase-5.md
tags: [vscode-extension, ui]
---

# CodeLens on the diff's new side for hunk actions (not gutter decorations)

## Context
Phase 5 needed per-hunk [Stage Hunk] / [Unstage Hunk] actions in the diff editors the Commit view opens. The plan offered two options: a CodeLens provider on each hunk's first line, or gutter decorations + command URIs, to be decided at implementation time with the tradeoff recorded.

## Insight
CodeLens won, and the design detail that made it clean: derive the lens target **statelessly from the currently open diff tabs** (left side + right side document URIs of a diff editor opened by this extension), rather than tracking which editor was opened for which purpose. Worktree file (changes bucket) → [Stage Hunk]; `:0` index document via the engine scheme (staged bucket) → [Unstage Hunk]. Unrelated editors can never get lenses because they never match a tracked diff-side pairing.

Supporting pieces: a short-TTL cache of the engine's `getDiff` response (~3s) with immediate invalidation on hunk actions and engine notifications, and lens provider registration filtered to `file` + engine-scheme documents only.

## Evidence
`editors/vscode/src/hunkStaging.ts` (223 lines) implements the controller with the CodeLens-vs-gutter rationale documented in its header comment, per the plan's requirement to record the tradeoff. The plan-mandated record lives in code where the next maintainer will actually see it.

## Recommendation
For per-hunk actions in diff editors opened by an extension, prefer CodeLens over gutter decorations: no margin hover affordances to build, keyboard accessible, and naturally scoped by document selector. Derive targets from the open diff editors' structure instead of maintaining open-intent state — stateless derivation eliminates stale-target bugs by construction. Put design-tradeoff notes in the implementing file's header comment when a plan says "record the decision".
