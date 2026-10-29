---
title: Short cycles still need per-criterion acceptance checks before "done"
date: 2026-10-29
category: anti-pattern
plans:
  - .ai-workflow/plans/20261025-network-ops-enhancements-phase-5.md
tags: [llm-agent]
---

# Short cycles still need per-criterion acceptance checks before "done"

## Context
Phase 5 (hunk staging + stash UI) was implemented and closed out in a single ~13-minute, 3-commit session — the fastest phase of the network-ops group. The retrospective checked the plan file afterwards.

## Insight
When implementation is fast, plan hygiene degrades invisibly: the plan's status jumped `ready` → `done` (skipping `in-progress`), and none of the acceptance-criteria checkboxes were ever ticked. Earlier phases (1, 3) had dedicated commits to check them off ("Check off phase-1 acceptance criteria"); phase 5 skipped this step. The acceptance list is the only artifact that forces a per-criterion pass/fail reading — without it, "tests pass" silently substitutes for "acceptance criteria met" (they overlap but aren't identical: manual smoke tests like 'stage one of two hunks → commit contains only that hunk' are criteria, not test-suite coverage).

## Evidence
Commit `e287924` ("Mark the phase 5 plan (hunk staging + stash UI) as done") changed exactly one line: `status: ready` → `done`. The plan file's 7 acceptance checkboxes remained `- [ ]` unchecked at close. Compare phase 1's `47d83da` ("Check off phase-1 acceptance criteria"). The gap was only caught during the retrospective, a day later.

## Recommendation
Before marking any plan `done` — especially in fast single-session implementations — walk the acceptance-criteria list explicitly and tick each box (or note why it's deferred). If a criterion is only manually verifiable, record the smoke-test result in the plan. A plan that goes `ready` → `done` with unticked checkboxes is a signal the criteria were never read as a checklist.
