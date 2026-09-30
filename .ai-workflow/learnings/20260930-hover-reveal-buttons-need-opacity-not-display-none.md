---
title: Hover-revealed webview buttons must use opacity, not display:none
date: 2026-09-30
category: pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md
tags: [vscode-extension, ui]
---

# Hover-revealed webview buttons must use opacity, not display:none

## Context
The Commit view reveals action buttons (stage/unstage, bulk Stage All / Unstage All) only on row/header hover, VS Code toolbar style.

## Insight
`display: none` removes elements from the tab order, so hover-revealed buttons are permanently keyboard-unreachable. `visibility: hidden` is also unfocusable. The combination that works: `opacity: 0; pointer-events: none` (element stays in the tab order), revealed by `:hover` / `:focus-visible` / `:focus-within` setting `opacity: 1; pointer-events: auto`, plus an explicit `.action-btn:focus-visible` outline.

## Evidence
Phase 2 shipped `display: none` reveal; the code-quality reviewer flagged it as a warning (feature was mouse-only). Fixed in commit `74e4ee8` — Tab now reaches and reveals the buttons. Side benefit: the revealed block reserves layout space, so rows stop shifting when hovered.

## Recommendation
For any hover-reveal UI in a webview, hide with `opacity` + `pointer-events`, never `display`/`visibility`, and always pair the hover selector with focus selectors.
