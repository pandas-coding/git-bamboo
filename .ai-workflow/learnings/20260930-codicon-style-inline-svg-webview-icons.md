---
title: Codicon-style inline SVG replaces text glyphs in webview icon buttons
date: 2026-09-30
category: pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-2.md
tags: [vscode-extension, ui]
---

# Codicon-style inline SVG replaces text glyphs in webview icon buttons

## Context
The Commit view's row/section action buttons used text glyphs (`+`, `−`, `⇄`, `▾`) at small font sizes — they render inconsistently across fonts/DPI and read as unpolished next to VS Code's codicon-font chrome.

## Insight
16×16 inline SVG with `fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"` gives codicon-looking buttons that inherit theme foreground for free. Static SVG markup (hand-written or a constant string inserted via `innerHTML`) is CSP-safe in webviews — CSP's `script-src`/`style-src` don't apply to SVG DOM nodes, and no untrusted data is interpolated. Keep the SVG `aria-hidden` and put `title` + `aria-label` on the button.

## Evidence
Commit `74e4ee8` replaced all text glyphs in `webview/commit.js` (`ICONS` + `iconSvg()` helper) and `webview/commit.html` (bulk buttons, section chevrons); verified in both themes by the user's F5 smoke. Note the contrast: `StatusBarItem.text` only supports `$(codicon)` — no images — so this technique is webview-only.

## Recommendation
For webview iconography, use a small `ICONS` map of static SVG bodies + a wrapper helper; reserve `$(name)` codicons for non-webview surfaces (status bar, view titles, contributed commands).
