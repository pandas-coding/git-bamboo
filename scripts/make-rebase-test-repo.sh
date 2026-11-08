#!/usr/bin/env bash
#
# Builds a deterministic git repository for manually verifying the
# git-bamboo rebase phase 2 UI (plan: 20261025-rebase-and-history-insights-
# phase-2.md). Every acceptance-criteria item has a matching scenario in
# the MANUAL-TESTS.md written into the created repo.
#
#   scripts/make-rebase-test-repo.sh [target-dir] [--force]
#
#   target-dir  defaults to /tmp/git-bamboo-rebase-manual
#   --force     deletes an existing target (and its -origin.git sibling)
#               before recreating it
#
# Layout (all authors/dates fixed so the graph and the plan panel render
# the same way on every run):
#
#   * 15a4c21 (topic) Add widget docs                        ← 拖拽/上移目标
#   * 8a52e19 Update shared.txt headline                     ← 与 m4 冲突
#   * 9c1e7d3 Document shared.txt footer                     ← 干净重放
#   * 3b7f2a8 Add widget: part 2                             ← squash 目标
#   * 2e4c6f1 Add widget: part 1                             ← squash 目标
#   * 1d9b5e4 WIP: scratch notes                             ← drop 目标
#   | * 6f3a8c2 (main) Rework shared.txt headline            ← 冲突源
#   | * 5e2d7b1 Add changelog
#   |/
#   * 4c8a1f3 (base) Add shared.txt baseline
#   * 0a7e6d5 Initial commit
#
# plus a sibling bare repo <target>-origin.git whose main is one commit
# ("Touch shared.txt from origin") ahead of base — local main and
# origin/main are diverged, so an extension Pull (merge) hits a conflict
# (the pull/stash conflict-flow regression scenario).
set -euo pipefail

TARGET="${1:-/tmp/git-bamboo-rebase-manual}"
FORCE="${2:-}"
ORIGIN="${TARGET}-origin.git"

if [ -e "$TARGET" ] || [ -e "$ORIGIN" ]; then
  if [ "$FORCE" != "--force" ]; then
    echo "error: $TARGET (or $ORIGIN) already exists — pass --force to recreate" >&2
    exit 1
  fi
  rm -rf "$TARGET" "$ORIGIN"
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

mkdir -p "$(dirname "$TARGET")"
git init -q -b main "$TARGET"
cd "$TARGET"
git config user.name "Pan Test"
git config user.email "pan@test.example"
git config commit.gpgsign false
# Divergent-branch pull needs an explicit reconcile strategy; plain
# `git pull` (engine Default strategy, no flags) would refuse without it.
git config pull.rebase false

# ---------------------------------------------------------------------------
# base history (main and topic share this)
# ---------------------------------------------------------------------------

cat > README.md <<'EOF'
# git-bamboo rebase manual-test repo

See MANUAL-TESTS.md for the scenario checklist (rebase phase 2).
EOF
git add README.md
commit_as "Pan Test" "pan@test.example" "2026-01-01T10:00:00" "Initial commit"

cat > shared.txt <<'EOF'
headline: git-bamboo manual test repo
stable line 2
stable line 3
stable line 4
footer: end of file
EOF
git add shared.txt
commit_as "Pan Test" "pan@test.example" "2026-01-01T10:05:00" "Add shared.txt baseline"
git branch base # visible merge-base anchor for the graph

# ---------------------------------------------------------------------------
# topic: six commits exercising every plan-panel action
# ---------------------------------------------------------------------------

git checkout -q -b topic

mkdir -p scratch docs
printf 'half-finished notes\n' > scratch/notes.txt
git add scratch/notes.txt
commit_as "Scratch Bot" "bot@test.example" "2026-01-03T09:00:00" "WIP: scratch notes"

printf '# Widget\npart 1\n' > widget.txt
git add widget.txt
commit_as "Widget Dev" "widget@test.example" "2026-01-03T09:10:00" "Add widget: part 1"

printf 'part 2\n' >> widget.txt
git add widget.txt
commit_as "Widget Dev" "widget@test.example" "2026-01-03T09:20:00" "Add widget: part 2"

sed -i 's/^footer: end of file$/footer: documented by topic/' shared.txt
git add shared.txt
commit_as "Pan Test" "pan@test.example" "2026-01-04T09:00:00" "Document shared.txt footer"

sed -i 's/^headline: git-bamboo manual test repo$/headline: updated by topic/' shared.txt
git add shared.txt
commit_as "Pan Test" "pan@test.example" "2026-01-04T09:10:00" "Update shared.txt headline"

printf '# Widget docs\nusage: TBD\n' > docs/widget.md
git add docs/widget.md
commit_as "Pan Test" "pan@test.example" "2026-01-05T09:00:00" "Add widget docs"

# ---------------------------------------------------------------------------
# main: two commits after the fork point — m3 clean, m4 conflicts with t5
# ---------------------------------------------------------------------------

git checkout -q main

printf '# Changelog\n\n- initial release\n' > CHANGELOG.md
git add CHANGELOG.md
commit_as "Main Maintainer" "main@test.example" "2026-01-02T11:00:00" "Add changelog"

sed -i 's/^headline: git-bamboo manual test repo$/headline: reworked by main/' shared.txt
git add shared.txt
commit_as "Main Maintainer" "main@test.example" "2026-01-02T11:10:00" "Rework shared.txt headline"

# ---------------------------------------------------------------------------
# sibling bare "origin": main is one conflicting commit ahead of base
# ---------------------------------------------------------------------------

git init -q --bare "$ORIGIN"
git push -q "$ORIGIN" base:refs/heads/main
git -C "$ORIGIN" symbolic-ref HEAD refs/heads/main

TMPCLONE="$(mktemp -d)"
git clone -q -b main "$ORIGIN" "$TMPCLONE/clone"
cd "$TMPCLONE/clone"
git config user.name "Origin Bot"
git config user.email "origin@test.example"
git config commit.gpgsign false
sed -i 's/^headline: git-bamboo manual test repo$/headline: touched by origin/' shared.txt
git add shared.txt
GIT_AUTHOR_DATE="2026-01-06T09:00:00" GIT_COMMITTER_DATE="2026-01-06T09:00:00" \
  git commit -q -m "Touch shared.txt from origin"
git push -q origin HEAD:main
cd "$TARGET"
rm -rf "$TMPCLONE"

git remote add origin "$ORIGIN"
git fetch -q origin
git branch --set-upstream-to=origin/main main

# ---------------------------------------------------------------------------
# final state: checked out on topic, clean worktree
# ---------------------------------------------------------------------------

git checkout -q topic

# ---------------------------------------------------------------------------
# the checklist
# ---------------------------------------------------------------------------

cat > MANUAL-TESTS.md <<'EOF'
# git-bamboo · Rebase Phase 2 手测清单

对应 plan：`.ai-workflow/plans/20261025-rebase-and-history-insights-phase-2.md` 的 Acceptance Criteria。
每项做完在 `[ ]` 里打 `x` 并记下结果；失败项写明现象。

启动方式：在 git-bamboo 工程里 F5（Run Extension）→ 在弹出的 Extension Development Host 窗口里
File → Open Folder → 本仓库。之后所有操作都在那个窗口里做。

仓库形态（`git log --oneline --graph --all` 验证）：

```
* 15a4c21 (topic) Add widget docs
* 8a52e19 Update shared.txt headline        ← 与 m4 冲突
* 9c1e7d3 Document shared.txt footer
* 3b7f2a8 Add widget: part 2                ← squash 目标
* 2e4c6f1 Add widget: part 1
* 1d9b5e4 WIP: scratch notes                ← drop 目标
| * 6f3a8c2 (main) Rework shared.txt headline
| * 5e2d7b1 Add changelog
|/
* 4c8a1f3 (base) Add shared.txt baseline
* 0a7e6d5 Initial commit
```

（sha 每次生成不同，忽略前缀。`<dir>-origin.git` 是 pull 冲突场景用的本地裸远程。）

---

## 1. 面板打开与 todo 一致性

- [ ] graph 里右键 `main`（6f3a8c2 那行）→ **Rebase onto This Commit…** → 编辑器区弹出 Rebase 面板。
- [ ] todo 列表 6 行，与 `git log --oneline main..topic` 完全一致（t1→t6，最旧在上）。
- [ ] 再右键 `base`（4c8a1f3）→ 仍弹面板，todo 同样 6 行（rebase 到更早的基点）。
- [ ] 右键 `topic` 自己的 tip（15a4c21，当前分支祖先链已包含）→ 通知 "nothing to rebase"，**不弹面板**。

## 2. 编辑组合执行（拖拽 + drop + squash + reword）

在面板（onto = main）里按下面计划修改：

1. `WIP: scratch notes` 行动作改为 **drop**（行变灰、可 Restore 恢复）。
2. `Add widget: part 2` 动作改为 **squash**（消息输入框留空 = 折叠进上一条）。
3. `Add widget docs` 动作改为 **reword**，消息填 `Document the widget`。
4. 把 `Add widget docs` 行**拖拽**到 `Document shared.txt footer` 之上（或用 ↑ 按钮）。
5. 点 **Execute** → 确认 modal 摘要应为：**5 commits**: 3 pick, 1 reword, 1 squash, 1 dropped
   （N = 待重放的 todo 条目数，squash 条目也算一个；最终产物是 4 个 commit）、order differs → 确认。

预期（`git rebase -i main` 同计划对照）：在 t5 处**停在冲突**（见场景 3）——先按场景 3 解决并
Continue，完成后验证：

```sh
git log --oneline main..topic
# Update shared.txt headline     （t5，冲突按你的解算落地）
# Document shared.txt footer     （t4）
# Document the widget            （t6 reword）
# Add widget: part 1             （t2 + squash t3；git 默认折叠，body 保留两条消息）
```

（此期望与命令行 `GIT_SEQUENCE_EDITOR=… git rebase -i main` 同计划实测一致，sha 前缀忽略。）

- [ ] 执行前历史被其它终端改动 → 见场景 5（可单独先测）。
- [ ] modal 摘要与实际操作一致；执行后 `git log` 与上面期望一致（`git log --format='%s' main..topic`
      对照命令行 `git rebase -i` 的结果）。

补充微测：
- [ ] squash 行填写自定义消息 → 提交消息为自定义值。
- [ ] reword 不填消息直接执行 → 引擎停靠等待消息（reword_stop），按 Phase 1 机制编辑后继续。
- [ ] 所有行都 drop → Execute 按钮禁用且 title 提示 "Nothing to replay"。
- [ ] 拖拽句柄按下即松开（未形成拖拽）→ 之后在 reword 输入框里选择文本**不再触发整行拖拽**。

## 3. 冲突停靠流（面板 + 冲突栏 + 三方合并编辑器）

（接场景 2，或重新 `git rebase --onto` 前重置：`scripts/make-rebase-test-repo.sh <dir> --force`）

- [ ] 执行停在 t5 冲突 → 面板切到**停靠视图**：红条提示 1 个未解决冲突、"Open Conflicts" 入口、
      done/remaining 分区、进度 5/6、当前条目高亮。
- [ ] 点停靠视图的冲突入口（或看提交视图）→ 提交视图冲突栏出现 `shared.txt`。
- [ ] 打开三方合并编辑器：**rebase 侧标签正确**（Current = Upstream / Incoming = Your change——
      rebase 进行中由 isRebaseActive 自动判定）。
- [ ] 解决（任选一侧或手动改）→ 保存 → 面板点 **Continue** → 完成 → 面板绿条 "Rebase completed"，
      graph 因通知自动刷新、无半成品分支。
- [ ] Continue 时若还有未解决文件 → 面板出现 blocked 提示（黄条/提示文本），不崩溃。
- [ ] 期间 graph 顶部出现 **Rebase in progress** 状态条（N/M + stopped at sha），带继续/跳过/中止按钮，
      按钮点击**真实生效**（这是 review 修的字段名 bug 的回归点）。

## 4. 中止恢复 pre-rebase tip

- [ ] 冲突停靠状态下记下当前 tip：`git rev-parse topic` → 面板点 **Abort** → 绿条 "Rebase aborted"。
- [ ] `git rev-parse topic` 与中止前记下的**执行前** tip 相同（用 `git reflog` 里的 pre-rebase 记录对照），
      graph 状态条消失，分支回到原 sha。

## 5. stale 计划（用户编辑不被清空）

- [ ] 打开面板（onto = main），随便改几行 todo（别执行）。
- [ ] 终端里：`git commit --allow-empty -m "external change"`。
- [ ] 回面板点 **Execute** → 顶部黄条 "底层历史已变化/计划过期"，**已做的编辑仍在**（不清空）。
- [ ] 点黄条 **Reload plan** → todo 重新加载（含新 commit，仍 6+1 行）。
- [ ] 事后终端 `git reset --hard HEAD~1` 清理。

## 6. reload 序列化恢复

- [ ] 冲突停靠状态（场景 3 中途）→ 命令面板 **Developer: Reload Window**。
- [ ] 面板恢复为**停靠只读视图**（done/remaining + 继续/跳过/中止），不是空白或报错。

## 7. 外部 CLI rebase 检测

> **前置**：引擎已修复 watcher re-arm（仓库目录 rm -rf 重建后自动重新注册 inotify
> 监视并发全量刷新通知）。若当前会话仍运行旧引擎二进制，先 Reload Window 一次。

- [ ] 终端：`git rebase main`（非交互，会在 t5 停）→ graph 状态条出现、Commit 视图
      CONFLICTS 桶出现 shared.txt、已打开的 rebase 面板自动切入停靠视图（不依赖
      引擎簿记，磁盘状态检测）。按钮可用性同场景 3。
- [ ] `git rebase --abort` 或解决完成 → 状态条消失。
- [ ] 变体：`GIT_SEQUENCE_EDITOR=true git rebase -i main`（自动接受 todo）同样被检测。

## 8. pull / stash 冲突流回归（conflictFlow 抽取后行为不变）

切换到 main：`git checkout main`（本地 main 与 origin/main 已分叉且同文件冲突）。

- [ ] 工具栏/命令面板 **Pull**（merge 策略；本仓库已配置 `pull.rebase false`，也可用
      命令面板 **Pull with strategy…** → Merge）→ 冲突通知 + "Open Conflict Files" →
      提交视图冲突栏出现 `shared.txt` → 三方合并编辑器（**merge 侧标签**：
      Current (Yours) / Incoming (Theirs)）→ 解决。
- [ ] 解决后 pull 完成提示出现（或手动 commit 收尾 merge），无回归。
- [ ] stash pop 冲突（可选）：
      ```sh
      sed -i 's/^stable line 2$/stable line 2 (stashed)/' shared.txt && git stash
      sed -i 's/^stable line 2$/stable line 2 (conflicting)/' shared.txt
      git stash pop    # 冲突 → 走同一条冲突栏路径
      ```
      冲突出现在同一冲突栏，解决路径与 pull 相同。

## 9. 入站消息校验（伪造 payload）

> **本轮跳过（已用代码评审覆盖）**：宿主端逐字段校验（`rebasePanel.ts` 的
> `MAX_TODO_ENTRIES` + `executeRebase` payload 校验，malformed → drop + warn）
> 是本轮 review 的 Critical 修复产物，三个 reviewer 已核。运行时篡改测试需注入
> 被篡改的 webview，且 VS Code webview 为双层嵌套 frame，devtools 控制台需选中
> 最内层 frame 才能拿到 `vscode.postMessage`——手动触达成本高、增量价值低。
> 如需手动复跑：Open Webview Developer Tools → 控制台上下文下拉选**最内层
> active frame**（非 top），再执行下列 payload。

命令面板 **Developer: Open Webview Developer Tools** → 在 Rebase 面板对应的 frame 控制台里依次执行：

```js
vscode.postMessage({ type: 'executeRebase', onto: 'deadbeef', todo: [{ id: 'zzz', action: 'nuke' }] })
vscode.postMessage({ type: 'executeRebase', onto: '<面板显示的 onto sha>', todo: new Array(501).fill({ id: 'a'.repeat(40), action: 'pick' }) })
vscode.postMessage({ type: 'rebaseContinue' })  // 无 rebase 在跑
```

- [ ] 面板无任何变化/无报错回显；Extension Host 控制台出现
      `rebase panel: dropped malformed executeRebase payload` warn（前两条）。
- [ ] `git status` / `git log` 完全未动（没有任何引擎写 RPC 被触发）。

## 10. 编译/回归

- [ ] `npm run compile` + `npm run typecheck` 无错误。
- [ ] cargo 改动仅限 watcher re-arm 修复（session.rs + engine_tests.rs，用户批准，
      超出原 Phase 2 红线）：`cargo test` 全绿（112 项，含
      watcher_rearms_after_repo_recreate）。

---

做完后把本文件勾选结果同步回 plan 文件的 Acceptance Criteria（或截图）。
EOF

echo "created: $TARGET"
echo "created: $ORIGIN (bare remote, origin/main = 1 conflicting commit ahead of base)"
echo
echo "current branch: topic (clean) — open the repo in the Extension Development Host"
echo "window and follow $TARGET/MANUAL-TESTS.md"
echo
git log --oneline --graph --all --decorate
