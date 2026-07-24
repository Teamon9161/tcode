# tcode 桌面 app —— 设计与实施计划

Tauri 桌面前端：Rust core 作为库链接进后端，webview 跑 web UI，`AgentEvent`（全数据、可序列化）经 Tauri emit 到前端。目标是终端做不到的两件事——**并行管理多个项目/会话**，和**摆脱终端后更好的展示与交互**。

硬规则见 `crates/tcode-app/AGENTS.md`；视觉与产品判断见同目录的 `DESIGN.md` / `PRODUCT.md`。

## 已落地

端到端能用：起 app → 启动台选项目/会话 → 发消息 → 流式输出 → 文件编辑审批 → 放行写盘。多会话并行、事件按 `session_id` 隔离不串。

- **共享装配层 `tcode-frontend`**（不依赖任何 UI）：`boot`/`open_session`/`build_agent`，以及**已经备好但 app 还没用的** `build_menu`/`build_preset_menu`/`build_provider_setup` + `SwitchFn`/`PinFn`/`ApplyPresetFn` 和 provider setup 状态机（UI 无关的 `setup::Key`）。这些是下面"待做"直接复用的料。
- **后端**：`bridge.rs`（事件/审批桥，一切写在 `Emit` trait 上，无窗口可测）、`state.rs`（`Supervisor` + 每会话 `SessionHandle`，一会话一 turn 靠所有权保证）、`projects.rs`（从 session log 首行 `Meta{cwd}` 还原项目清单）、`SessionFactory`（开新文件夹按其项目级 config 重载）。11 个测试，不打真 API。
- **设计阶段（impeccable）**：token 层 + 可整套替换的主题包（porcelain 亮色默认）、几何标记（中心菱形即状态灯）、启动台、工作区（会话栏 + 对话 + 文件侧栏）、审批 diff、`npm run preview:ui` 设计预览。

**已知限制**：多文件夹会话共用一条 `ShellFilters` 链（输出裁剪会串项目，不涉权限边界）。细节见 AGENTS.md。

---

## 核心洞察：事件契约远比 webview 现在消费的丰富

`AgentEvent` 已经 emit 一整套东西，`transcript.ts` 只认了 10 种，其余全丢进 default 分支。**"摆脱终端后能做得更好"的机会几乎都在这批未消费的事件里**——终端只能把它们压成一行文字或干脆不显示，webview 能给每一种一个称手的控件。

| 已 emit 但前端没用 | 终端的做法 | webview 能做的 |
|---|---|---|
| `TaskRunStarted/Event/Finished`、`DelegatedUsage` | 交错进正文的文字 | 每个 sub-agent 一条可展开的并行泳道，带自己的迷你对话流 |
| `Usage`/`RateLimits` + `estimate_context_tokens` | 一个数字 | 上下文占用量表，逼近 auto-compact 阈值转琥珀；成本累计 |
| `ModeChanged` + `PermissionMode::cycle` | 循环键 + 猜当前是什么 | 分段控件，每个模式一句话说明；staged→committed 有可见的 pending 态 |
| `Compacting`/`Compacted` | "compacting…" | 进度 + 压缩后 summary 可读可回看 |
| `ToolBatchStart` | 五个相同的头 | 一个头收起并发调用 |
| `QueuedInput` | 排队时看不见 | 跑turn时排的消息显示成"待发"气泡 |
| `ReferencesExpanded`（`@path`） | 只留标记 | 附带上下文 chip，可点开看快照 |
| `AutoClassifierUnavailable`/`AutoModePaused` | 一行 note | Auto Mode 健康状态条，说明为何回退 |
| `UserNote`、`StepLimitReached` | 文字 | 带"继续"按钮的行内提示 |

---

## 待做（按优先级）

### 1. 会话控制条 —— 模式 / 模型 / 用量

工作区顶栏现在只有路径和文件面板开关。它该承载**这个会话此刻怎么跑**的三件事。全部 per-session（每个 `SessionHandle` 各自持有），共享层的数据/闭包已就绪。

**a. 审批模式切换**（`PermissionMode`：plan / default / accept-edits / auto / unsafe）
- 后端：`SessionHandle` 暴露 stage/commit（core 已有"安全边界提交"语义），新 command `set_mode(session, mode)`；`ModeChanged` 已会 emit。
- 前端：分段控件或下拉，**不是终端的循环键**——每个模式并列展示，各带一句话（"plan：只读""auto：分类器把关，其余不问"）。切换在 turn 中途按下时显示 pending 标记，等 `ModeChanged` 到达再落定。危险模式（unsafe）要有视觉重量，不能和 default 长一样。
- 复用：`permission.rs` 的 `cycle()`/label 已在；只差 command 和控件。

**b. 模型 / preset 切换**
- 后端：`build_menu`/`build_preset_menu` 直接给出 `ModelMenu`/`PresetMenu`（含 `switch`/`apply` 闭包）。包成 command：`model_menu()` 返回数据，`switch_model(...)`/`apply_preset(...)` 调闭包。切 preset 会整套换主/子模型编排并清临时 pick——这套语义 core 已实现，app 只转发。
- 前端：下拉显示当前模型 + effort，点开选。**摆脱终端的增量**：把 sub-agent 各角色的模型钉法（`[agents.*]`）一眼列出，preset 切换预览会改哪些角色；effort 档位用滑杆而非文字。
- 归属：这是"操纵前端专属菜单对象"，按 CLAUDE.md 的归属规则留前端，但数据源在 `tcode-frontend`。

**c. 上下文 / 用量计量**
- 后端：`Agent::estimate_context_tokens(session)` 给占用量；`Usage`/`RateLimits`/`DelegatedUsage` 事件给每步与累计。加一个轻量 command 或随 `TurnFinished` 带出估算。
- 前端：一个量表——上下文窗口用了百分之多少，auto-compact 阈值画一道线，逼近转琥珀；旁边成本累计。sub-agent 的 `DelegatedUsage` 记进成本但**不**记进上下文表（core 已区分，前端别混）。

### 2. Sub-agent 展示与交互

`task` 工具 spawn 的每个 sub-agent，其整条事件流已经以 `TaskRunEvent{ run, event: Box<AgentEvent> }` 嵌套流出，配 `TaskRunStarted`（kind/model/prompt/summary）与 `TaskRunFinished`（status/tool_calls/usage）。webview 现在**全丢弃**。

设计：
- **一个 sub-agent run = 对话流里一条可展开块**，比工具卡更重。收起显示 kind·model·summary·状态·用量；展开是它自己的迷你对话流（把嵌套 `event` 喂给同一个 transcript reducer，天然递归）。
- **并行 run 用并排泳道**，不交错进正文——这正是终端做不到、而并行是这个 app 立身之本的地方。多个 run 同时在跑时，每条泳道各自有 running 脉冲。
- **它碰的文件汇进同一个文件侧栏**，按 `run` 打标签，这样"是主 agent 还是某个子任务改了这个文件"一眼可分（`ToolStart` 的 `call_id` 与 run 有关联，core 已给）。
- 交互（后续）：点运行中的 run 可聚焦看它的完整流；失败的 run 直接看到它的 status 和最后输出。
- reducer 层要先扩：`transcript.ts` 现在是平的，要加 `run` 维度的分组，`files.ts` 的路径提取要能穿透嵌套事件。

### 3. 命令面板（⌘K）替代斜杠命令

`CommandRegistry::builtin().dispatch()` 给了 /compact /cost /resume /clear /export /note /memory /mode 等，语义作用于 `Session`/`Ledger`/FS 的那些 TUI 与 REPL 已共享。webview 一个都没接。

设计：**⌘K 命令面板**而非在输入框打斜杠——每条命令带一句说明，可搜索。其中 /mode /model 上升为控制条的控件（见 1），/compact /export /resume /clear 留面板。后端加一个 `run_command(session, line)` 薄 command 转 `dispatch`，输出走已有事件通道。这条一次性把一大批终端命令搬进桌面，投入产出比最高。

### 4. Provider setup（首启没配时）

现在 `boot()` 没配 provider 就报错停。setup 状态机（`tcode-frontend::setup`，UI 无关的 `setup::Key`）与 `build_provider_setup` 已备好。
设计：首启检测到无 config，不是画错误屏，而是在 webview 里走 provider 向导（选 provider → 填 key/登录 → 写 config）。`/login` 的 `CodexLogin`/`LoginUpdate` 契约也在里面。这让桌面 app 能独立完成冷启动，不必"先去终端跑一次 tcode"。

### 5. 并行会话的桌面级通知

终端做不到、桌面能做、且并行会话最需要的一环：**某个后台会话卡在审批、或 turn 结束时，发 OS 通知**。现在 app 里已有"从启动台被审批拉过去"的逻辑，但你在别的窗口时完全无感。Tauri 通知插件 + 每会话状态变迁触发。这是"并行管理多任务"承诺的最后一块。

### 6. 富展示细节（摆脱终端的边角）

按性价比排，逐个补：
- **Markdown 补全**：现在只认代码围栏，补列表/表格/标题/链接（仍然只构 React 节点，绝不 `innerHTML`——模型输出是数据）。
- **文件侧栏真 diff**：edit 结果做成带行号的语法高亮 diff，而非纯文本。
- **`QueuedInput`**：跑 turn 时排的消息显示成"待发"气泡（core 已在安全边界投递，前端只差渲染）。
- **`ReferencesExpanded`**：`@path` 展开成可点的上下文 chip。
- **`Compacting`**：压缩进行时给进度，`Compacted` 后 summary 可展开回看。
- **`ToolBatchStart`**：并发调用收进一个头。
- **Auto Mode 健康**：`AutoClassifierUnavailable`/`AutoModePaused` 变成一条状态提示，说明为何回退到人工审批。
- **图片/附件**：`ViewImageTool` 与粘贴的图，webview 可内联显示（终端只能给路径）。

### 7. 富文档预览与 plan 批注（原计划遗留）

- 文件侧栏支持 **ppt/docx** 等富文档预览（webview 内嵌渲染或转换预览，技术路线待定）。
- 对 plan/文档**逐段批注**的交互。
两者都纯前端，不动后端契约。优先级最低，独立性最强，随时可插。

---

## 复用清单（不要重造）

- 驱动会话：`Agent::user_turn`、`compact_with_focus`、`estimate_context_tokens`。
- 事件/审批契约：`AgentEvent`（信封形状由 `event_wire_tests` 钉住）、`Approver`、`PendingInput`/`PendingMode`。
- 模型/preset/provider：`tcode-frontend` 的 `build_menu`/`build_preset_menu`/`build_provider_setup` + 各 `*Fn` + setup 状态机。**别在 app 里重写装配。**
- 模式：`PermissionMode::cycle()` 与 label。
- 斜杠命令：`CommandRegistry::builtin().dispatch()`。
- 持久化：`SessionStore::{list,resume,create}`、`CheckpointStore`。
- 参考实现：`src/printer.rs`（事件→渲染最小映射）、`crates/tcode-tui/src/app/turn.rs`（spawn/own-Session/drain 完整版）、`crates/tcode-tui/src/overlay.rs`（模型/provider overlay 怎么消费共享菜单）。

## 验证

- 后端：`crates/tcode-app/tests/` 用 MockProvider 脚本化 tool_use，断言事件桥输出与并发隔离，不打真 API。新加 command 各配一个往返测试。
- 前端：`npm run build`（tsc 严格）+ `npm run preview:ui` 逐场景肉眼核对。新界面（控制条、sub-agent 泳道、命令面板）各加一个 preview 场景。
- 端到端：起 app，两个项目并排各发任务、各触发审批，确认流式、隔离与桌面通知。
