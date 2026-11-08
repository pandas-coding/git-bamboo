#!/usr/bin/env bash
#
# Builds a deterministic git repository for manually verifying the
# git-bamboo blame UI (plan: 20261025-rebase-and-history-insights-phase-3.md).
# Every 手测 acceptance criterion has a matching scenario in the
# MANUAL-TESTS.md written into the created repo.
#
#   scripts/make-blame-test-repo.sh [target-dir] [--force]
#
#   target-dir  defaults to /tmp/git-bamboo-blame-manual
#   --force     deletes an existing target before recreating it
#
# What the history is built to exercise:
#
#   notes/headline.txt   one line rewritten by three commits in a row
#                      (c1 → c4 → c7): the annotate-previous chain has
#                      three links to walk back, and every step lands on a
#                      different author/date.
#   notes/README.md      added in c2 and RENAMED to notes/guide.md in c6, so
#                      blame crosses the rename (engine `source_path`) and
#                      annotate-previous has to open the parent's README.md.
#   src/config.json      layered edits: some lines untouched since c1, some
#                      rewritten in c5 — a gutter with several authors.
#   big.log              1500 lines added in three chunks and edited twice,
#                      i.e. above the 1000-line viewport-slicing threshold
#                      with attribution that changes while scrolling.
#   (worktree)           headline.txt left dirty and scratch.txt left
#                      untracked: uncommitted-line attribution, and the
#                      buffer-vs-worktree paths.
#   side branch          `other` rewrites config.json, so switching branches
#                      (headChanged) has to invalidate and re-render.
#
# Authors/dates are fixed, so the annotations render identically on every
# run. Nothing is pushed anywhere and no hooks are installed.
set -euo pipefail

DEFAULT_TARGET=/tmp/git-bamboo-blame-manual
# `--force` alone means "recreate the default target".
if [ "${1:-}" = "--force" ]; then
  TARGET="$DEFAULT_TARGET"
  FORCE="--force"
else
  TARGET="${1:-$DEFAULT_TARGET}"
  FORCE="${2:-}"
fi

if [ -e "$TARGET" ]; then
  if [ "$FORCE" != "--force" ]; then
    echo "error: $TARGET already exists — pass --force to recreate" >&2
    exit 1
  fi
  rm -rf "$TARGET"
fi

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

# commit_as <name> <email> <iso-date> <message>  (index already staged)
commit_as() {
  GIT_AUTHOR_NAME="$1" GIT_AUTHOR_EMAIL="$2" \
  GIT_AUTHOR_DATE="$3" GIT_COMMITTER_NAME="$1" GIT_COMMITTER_EMAIL="$2" \
  GIT_COMMITTER_DATE="$3" git commit -q -m "$4"
}

ANN="2026-01-05T09:14:00Z"
BOB="2026-02-11T17:02:00Z"
CYD="2026-03-03T08:45:00Z"
DEE="2026-04-19T21:30:00Z"

mkdir -p "$(dirname "$TARGET")"
git init -q -b main "$TARGET"
cd "$TARGET"
# Pin line endings: a CRLF checkout conversion would change the blamed
# bytes, and this repo is what the annotations are checked against.
git config core.autocrlf false
mkdir -p notes src

# --- c1: baseline (Ann) ----------------------------------------------------
cat > notes/headline.txt <<'EOF'
The quick brown fox
EOF
cat > src/config.json <<'EOF'
{
  "name": "git-bamboo",
  "retries": 3,
  "verbose": false,
  "endpoint": "https://example.invalid/api"
}
EOF
git add -A
commit_as Ann ann@example.com "$ANN" "Add headline and config baseline"

# --- c2: notes/README.md (Bob) — renamed to notes/guide.md in c6 -----------
# Eight lines, and c6 changes only two of them: rename detection is
# similarity-based, so a three-line file rewritten in the move is seen as
# delete+add (git reports D/A, blame stops at the rename) and the scenario
# this commit exists for never happens.
cat > notes/README.md <<'EOF'
# Notes

Written by Bob in the second commit.

This file exists so that a later commit can rename it: blame has to follow
the rename instead of stopping there and attributing every older line to
the renaming commit.

Nothing else lives in this file.
EOF
git add -A
commit_as Bob bob@example.com "$BOB" "Document the notes file"

# --- c3: big.log first 500 lines (Cyd) ------------------------------------
mkdir -p .
: > big.log
for i in $(seq 1 500); do printf 'line %04d: alpha\n' "$i" >> big.log; done
git add -A
commit_as Cyd cyd@example.com "$CYD" "Add big.log: first 500 lines"

# --- c4: headline rewritten (Dee) -----------------------------------------
cat > notes/headline.txt <<'EOF'
The quick brown fox jumps
EOF
git add -A
commit_as Dee dee@example.com "$DEE" "Extend the headline"

# --- c5: config layered edits (Ann) + big.log 501-1000 (Ann) --------------
cat > src/config.json <<'EOF'
{
  "name": "git-bamboo",
  "retries": 5,
  "verbose": true,
  "endpoint": "https://example.invalid/api",
  "timeoutSeconds": 30
}
EOF
for i in $(seq 501 1000); do printf 'line %04d: beta\n' "$i" >> big.log; done
git add -A
commit_as Ann ann@example.com "2026-05-02T11:20:00Z" "Raise retries, add timeout, grow big.log"

# --- c6: rename notes/README.md (Bob) + big.log 1001-1500 (Bob) -----------
git mv notes/README.md notes/guide.md
cat > notes/guide.md <<'EOF'
# Guide

Written by Bob in the second commit.

This file exists so that a later commit can rename it: blame has to follow
the rename instead of stopping there and attributing every older line to
the renaming commit.

Nothing else lives in this file.
Renamed in this commit — blame must follow the move.
EOF
for i in $(seq 1001 1500); do printf 'line %04d: gamma\n' "$i" >> big.log; done
git add -A
commit_as Bob bob@example.com "2026-06-08T14:05:00Z" "Rename notes to guide, finish big.log"

# --- c7: headline rewritten again (Cyd) + config endpoint -----------------
cat > notes/headline.txt <<'EOF'
The quick brown fox jumps over the lazy dog
EOF
# Touching config.json here as well gives that small file three distinct
# annotation labels (Ann 01-05, Ann 05-02, Cyd 07-21): scenario 1 checks a
# mixed-author gutter, which a single-author file cannot show.
python3 - <<'PY'
import io

p = 'src/config.json'
text = io.open(p, encoding='utf-8').read()
io.open(p, 'w', encoding='utf-8').write(
    text.replace('https://example.invalid/api', 'https://v2.example.invalid/api')
)
PY
git add -A
commit_as Cyd cyd@example.com "2026-07-21T07:55:00Z" "Complete the headline, move the endpoint"

# --- c8: big.log edited in two places (Dee) -------------------------------
python3 - <<'PY'
import io
p = 'big.log'
lines = io.open(p, encoding='utf-8').read().splitlines(True)
lines[249] = 'line 0250: edited by Dee\n'
lines[1249] = 'line 1250: edited by Dee\n'
io.open(p, 'w', encoding='utf-8').writelines(lines)
PY
git add -A
commit_as Dee dee@example.com "2026-08-14T19:40:00Z" "Edit two big.log lines"

# --- side branch: a different config.json (Ann) ---------------------------
# Branched from the TIP, not from an older commit: the worktree is left dirty
# below, and a branch that also changed headline.txt would make `git switch
# other` (scenario 7) abort with "local changes would be overwritten".
git branch other HEAD
git switch -q other
cat > src/config.json <<'EOF'
{
  "name": "git-bamboo",
  "retries": 9,
  "endpoint": "https://other.invalid/api"
}
EOF
git add -A
commit_as Ann ann@example.com "2026-09-01T10:00:00Z" "Rework config on the side branch"
git switch -q main

# --- worktree state: one dirty file, one untracked file -------------------
cat > notes/headline.txt <<'EOF'
The quick brown fox jumps over the lazy dog
...and keeps running (unsaved edit)
EOF
cat > scratch.txt <<'EOF'
never added to git
second untracked line
EOF

cat > MANUAL-TESTS.md <<'EOF'
# Blame 手测清单（Phase 3）

仓库由 `scripts/make-blame-test-repo.sh` 生成，作者/日期固定，标注渲染每次一致。
先打开仓库 → 命令面板 **Git Bamboo: Toggle Blame Annotations**（或标题栏历史图标）。
所有条目对应 plan 的 Acceptance Criteria。

## 1. gutter 标注（开关 + 弱化配色）

- [ ] 默认**无**标注；toggle 后 `src/config.json` 每行左侧出现 `作者, 相对时间`
      （Ann/Bob/Cyd/Dee 混排），斜体、灰色（`gitDecoration.ignoredResourceForeground`），
      不与代码抢色。
- [ ] 标注列宽全文件一致，代码左边不参差；滚动时列宽不变（代码不横向跳动）。
- [ ] 再 toggle → 标注立刻消失（per-editor 开关态）。
- [ ] `git blame --date=short -- src/config.json` 对照：`{` / `name` / `}` 属于 Ann（2026-01-05），
      `retries` / `verbose` / `timeoutSeconds` 属于 Ann（2026-05-02），`endpoint` 属于
      Cyd（2026-07-21）——同一文件三种标注，行号与内容对齐。

## 2. 悬停详情

- [ ] 悬停 `notes/headline.txt` 第 1 行 → 卡片含作者、绝对时间 + 相对时间、短 sha、
      subject（`Complete the headline, move the endpoint`，Cyd）。
- [ ] 卡片里 **Open Commit** → commitDetail 面板打开该 sha；**Open Changes** →
      父↔该 commit 的 `notes/headline.txt` diff。
- [ ] 悬停未提交行（第 2 行 `...and keeps running`）→ 显示 **Uncommitted changes**，
      无命令链接。
- [ ] 悬停**作者时间 ≠ 提交时间**的 commit（rebase/amend 过的历史）→ 卡片多一行
      `Committed <绝对时间> (<相对时间>)`。本仓库生成时两者相等，所以默认**不**显示；
      验证：`git switch other && git commit --amend --no-edit --date='2026-02-01T10:00:00Z'`，
      再悬停 `src/config.json` 第 3 行 → 日历行为 2026-02-01（作者），Committed 行为今天。
- [ ] 悬停 `notes/guide.md` 第 3 行 → 归属 Bob（2026-02-11，`Document the notes file`），
      卡片提示该行来自重命名前的路径（`Renamed at this commit from notes/README.md`）。

## 3. CodeLens 行级动作

- [ ] 光标停在 `notes/headline.txt` 第 1 行 → 行上方出现两条 lens：
      **Jump to Introducing Commit** / **Annotate Previous Revision**。
- [ ] 点 Jump → commitDetail 打开 Cyd 的 `Complete the headline`（= 该行引入 commit）。
- [ ] 上下移动光标 → lens 跟随当前行（无残留、无其它行的 lens）；关闭 blame 后 lens 消失。

## 4. annotate previous 连续回溯 3 层

在 `notes/headline.txt` 第 1 行连续执行 **Annotate Previous Revision**（lens 或命令面板）：

- [ ] 第 1 层 → 打开只读 `git-bamboo:` 文档（Cyd 的父版本），该行标注为 **Dee, 2026-04-19**，
      光标行高亮且行号保持（第 1 行）。
- [ ] 第 2 层 → 标注为 **Ann, 2026-01-05**（`Add headline and config baseline`）。
- [ ] 第 3 层 → 提示 root commit，无更早版本（不打开空文档）。
- [ ] 在 `notes/guide.md` 第 3 行执行 → 回溯后打开的是**重命名前**的 `notes/README.md`
      （只读 `git-bamboo:` 文档）且自动带标注，第 3 行仍归属 Bob（跨 rename 的链式回溯）。

## 5. 脏缓冲 / 未保存修改

- [ ] `notes/headline.txt` 已是脏工作区（生成脚本留下）：标注 2 行，第 2 行为
      **Uncommitted changes**，第 1 行仍是 Cyd。
- [ ] 在编辑器里再敲一行（不保存）→ ~150ms 后新行标注为 Uncommitted changes，
      其余行归属不变（buffer 路径，行号不错位）。
- [ ] 保存 → 重渲染，标注仍与内容对齐（工作区路径），无报错、无重复通知。
- [ ] 打开未跟踪的 `scratch.txt` → 两行均为 Uncommitted changes，无错误弹窗。
- [ ] 撤销工作区改动（`git checkout -- notes/headline.txt`）→ 引擎通知触发失效重渲染，
      第 2 行标注消失。

## 6. 大文件 viewport 切片（>1000 行）

- [ ] 打开 `big.log`（1500 行）并 toggle → 标注立即出现，滚动流畅（无明显卡顿，
      交互 50ms 级）。
- [ ] 滚动到 250 / 1250 行 → 该行标注为 **Dee**（c8 改过），周边为 Cyd/Ann/Bob。
- [ ] 快速甩动滚动条 → 新可见行始终有标注（滚动中重切片，不是停下才补）。

## 7. 引擎通知后失效重渲染

- [ ] `git switch other`（或 UI 切分支）→ `src/config.json` 变为 5 行：`retries: 9` / `endpoint`
      标注为 **Ann, 2026-09-01**（`Rework config on the side branch`），`timeoutSeconds` 行消失，
      无 Cyd/旧归属残留。
- [ ] 切回 `main` → 标注恢复多作者。
- [ ] 另开终端 `git commit`（例如再改一行 config）→ 不切窗口，标注在通知后自动更新
      （TTL 3s + 通知双通道，无需手动 toggle）。

## 8. 边界与降级

- [ ] 对输出面板/webview 文档执行 toggle → 提示 "not available for this document"，
      不进入开启态。
- [ ] 停掉引擎进程后 toggle 已开启的文档 → 一次状态栏提示（不刷屏），标注清空而非半渲染。
EOF

echo
echo "created $TARGET"
echo "  main:   $(git log --oneline -1)"
echo "  other:  $(git log --oneline -1 other)"
echo "  dirty:  $(git status --porcelain | tr '\n' ' ')"
echo
echo "open it in VS Code, toggle blame (Git Bamboo: Toggle Blame Annotations),"
echo "and follow $TARGET/MANUAL-TESTS.md"
