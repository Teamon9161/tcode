# 新前端（Tauri 桌面 app + 进程内多会话 supervisor）实施计划

## Context

tcode 现在只有终端前端（TUI + 非 TTY 降级的 plain REPL）。目标是做一个更好的前端，能**更好地管理项目文件夹**、并**并行管理多个 agent 任务/会话**。

调查已确认三件事，决定了这份计划的形状：

1. **core 已经是 UI 无关的**（`tcode-core` 对 `tcode-tui` 零反向依赖，已验证）。事件契约完整，已被两个前端验证——TUI 和 `src/printer.rs`（~160 行的完整 plain 前端）。驱动一个完整会话只需 `Agent::user_turn` + `AgentEvent` mpsc 流 + `Approver` trait + `PendingInput`/`PendingMode` 句柄。
2. **"app vs web" 是次要问题**：管理本地文件夹 + 跑本地会话都需要本地 FS/进程能力，纯托管网页做不到。用户已选 **Tauri**：Rust core 直接作为库链接进后端，webview 跑 web UI，`AgentEvent`（全数据、无回调，天然可序列化）经 Tauri event emit 到前端。
3. **"一进程一会话"的硬假设只在 `src/main.rs`（启动读死 cwd）和 tui 的 `App`（单 `session` 字段）里，不在 core**。`Session`/`ToolCtx` 本就是 per-conversation 且 cwd 参数化，per-session 隔离在类型层已成立。用户已选 **进程内 supervisor**。

产出：一个 Tauri 桌面 app，后端是持有 `Arc<Agent>` + 多个隔离 `Session` 的 supervisor，前端并排展示多项目多会话、并行跑任务。

## 需要动的边界（core→tui 方向不搬任何东西）

只有 tui→共享层的下沉，让新前端**不必链接 `tcode-tui`**：

- **model/preset/agents/provider 菜单类型 + 构建逻辑**：现在劈在 `src/main.rs`（`build_menu` / `build_agent_menu` / `build_preset_menu` / `rebuild_from_config` / `run_model_command`，约 `main.rs:75-540`，~600 行）和 `tcode_tui` 的公共 struct（`ModelMenu` / `AgentMenu` / `PresetMenu` / `ModelOption` / `SwitchFn` / `PinFn` / `ApplyPresetFn` / `SavePresetFn`，`crates/tcode-tui/src/model_picker.rs:33-36` 等）。这些是 UI 无关的数据 + 闭包。
- **provider setup 状态机**：`crates/tcode-tui/src/setup.rs`（998 行，自注释 "a state machine that draws nothing"）+ `lib.rs:98-131` 的 `ProviderSetup` / `LoginUpdate` / `CodexLogin`。

## Recommendation — 分阶段实施

### 阶段 0：抽出共享非 UI crate `tcode-frontend` — **已完成**

`crates/tcode-frontend/`（依赖 core/tools/providers，不依赖任何 UI）已建立，硬规则见其 `AGENTS.md`。落地情况：

- ✅ **菜单/preset/agents 数据类型与 builder**：数据类型在 `menu.rs`（`ModelMenu`/`AgentMenu`/`PresetMenu`/`ModelOption`/`AgentModelChoice`/`ProviderSetup`/各 `*Fn`），builder 簇在 `build.rs`（`build_menu`/`build_agent_menu`/`build_preset_menu`/`build_provider_setup`/`rebuild_from_config`）。前置的"警告改返回值"也已做：`agent_models -> (AgentModels, Vec<String>)`，warnings 沿 `RebuiltMenus` 抛给 caller 打印，库里不再有 `eprintln!`。tui `model_picker.rs` 只剩 widget + re-export，`tcode_tui::*` 路径不变。
- ✅ **Agent 组装 helper**：`agent.rs::build_agent(AgentBuild) -> Arc<Agent>`、`session.rs::open_session(SessionSpec) -> Session`（含 `[tcode_state]` 播种与 JSONL create/resume）。
- ✅ **provider setup 状态机**：`setup.rs` 整体下沉，连同 `/login` 的 `CodexLogin`/`LoginUpdate` 契约。原本吃 `crossterm::KeyEvent`，下沉时换成 UI 无关的 `setup::Key`（Up/Down/Enter/Tab/Backspace/Char/Cancel/Other）；`tcode-tui/src/setup.rs` 缩成映射层（crossterm → `Key`，含 release 过滤），`wizard.rs` / `/provider` overlay 经它调用。
  - **一处刻意的语义收紧**：旧代码在 provider 列表里按 `KeyCode::Char(' ')` 匹配而不看 modifier，于是 Ctrl+Space 也会勾选；映射层现在把除 Ctrl+C（= Cancel）外的所有 Ctrl 组合键归为 `Other`。这是把终端细节挡在映射层的直接后果，已由 `tcode-tui/src/setup.rs` 的测试钉住。
- ⏸ **turn-driver（`driver.rs`）**：**刻意推迟到阶段 1 之后**。现在只有两个差异极大的消费者（`src/main.rs::run_turn` ~40 行 vs `app/turn.rs` ~1250 行），照它们抽出的抽象很可能与桌面 app 的实际需要不符。等 app 的事件桥写完、有第三个真实消费者，再回来抽公因子。

验收结果：`cargo build --workspace`、`cargo clippy --workspace --all-targets`、`cargo test --workspace` 全绿，无新增 warning。

关键文件：`crates/tcode-frontend/src/{lib.rs,agent.rs,session.rs,menu.rs,build.rs,setup.rs}`；`src/main.rs`（build_* 已删，改调 `tcode-frontend`）、`crates/tcode-tui/src/{model_picker.rs,setup.rs,wizard.rs,overlay.rs,lib.rs}`（改成消费共享类型）。

### 阶段 1：单会话 Tauri app（打通端到端）— **后端与最小 UI 已完成**

`crates/tcode-app/`（Tauri 2 后端，**不在 workspace**，理由与 `tcode-voiced` 同构：链接 webkit2gtk/libsoup）+ `crates/tcode-app/ui/`（Vite + React + TS）。硬规则见 `crates/tcode-app/AGENTS.md`。

- ✅ **`AgentEvent` 过 IPC**：这项"最高风险"实测是一行 derive——`Usage`/`RateLimits`/`TaskRunStatus`/`PermissionMode` 早已 `Serialize`，`Value` 天然可序列化。用 **adjacently tagged**（`#[serde(tag="type", content="data")]`）而非默认外部标签，因为它是唯一同时覆盖 unit / newtype / struct 三种变体形状的表示，TS 侧才能拿到一个判别联合。信封形状由 `tcode-core` 的 `event_wire_tests` 钉住（含嵌套 `Box<AgentEvent>` 与 `ToolBatchStart` 的元组→位置数组）。
- ✅ **事件桥 / 审批桥**：`bridge.rs`。关键决定是**一切写在 `Emit` trait 上而非 `AppHandle` 上**——跑 turn 的整条路径因此能在没有窗口时被测试驱动。`WebviewApprover` 用 `oneshot` 等前端答复，**认不出的 decision 字符串一律当拒绝**（webview 传来的是数据不是指令）。
- ✅ **supervisor 形状先摆好**：`state.rs` 从第一天就是 `Supervisor{ agent, sessions: HashMap }` + 每会话独立 `SessionHandle{ session: Option<Session>, cancel, pending }`。阶段 2 是往表里多插一条，不是重写。"一会话一次一个 turn"由所有权（take/放回）保证而非 bool 标记。
- ✅ **最小 UI**：`ui/src/` = `types.ts`（wire 契约）+ `transcript.ts`（事件→块的纯函数 reducer）+ `App/Transcript/ApprovalDialog`。流式增量、工具卡片（可展开完整输出）、审批弹窗（默认焦点在"no"上，误按回车不会放行）、中断按钮。视觉刻意从简，留给设计阶段。
- ⏸ **未做**：右侧文件预览侧栏、富文档（ppt/docx）预览、plan 批注。按原计划留到设计阶段用 impeccable skill 做。

验收结果：`cargo test`（app 目录）6 个后端集成测试全过——事件流带 session 标签且顺序正确、审批往返后**文件真的被写入**、不可识别决定 fail-closed、双会话并发不串流、忙会话拒绝第二个 turn。`npm run build` 通过（tsc 严格模式）。**端到端已人工确认**：起 app、发消息、看到流式输出。

端到端打通过程中踩到并已修的两个坑（细节与排查手册见 `crates/tcode-app/AGENTS.md`）：

1. **`devUrl` 不能配**。Tauri 在 debug 构建下只要看见它就去连 vite dev server，而主流程是 `cargo run`，于是白屏 + "Connection refused"。删掉后 debug/release 一致加载 `frontendDist`。
2. **漏了 `capabilities/default.json`**（真正的元凶）。自定义 `#[tauri::command]` 默认放行，但 `listen()` 走 core event 插件，必须显式授权；未授权时 promise 静默 reject，一个监听器都装不上。表现为 turn 正常跑完、事件正常 emit，界面全空——**和"卡死"完全无法区分**。已同时补上：capabilities 授予 `core:default`；前端所有 `listen`/`invoke` 接 catch 显示致命错误屏；后端 emit 失败与 turn 生命周期打 stderr。这三条合起来才让这类失败下次一眼可见。

**UI 设计与交互细节（到设计阶段再展开，用 impeccable skill 来做）**：本计划只定后端契约与骨架；具体的视觉与交互留到设计阶段，届时用 **impeccable skill** 设计。要覆盖的交互目标（先记下，实现时再细化优化）：
- **右侧文件预览侧栏**（对齐 Claude Code）：对话过程中创建/修改的文件，在右侧一小块区域点击即可预览。文件改动信息 core 已给足——`AgentEvent::ToolStart{ input }`（edit/write 的路径与内容）、`ToolEnd{ content }`、以及 checkpoint 里的快照——前端据此维护"本会话涉及的文件"列表。
- 该侧栏**支持富文档预览**：不只是文本/代码 diff，还要能预览 **ppt、docx** 等（走 webview 内嵌渲染或转换预览，具体技术路线设计阶段定）。
- **给 plan / 文档加 comment**：在预览区对内容做批注/评论的交互（如对 plan.md 逐段 comment）。
- 这些是"交互体验"层，全部落在前端，不影响后端契约；后端只需保证文件改动事件与路径信息可被前端消费（现有 `AgentEvent` 已满足）。

剩余验收（需人工）：起 app，对一个项目发消息、看到流式输出、触发一次文件编辑审批并放行。右侧文件预览属于上面标 ⏸ 的设计阶段。

```bash
cd crates/tcode-app && (cd ui && npm install && npm run build) && cargo run
```

### 阶段 2：进程内 supervisor（并行多会话）

后端引入 supervisor：

```rust
struct Supervisor {
    agent: Arc<Agent>,                        // 无状态, 全会话共享
    sessions: HashMap<SessionId, SessionHandle>,
}
struct SessionHandle {
    session: Option<Session>,                 // 跑 turn 时 take 出去(对齐 App 的 take/放回)
    events_rx / approver / cancel,            // 每会话独立
    cwd, project_id,
}
```

- 每会话独立 `ToolCtx`（独立 cwd/scratch/background 注册表——`ToolCtx::with_scratch_dir(cwd,…)` 已支持不同 cwd）。`ToolCtx` 的可变 delegate 槽是 per-ctx 的、约束是"同一 ctx 同时只一个 turn"，每会话各自的 ctx + 各自串行的 turn 天然满足，**不需要改 core**。
- 事件多路复用：每会话的 `Receiver<AgentEvent>` 各自 emit 时带 `session_id`。
- 审批路由：`TauriApprover` 按 `session_id` 把请求送到对应前端会话面板。
- 并发：每会话的 `user_turn` 在独立 tokio task 上跑，互不阻塞。

验收：MockProvider 写一个后端集成测试，并发驱动两个 Session、断言两路事件不串。真机上并排跑两个项目的任务。

### 阶段 3：项目/会话浏览器（文件夹管理）

- Tauri command 枚举 `~/.tcode/projects/*`，对每个项目 `SessionStore::list(data_dir)`（`store.rs:509`，已按 `data_dir` 参数化）聚合出"所有项目 × 所有会话"清单（`SessionInfo{ id, last_user_preview, modified }`）。
- "打开项目文件夹" = 选一个 cwd → `open_session(agent, cwd, …)` 新建/或 `SessionStore::resume(data_dir, id)` 恢复 → 挂进 supervisor 的 `sessions` map。
- 项目文件夹树浏览可用 Tauri 的 fs API 或前端直接读（后端已在本地）。

验收：浏览器里看到多个项目及其历史会话，点开任一会话 resume 并继续对话。

## 复用清单（不要重造）

- 驱动一个会话的完整入口：`Agent::user_turn`（`agent/mod.rs:537`）、`Agent::compact_with_focus`、`Agent::estimate_context_tokens`（`:300`，上下文表所有前端共用）。
- 事件/审批契约：`AgentEvent`（`agent/mod.rs:49`）、`Approver`（`permission.rs:242`）、`PendingInput`/`PendingMode`（`session.rs`）。
- 持久化：`SessionStore::{list,resume,create}`（`store.rs:509/620/576`）、`CheckpointStore`、`Ledger` sink（`main.rs:1064` 的挂法）。
- 斜杠命令：`CommandRegistry::builtin().dispatch(...)`（`commands/mod.rs:165`）——新前端直接复用，自动拿到 /help、/compact、/resume、/model 等。
- 参考实现：`src/printer.rs`（事件→渲染的最小映射）、`src/approver.rs`（`Approver` 的阻塞式实现）、`crates/tcode-tui/src/app/turn.rs`（spawn/own-Session/drain 的完整版）。

## 风险与注意

- **最高风险**：`AgentEvent` 加 `Serialize` 时，内含的 `Value`/`Usage`/`RateLimits`/嵌套 `Box<AgentEvent>`（`TaskRunEvent`）都要能序列化——先审一遍字段。这是 event 过 IPC 边界的前提。
- supervisor 的 `Session` take/放回要严格对齐 tui 现有做法（`app/turn.rs:162/219`），避免一个会话跑 turn 期间被别处并发借用。
- provider client 共享：`Arc<Agent>` 里的 model/provider 句柄多会话共用，确认底层 reqwest client 并发安全（core 现在 sub-agent 并行已在共用，风险低）。
- 阶段 0 是纯重构，必须先让 `cargo test --workspace` 全绿再往上叠 Tauri，别把重构和新功能混进一个不可回滚的大改。

## 验证方式

- 阶段 0：`cargo build --workspace` + `cargo test --workspace` 全绿；手动跑 `cargo run`（TUI）与非 TTY plain REPL 确认无回归。
- 阶段 1/2：后端加 `crates/tcode-app/tests/` 用 MockProvider 脚本化 tool_use（对齐 `plan.md` 的"测试永不打真 API"），断言事件桥输出；阶段 2 断言并发两会话事件隔离。
- 阶段 3：`SessionStore::list` 聚合逻辑单测（临时 `~/.tcode/projects/*` fixture，注意用 `tcode_core::home::testing::temp_home()`）。
- 端到端：起 Tauri app，两个项目并排各发一个任务、各触发一次审批，肉眼确认流式与隔离。
