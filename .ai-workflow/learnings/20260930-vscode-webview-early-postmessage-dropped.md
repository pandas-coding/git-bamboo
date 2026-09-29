---
title: VS Code webview 消息在监听器就绪前会被静默丢弃
date: 2026-09-30
category: anti-pattern
plans:
  - .ai-workflow/plans/20260929-vscode-ux-polish-round1-phase-1.md
tags: [vscode-extension, webview, message-passing]
---

# VS Code webview 消息在监听器就绪前会被静默丢弃

## Context

git-bamboo 的 Commit Graph webview（GraphSession）需要在会话建立时把 refs 与 HEAD 推送给 webview。会话在 `webview.html` 赋值后立即构造，构造函数里调用 `postMessage` 推送初始数据。

## Insight

Extension host 对 webview 的 `postMessage` 没有缓冲：webview document 加载并执行脚本、注册 `window.addEventListener('message')` 之前发出的消息会被**静默丢弃**（无报错、无警告）。`webview.html = ...` 赋值返回 ≠ webview 已加载。凡是"构造时推初始数据"的写法，第一批推送全部丢失，且只有用户后续交互触发通知才会补上——表面上功能"基本正常"，初始状态却缺失。

## Evidence

Phase 1 计划照既有 `sendRefs()`（构造函数推送）的写法新增了 `sendHead()`，review 确认两条初始推送都被丢弃：首次打开 graph 无 HEAD 标记，直到第一次切换分支才出现。而同仓库 commitView.ts 早已注释过"a postMessage before that would be dropped anyway"——同一代码库内两处对同一机制的理解不一致。计划研究阶段断言"ready 由 webview 发出时消息通道必然已通"，该推理只对发送方向成立，没有推广到接收方向。

## Recommendation

- Host→webview 的所有初始推送（refs、HEAD、持久化配置等）统一挂在 webview 发来的 `ready` 握手处理器里，不要在构造函数里推。
- Webview 脚本末尾发 `vscode.postMessage({type:'ready'})`；此时其监听器已注册，握手必然可达。
- Code review 时把"构造函数里的 postMessage"当作红旗检查；引用既有代码的消息推送模式前，确认接收方就绪时序。
