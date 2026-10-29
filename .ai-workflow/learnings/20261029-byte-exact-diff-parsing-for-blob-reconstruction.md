---
title: Diff parsing that feeds blob reconstruction must be byte-exact
date: 2026-10-29
category: decision
plans:
  - .ai-workflow/plans/20261025-network-ops-enhancements-phase-5.md
tags: [git, rust]
---

# Diff parsing that feeds blob reconstruction must be byte-exact

## Context
Phase 5's hunk staging parses `git diff -U3` output and uses the parsed hunks to reconstruct index blob content (see the blob-reconstruction learning). The parsed representation is not just display data — its output is re-hashed into git.

## Insight
When diff output feeds back into git object writes, parse at the byte level (`Vec<u8>`), not `String`. Lossy UTF-8 conversion (e.g. `String::from_utf8_lossy`) would silently corrupt non-UTF-8 file content: the reconstructed blob would hash to a different OID and the file's bytes would be permanently wrong in the index. Display-only parsing can afford lossy conversion; write-path parsing cannot.

Also add a self-consistency guard between the parser and git's own output: re-count the old-side lines (`origin != '+'`) and new-side lines (`origin != '-'`) per hunk and compare against the `@@` header counts — refuse (bail) rather than construct a corrupt blob if they disagree. Handle `\ No newline at end of file` markers explicitly, since trailing-newline state must survive reconstruction.

## Evidence
`crates/engine/src/gix_read.rs` defines `RawFileDiff`/`RawHunk` with `Vec<u8>` fields and per-side `old_no_newline`/`new_no_newline` flags; `apply_hunks` in `write_queue.rs` performs the header-vs-body count check before building content, and `read_blob_bytes` is likewise byte-exact with comments explaining the re-hash corruption risk. This constraint was not anticipated in the plan's research phase — it emerged from correctness reasoning during implementation.

## Recommendation
Any code path where parsed data becomes git object content should be byte-exact end-to-end (diff parse → hunk apply → `hash-object --stdin`) and include a parser-vs-git-header consistency check. Treat "it displayed fine" as insufficient evidence for write-path parsing.
