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

### 阶段 0：抽出共享非 UI crate `tcode-frontend`

新建 `crates/tcode-frontend/`（依赖 core/tools/providers，**不依赖任何 UI**）。把上面两块下沉进来：

- 菜单/preset/agents 的数据类型与 builder（从 `tcode-tui` 的 `model_picker.rs` 里剥出纯数据部分 + `src/main.rs` 的 build_* 函数）。渲染层留在 tui，只保留"给我一个 `ModelMenu` 数据"的消费。
  - **进度**：数据类型已下沉到 `tcode_frontend::menu`（`ModelMenu`/`AgentMenu`/`PresetMenu`/`ModelOption`/`AgentModelChoice`/各 `*Fn` 等），tui `model_picker.rs` 只留 widget 并 re-export，`tcode_tui::*` 路径不变，全绿。
  - **builder 下沉的前置改动（未做，需单独一次）**：`build_*` 簇里 `agent_models`/`build_agent_model` 用 `eprintln!` + ANSI 常量直接往 stderr 打警告。原样下沉会让库函数做 binary I/O，违反本仓库"lib 不依赖具体 provider/不打印"的原则（`model_picker.rs` 老注释即此意）。要干净下沉必须先把警告改成**返回值**（`agent_models -> (AgentModels, Vec<String>)`），并沿 `rebuild_from_config` → `build_preset_menu` → `build_provider_setup` 的返回签名把 warnings 抛给 caller 打印。这是行为形状改动，按 AGENTS.md「搬迁=纯提取」原则单独成一次改动，不与本次机械提取混。
- provider setup 状态机（`setup.rs` 整体移入；tui 的 `wizard.rs` / `/provider` overlay 改成消费它）。
- **Agent 组装 helper**：把 `src/main.rs:983-1075` 那段"从 Config 造 `Arc<Agent>` + 初始 `Session`"抽成 `tcode_frontend::build_agent(config, ...) -> Arc<Agent>` 与 `open_session(agent, cwd, mode, rules) -> Session`（内部走 `Session::new(ToolCtx::new(cwd,…).with_model(cell), …)` + 挂 `SessionStore` sink）。三个前端（tui / plain / 新 app）共用。
- **可选**：一个可复用的 turn-driver（封装 spawn `user_turn` + own Session + drain events + 回收 pending 的动作），消掉每个前端手搓这段的重复（对比 `src/main.rs::run_turn` ~40 行 vs `app/turn.rs` ~1250 行）。

验收：`cargo build --workspace` 通过，tui 与 plain REPL 行为不变（改成从 `tcode-frontend` 消费）。这一步不引入 Tauri，纯重构，风险最低，先落地。

关键文件：新建 `crates/tcode-frontend/src/{lib.rs,menu.rs,setup.rs,build.rs,driver.rs}`；改 `src/main.rs`（删 build_*，改调 `tcode-frontend`）、`crates/tcode-tui/src/{model_picker.rs,setup.rs,wizard.rs,lib.rs}`（改成消费共享类型）。

### 阶段 1：单会话 Tauri app（打通端到端）

新建 `crates/tcode-app/`（Tauri 后端）+ 前端目录（Vite + React/Vue，UI 框架由前端目录自行决定）。

- Tauri command `send_message(session_id, text)` → 在后台 tokio task 上跑 `agent.user_turn(...)`。
- **事件桥**：一个 `TauriEventSink` 从 `mpsc::Receiver<AgentEvent>` 收事件，`app_handle.emit("agent-event", {session_id, event})` 推给前端。`AgentEvent` 加 `Serialize`（若尚未）——core 侧只加 derive，不改语义。
- **审批桥**：`TauriApprover: Approver`，`ask(...)` 经 event 把审批请求推给前端、用 `oneshot` 等前端 command `respond_approval(...)` 回填（对齐 tui 的 `ChannelApprover` 模式，`app/mod.rs:176-237`）。
- 前端：单会话 transcript 渲染（消费 `AgentEvent` 各变体，参考 `src/printer.rs:27` 与 `app/turn.rs:284 on_agent_event` 的映射）+ 输入框 + 审批弹窗。

**UI 设计与交互细节（到设计阶段再展开，用 impeccable skill 来做）**：本计划只定后端契约与骨架；具体的视觉与交互留到设计阶段，届时用 **impeccable skill** 设计。要覆盖的交互目标（先记下，实现时再细化优化）：
- **右侧文件预览侧栏**（对齐 Claude Code）：对话过程中创建/修改的文件，在右侧一小块区域点击即可预览。文件改动信息 core 已给足——`AgentEvent::ToolStart{ input }`（edit/write 的路径与内容）、`ToolEnd{ content }`、以及 checkpoint 里的快照——前端据此维护"本会话涉及的文件"列表。
- 该侧栏**支持富文档预览**：不只是文本/代码 diff，还要能预览 **ppt、docx** 等（走 webview 内嵌渲染或转换预览，具体技术路线设计阶段定）。
- **给 plan / 文档加 comment**：在预览区对内容做批注/评论的交互（如对 plan.md 逐段 comment）。
- 这些是"交互体验"层，全部落在前端，不影响后端契约；后端只需保证文件改动事件与路径信息可被前端消费（现有 `AgentEvent` 已满足）。

验收：起 app，对一个项目发消息、看到流式输出、触发一次文件编辑审批并放行；右侧能点开本次改动的文件预览。

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
