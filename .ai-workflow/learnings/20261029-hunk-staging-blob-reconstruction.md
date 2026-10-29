---
title: Stage hunks by index-blob reconstruction, never `git apply --cached`
date: 2026-10-29
category: pattern
plans:
  - .ai-workflow/plans/20261025-network-ops-enhancements-phase-5.md
tags: [git, rust, vscode-extension]
---

# Stage hunks by index-blob reconstruction, never `git apply --cached`

## Context
Phase 5 added hunk-level staging to git-bamboo (greenfield — the engine previously only staged whole files). The plan chose, from external research of how VS Code's built-in git extension does it, to construct the post-staging file content engine-side and write it into the index as a blob.

## Insight
The reliable way to stage a line range is: read the base blob → apply the selected hunks engine-side → `git hash-object -w --stdin --path` → `git update-index --add --cacheinfo <mode>,<hash>,<path>`. `git apply --cached` is the trap: it fails with "patch does not apply" whenever context lines don't match exactly, and its semantics get murky for partial selections. Blob reconstruction works for ANY line interval — which also means the same engine RPC already supports future line-level staging; only the UI is missing.

Three details that made it work cleanly:
- **Client sends only line ranges, never content.** The engine re-computes the diff at execution time and intersects the requested ranges with the current hunks; if they no longer intersect, return `INVALID_ARGUMENT` (stale diff) instead of applying wrongly. This is the same freshness-at-consumption pattern as context-menu targets and discard.
- **The unstaged diff must be worktree-vs-index, not worktree-vs-HEAD.** Our own plan text got this wrong (`staged=false → 工作区 vs HEAD`); the implementation correctly used plain `git diff` (worktree vs index) and `git diff --cached` (index vs HEAD), matching VS Code. Worktree-vs-index is the precondition for "stage = apply selected hunks onto the index blob" to compose correctly.
- **Rename detection must be off** (`--no-renames`) so every hunk belongs to exactly one path.

## Evidence
Commit `121a71e` implemented `getDiff`/`stageRanges`/`unstageRanges` in `crates/engine/src/write_queue.rs` (L1064+: `current_path_diff` freshness basis, L1203+: `update-index --cacheinfo`, byte-exact `apply_hunks`). 90 engine tests pass including partial-hunk staging, cross-expiry rejection, and `git diff --cached` content assertions. Zero rework across the whole phase — the researched design translated directly.

## Recommendation
For any future partial-content staging (line-level staging, interactive discard, partial cherry-pick), reuse the blob-reconstruction path and its range-intersection freshness check. When writing plans that specify diff semantics, double-check which two trees a diff actually compares — this plan shipped with a wrong one and only the implementation caught it.
