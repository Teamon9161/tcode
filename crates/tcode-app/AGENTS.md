# tcode-app — 硬规则

Tauri 桌面前端：Rust 后端（本 crate）+ webview 前端（`ui/`，Vite + React + TS）。后端是持有一个 `Arc<Agent>` 与多个隔离 `Session` 的 supervisor，事件经 Tauri emit 推给 webview。

## 构建与运行

**不在 workspace 里**（理由同 `tcode-voiced`：Tauri 链接平台 webview，Linux 上需要 webkit2gtk + libsoup，`cargo build --workspace` 不能开始要求所有人装这些）。所以命令都在本目录跑：

```bash
cd crates/tcode-app
(cd ui && npm install && npm run build)   # 首次 / 改过前端
cargo build && cargo test                 # 后端 + 集成测试
./target/debug/tcode-app                  # 起 app（把 cwd 作为第一个会话）
```

**`tauri.conf.json` 里刻意不配 `devUrl`。** Tauri 在 debug 构建下只要看见 `devUrl` 就去连它，于是 `cargo run` 会撞上 "Connection refused"——而 `cargo run` 正是这里的主流程。不配它，debug 与 release 一样加载 `frontendDist`（`ui/dist`），代价是改前端要重跑一次 `npm run build`。想要 HMR 就临时加回 `devUrl: "http://localhost:5173"` 并同时起 `npm run dev`，别把它留在提交里。

## 不可违背

1. **装配逻辑不在这里重写**。config 加载、`Arc<Agent>` 组装、开会话全部走 `tcode-frontend`（`boot` / `open_session`）。`src/boot.rs` 只放 app 独有的决定（开哪个文件夹、没配置 provider 时报错而不是画向导——这里没有终端可画）。发现自己在抄 `src/main.rs` 的段落时，那段就该下沉到 `tcode-frontend`。
2. **一切逻辑写在 `Emit` 上，不写在 `AppHandle` 上**。跑 turn 的路径必须能在没有窗口时被测试驱动（`tests/bridge.rs` 用 collector 顶替 webview）。要 `AppHandle` 才能做的事只允许出现在 `main.rs` 与 `impl Emit for AppHandle` 里。
3. **webview 传来的一切是数据，不是指令**，`decision` 字符串尤其如此：认不出的决定一律当拒绝（`ApprovalAnswer::into_approval` 的 `_ =>` 分支），有测试钉住。永远不要为了"宽容"给它加 fallback 到放行的分支。
4. **一个会话同时只跑一个 turn，靠所有权保证**：`SessionHandle` 里的 `Session` 被跑 turn 的一方 `take` 走，结束再放回。不许改成"用一个 bool 标记忙"——那会漂移。
5. **事件名是契约**：`bridge.rs` 的 `AGENT_EVENT`/`APPROVAL_REQUEST`/`TURN_FINISHED` 常量与 `ui/src/types.ts` 里的同名常量必须同时改。`AgentEvent` 的 JSON 信封形状（adjacently tagged，`{type, data}`）由 `tcode-core` 的 `event_wire_tests` 钉住，改它就要同时改 `ui/src/types.ts`。
6. **用到新的 Tauri 内建能力，先改 `capabilities/default.json`**。自定义 `#[tauri::command]` 默认放行，但 core 插件的命令（event 的 `listen`/`emit`、window、fs、dialog…）必须显式授权，**未授权时前端那侧只是 promise reject，没有任何报错会自己冒出来**。这条是踩出来的：漏了 `core:default` 时，turn 正常跑完、事件正常 emit，界面却全空，看起来和"卡死"一模一样。
7. **前端不许有静默 reject 的 promise**。`listen()` / `invoke()` 一律接 `catch`，把原因显示成致命错误屏。第 6 条那个 bug 之所以难查，就是因为它当时是个 unhandled rejection。

## 现有结构

- `src/bridge.rs`：出向事件（`SessionEvent`/`TurnFinished`/`ApprovalRequest`）、入向审批（`ApprovalAnswer`/`Pending`）、`WebviewApprover`、`pump_events`。`Emit` trait 在这里。
- `src/state.rs`：`Supervisor`（agent + 会话表）、`SessionHandle`（会话私有的 session/cancel/pending）、`run_turn`。
- `src/commands.rs`：Tauri command，薄封装，只做参数校验后转 `state`。
- `src/boot.rs`：app 的 composition root。
- `tests/bridge.rs`：scripted provider 驱动真实 agent loop，断言事件流、审批往返、fail-closed、双会话隔离、忙会话拒绝第二个 turn。**测试不打真 API。**
- `ui/src/`：`types.ts`（wire 契约）、`transcript.ts`（事件→块的 reducer，纯函数）、`App.tsx`/`Transcript.tsx`/`ApprovalDialog.tsx`。
- `capabilities/default.json`：webview 的权限授予（见硬规则 6）。
- `icons/`：占位图标，发布前要换成真图。

## 排查手册

界面没反应时，**先看 stderr**，它把"没跑起来 / 跑完了但前端没收到"分得很清楚：

- 无 `turn started` → command 里的 spawn 没起来。
- 有 `turn started` 无 `turn finished/failed` → 卡在 provider 请求。
- 两行都有但界面空 → 前端监听侧。九成是 capabilities 或某个没接 catch 的 promise。
- `could not emit '…'` → 事件名非法（Tauri 只收 `[a-zA-Z0-9-/:_]`）或窗口已关。
