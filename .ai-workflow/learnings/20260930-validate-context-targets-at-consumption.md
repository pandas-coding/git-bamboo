---
title: Validate parked context-menu targets at consumption, not on refresh
date: 2026-09-30
category: decision
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md
tags: [vscode-extension, architecture]
---

# Validate parked context-menu targets at consumption, not on refresh

## Context
Webview context menus hand their target to host commands via a parked slot (right-click parks → menu command takes). Engine notifications (worktreeChanged etc.) can refresh the file list between the right-click and the menu click.

## Insight
Clearing the parked target whenever the list refreshes is wrong: a legitimate engine refresh in that window would break the user's actual menu action. The correct design is consumption-time validation — `takeContextTarget()` checks the target's path is still listed under the same bucket in the last pushed status, and returns nothing if stale. Combine with a TTL and an action↔bucket cross-check (mirroring the menus' `when` clauses) so programmatic invocation of the palette-hidden commands can't act on stale or bucket-mismatched targets.

## Evidence
The code-quality reviewer suggested clearing on `doRefresh()`; analysis showed that breaks the right-click → refresh → menu-click sequence (refreshes fire from engine watchers at any time). Commit `74e4ee8` implemented consumption-time validation instead, and also fixed malformed-payload handling to clear rather than silently keep a prior target.

## Recommendation
For parked/handed-off UI state, prefer validating at the point of use against current authoritative data; only use TTL/clearing as a backstop for programmatic misuse.
