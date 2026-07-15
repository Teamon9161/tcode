# Plan Mode 改进计划

> 状态：设计定稿，待实施。实施完成后本文件的结论性内容并入 `plan.md`，本文件删除。

## 背景与现状

当前 `PermissionMode::Plan` 只是一个纯权限门（`permission.rs::decide` 对 mutating 工具直接
`Deny("blocked: plan mode is active…")`）。三个缺口：

1. **模型不知情**：进入 plan 模式没有任何信号进 ledger，模型只能靠一次失败的工具调用
   "撞见"这个事实——违背零猜测原则。
2. **没有退出流程**：方案写完只能散落在普通回复文本里，用户要自己 shift+tab 切出模式，
   没有"审阅 → 批准 → 转执行"的仪式与记录。
3. **没有 plan 产物**：无落盘文件、无专门渲染、无反馈回路。

参照 Claude Code 的成熟形态（只读门 + 模型知情 + `ExitPlanMode` 工具 + 审批选项
"Yes, and auto-accept edits" 等 + plan 落盘 + plan sub-agent），结合 tcode 自身的
三条全局约束（零猜测、append-only 缓存、注册表插拔）设计如下。

## 目标与非目标

**目标**

- plan 模式成为完整的状态机：模型知情 → 只读探索 → `exit_plan` 提交 → 审阅面板 →
  批准即转换模式 / 拒绝带反馈继续规划。
- 审阅支持块级评论（TUI 里做到接近 Claude Code 桌面端的"选段评论"体验）+ `$EDITOR` 直改。
- plan sub-agent 作为内容生产者接入 task 注册表，与 plan 模式松耦合联动。

**非目标**

- 不做字符级选区评论（块级锚点已覆盖 90% 价值）。
- 不把 plan 模式本身搬进 sub-agent（理由见下文"关系与联动"）。
- 不做 plan 的版本管理/多 plan 并行；一次会话一条主线。

---

## 核心设计

### 1. 运行中切模式：staged 提交（顺带修掉"运行中不允许切"）

现状：turn 运行期间 `Session` 被 move 进 agent task（app.rs `self.session.take()`），
前端摸不到 `session.mode`，shift+tab 只能打印 "mode can be changed when idle"。
修法完全镜像 `PendingInput` 的既有模式：

- `Session` 加 `pending_mode`（`Arc<Mutex<Option<PermissionMode>>>` 共享句柄，前端在
  turn 开始时克隆，与 `pending` 同点）。shift+tab / `/mode` 任何时刻都只是**写入
  staging**：从"staged 目标（若有）否则已生效模式"继续 cycle，运行中连按多次只留
  最终目标。
- **提交点 = 批次边界**（`deliver_pending_input` 同一个点）+ turn 结束；空闲时
  staging 立即提交（同一段代码，特例化为即时）。
  - 为什么不用"交互步骤"（用户 prompt / AskUser 回答 / 权限审批后）作提交点：
    交互点是批次边界的子集（prompt = turn 开始；AskUser/审批完成后所在批次的结束
    就是下一个边界），但**auto 模式的长自主运行没有任何交互点**，而"切出 auto 收回
    控制权"恰恰是最急迫的运行中切换用例。批次边界两者都覆盖。
  - 批次内权限判定原子：一个批次的所有调用在同一个模式下判定，不存在半批旧模式
    半批新模式。
  - 正在流式输出时切换，最坏延迟一个批次才生效——**这是故意的**：模式切换不承担
    刹车语义，急停是 esc/ctrl+c 的职责；esc 取消 turn 后 turn 结束边界照样提交
    staged。
- **运行中切到 Plan 的处理**：批次边界提交 + 同一边界注入 plan-enter note（见 §2），
  模型立刻被告知"用户要求转入规划：停止改动，收拢成 plan / 调 exit_plan"；飞行中
  的工具跑完不杀（它们是旧模式下批准的），从下一批起 mutating 调用被拒且模型已
  知情。
- **冲突消解**：`exit_plan` 审批选项自带 set_mode（§3），用户在对话框里的选择晚于
  更早的 staging → 批准时清空 staging。
- **提交要有回执**：agent loop 提交时发 `AgentEvent::ModeChanged(mode)`；TUI 收到后
  更新状态行并 bake 一行灰字 "permission mode → plan"（转录是事实源，模式在哪个
  边界生效要有记录）。staged 未提交期间状态行显示目标模式加待生效标记（如
  `→ plan`），避免用户误以为 plan 门已生效而实际当前批次仍可改动。
- `ModelState` 持久化保持在按键时写（现行为不变；turn 结束必然提交，staged 不会
  悬空）。

### 2. 模式知情：note 注入语义（惰性求值，防止 shift+tab 刷屏）

**原则：按键不产生任何 ledger 写入；note 是在投递点对比状态差得出的，不是模式
切换事件流。**

- `Session` 记 `last_notified_mode: PermissionMode`（初值 = 会话创建时的模式；若创建时
  就是 Plan，开局 note 随第一条 user prompt 注入）。
- 求值点与 §1 的提交点同点：**turn 开始**（用户 prompt append 前）与**批次边界**
  （`deliver_pending_input`，紧跟 staged mode 提交之后）。
- 求值规则：`session.mode` 与 `last_notified_mode` 相比，**只在进入 Plan 时注入**
  完整引导；其他一切切换（含退出 Plan、default ↔ accept-edits ↔ auto）不注入——
  权限差异由 harness 门控消化，模型无需知道。求值后无论是否注入都把
  `last_notified_mode` 同步为当前值。
  - 退出 Plan 不注入的兜底：正常退出路径本来就是 exit_plan 审批（result 里已写明
    新模式）；用户手动切出后模型若仍调 exit_plan，自愈错误（"not in plan mode"）
    一次即纠正，下一条用户 prompt 本身也是自然信号。状态行的模式显示服务于人。
- **由此自然满足"来回切不堆 note"**：staging 只留最终目标（§1），提交本身就把路径
  折叠掉了；两次发言之间转一圈回到原模式 → diff 为零 → 零条 note；切五次最终停在
  Plan → 只注入一条。
- `exit_plan` 批准导致的模式转换：tool result 已写明 "permission mode is now X"，
  直接同步 `last_notified_mode`，不补 note。
- 注入正文放 `prompts/plan-mode-enter.md`（完整引导：只读探索、可委派 plan sub-agent、
  成熟后调 `exit_plan`）。

### 3. `exit_plan` 工具与审批流

**注册**：常驻 `builtin_tools()`（工具列表是缓存前缀的一部分，不可按模式增删）。
新文件 `crates/tcode-tools/src/plan.rs`。

- schema：`{ plan: string (markdown), title?: string }`。描述写明：仅在 plan 模式下
  调用；plan 应是完整可执行的实施方案（阶段、涉及文件、风险），不是探索笔记。
- 非 plan 模式下 `run` 返回自愈错误："not in plan mode; nothing to exit. If you want
  to record a plan, just write it in your reply."
- `permission()` 返回新变体 `PermissionRequest::PlanReview { title }`（plan 正文从
  input 里取，与 ask_user 的"paged dialog 自己读 raw questions"同构）。不复用
  `UserInput`：PlanReview 的审批结果要携带模式转换回执，语义确实不同。

**审批结果携带模式转换（通用机制，不给 agent loop 加 plan 特判）**：

- `Approval` 加字段 `set_mode: Option<PermissionMode>`。
- agent loop 批准分支通用地应用它：设置 `session.mode`、同步 `last_notified_mode`、
  tool result 写 "Plan approved. Permission mode is now {mode}. Proceed with the plan."
- 拒绝分支：`comment`（复用现有 tab-annotation 机制）作为反馈写进 tool result：
  "User wants changes to the plan:\n{feedback}"，模式保持 Plan，模型继续规划。

**审批选项**（TUI 对话框，plain/REPL 降级为行式）：

| 选项 | set_mode | 语义 |
|---|---|---|
| Yes, auto-accept edits | `AcceptEdits` | 批准并自动放行文件编辑 |
| Yes, approve edits manually | `Default` | 批准，逐项审批 |
| Yes, use auto mode | `Auto` | 批准，交给安全分类器（tcode 差异化能力） |
| No, keep planning | 无 | 反馈文本必填，回给模型继续改 |

### 4. plan 落盘

- **ledger 是模型侧唯一事实源**（plan 在 `exit_plan` 的 tool_use input 里）；文件是给
  人的镜像。
- 批准时写 `~/.tcode/projects/<cwd-hash>/plans/<yyyymmdd-HHMMSS>-<slug>.md`（slug 取自
  title 或 plan 首个标题）。放运行时目录、不进用户仓库；想入库的用户自己拷。
- Phase 2 的 `$EDITOR` 流程需要在**审阅时**就有文件：PlanReview 弹出时先写
  scratchpad 临时文件，批准后正式落盘 plans/。
- 暂不进 sweep（纯文本很小）；若将来纳入，与 sessions 同尺（30 天）。

### 5. plan mode 与 plan sub-agent 的关系与联动

**职责划分——一句话：plan mode 是同意机制，plan sub-agent 是内容生产者。**

| | plan mode（主会话） | plan sub-agent（task kind） |
|---|---|---|
| 本质 | 主会话状态机：只读门 + 审批 + 模式转换 | 一次性起草工人：独立 ledger、只读工具集 |
| 谁触发 | 用户（shift+tab / `/mode` / 启动 flag） | 模型（`task(agent='plan')`），任何模式下可用 |
| 产出 | 经用户批准的、有落盘与模式转换后效的 plan | 一段 plan 草稿文本（task result） |
| 权力 | 唯一能结束 Plan 状态、转换模式的地方 | 无任何模式权力 |

**硬边界（防止越权）**：

- `exit_plan` 不进任何 sub-agent 工具集（`TaskTool::sub_tools` 过滤掉它，与 explore
  过滤 mutating 同点）。sub-agent 永远不能替用户批准任何东西。
- 落盘只发生在主会话批准时。sub-agent 的草稿只是文本。

**联动方式（引导，不强制，不自动 spawn）**：

- `prompts/plan-mode-enter.md` 引导主模型："探索量大的规划任务，把探索+起草委派给
  `task(agent='plan')`，你综合草稿与自己的判断后调 `exit_plan` 提交"。小任务主模型
  直接自己读代码写 plan，不必绕 sub-agent。
- 价值：主会话 context 不被探索转储灌满；`[agents.plan]` 可把起草钉到强模型上
  （主会话跑便宜模型），或反之。

**反馈回路**：拒绝反馈永远回到主模型（tool result），由主模型裁量——小改自己改、
大改重派 sub-agent（prompt 里带上一版 plan + 用户逐条评论）。sub-agent 保持一次性
（现有 `task-{kind}-{run}` cache scope），不做可续对话：重派自带独立缓存前缀，成本
可控，复杂度低得多。

**为什么不把 plan 流程整个搬进 sub-agent**（Claude Code 新版 Plan agent 的路线）：
主会话在 plan 模式积累的探索上下文，批准后直接服务执行阶段——同一 ledger、缓存前缀
延续，执行时模型已经"读过代码"。若 plan 在 sub-agent 里做，批准后主会话还要重新
探索一遍，token 与时间双输。tcode 选择：主会话为主线，sub-agent 是可选的省 context
工具。

### 6. 渲染（tcode-tui）

- "只渲染最终 plan"天然成立：研究过程照常走转录（折叠输出默认）；`exit_plan` 的
  input 不往转录直灌 JSON。
- 路由守规矩：`exit_plan` 的 `ToolRenderer` 在 `RenderRegistry::from_tools` 注册
  （全项目唯一按名 match 点）；app.rs 不出现 `if name == "exit_plan"`。
- 主渲染 = PlanReview 审批面板（见 Phase 1/2）；决议后把 **plan 全文（markdown 渲染）
  + 决定记录**（"✓ Plan approved → accept-edits" / "✗ 继续规划 + 反馈"）bake 进转录。
  转录是唯一事实源，live / replay / approval 三路共用 `bake_call_start` /
  `bake_call_result` 一组入口，replay 必须还原出与 live 相同的 plan 块。
- plain/REPL：打印 plan 全文 + 行式选项（1/2/3/n + 反馈文本），`approver.rs` 扩展。

---

## 分阶段实施

### Phase 1 — 核心循环（core + tools + prompts + 最小 TUI）

1. **staged 模式切换**（§1）
   - `Session` 加 `pending_mode` 共享句柄（镜像 `PendingInput`），前端 turn 开始时
     克隆；`Session::commit_pending_mode()` 在 turn 开始、`deliver_pending_input`、
     turn 结束三处调用，提交时发 `AgentEvent::ModeChanged`。
   - TUI `BackTab` 分支改写：任何时刻都写 staging（从 staged-else-committed 值
     cycle），空闲时立即提交；状态行待生效标记（`→ plan`）；收到 `ModeChanged`
     后转正 + bake 灰字记录。`/mode` 命令共用同一入口。
2. **模式知情**（§2）
   - `Session` 加 `last_notified_mode`；抽一个 `Session::take_mode_note() -> Option<String>`
     （对比 + 同步 + 生成文案），在 turn 开始 append user prompt 前与
     `deliver_pending_input`（紧跟 staged 提交后）两处调用。
   - 新增 `prompts/plan-mode-enter.md`，`include_str!` 引入。
3. **`exit_plan` 工具**：`crates/tcode-tools/src/plan.rs` + `builtin_tools()` 一行。
4. **权限与审批**
   - `tool.rs`：`PermissionRequest` 加 `PlanReview { title }` 变体。
   - `permission.rs`：`Approval` 加 `set_mode: Option<PermissionMode>`；
     `PermissionRules::decide` 对 PlanReview 返回 `Ask`（任何模式下都要问人，包括 Auto）。
   - `agent/mod.rs`：批准分支通用应用 `set_mode`（设置模式 + 同步 `last_notified_mode`
     + 清空 staged mode + result 文案）；拒绝分支反馈进 result。
5. **落盘**：批准路径写 `plans/` 目录（core 侧，紧挨 checkpoint/blob 的路径工具）。
6. **TUI 最小审批面板**：复用 `approval.rs::Dialog` 骨架——plan 经 `self.md.parse`
   渲染、可滚动，四选项，No 必填反馈；决议后 bake plan 全文 + 决定记录。
7. **plain/REPL 降级**：`approver.rs` 打印全文 + 行式选择。

**Phase 1 测试**

- `agent_loop.rs`（MockProvider 脚本）：
  - plan 模式下 mutating 工具仍被拒（回归）；
  - `exit_plan` → 批准（set_mode=AcceptEdits）→ 断言模式已转换、result 文案、后续
    edit 直接放行；
  - `exit_plan` → 拒绝带反馈 → 断言模式仍是 Plan、反馈在 result 里；
  - 非 plan 模式调 `exit_plan` → 自愈错误；
  - turn 运行中 stage 新模式 → 断言当前批次仍按旧模式判定、下一批次按新模式判定、
    `ModeChanged` 事件在批次边界发出；
  - stage 到 Plan → 断言提交与 plan-enter note 在同一边界、且只注入一条。
- session 单元测试：
  - staging：运行中连按多次只留最终目标；stage 后取消 turn，turn 结束边界提交；
    exit_plan 批准清空 staging；
  - `take_mode_note`：来回切净差为零 → None；多次切换只产一条；退出 Plan 及非 Plan
    边界切换 → None；批准转换后不补 note。
- TUI：replay 与 live 渲染一致（沿 `batch_display_label_matches_the_live_batch_header`
  先例钉住 plan 块 bake）；状态行待生效标记在 `ModeChanged` 后转正。

### Phase 2 — 审阅面板升级（tcode-tui）

1. **块级导航**：PlanReview 面板内 plan 按 md 块（标题/段落/列表项/代码块）切分，
   j/k 或方向键移动焦点块，鼠标点击定位（鼠标基础设施已有）。
2. **块锚点评论**：焦点块上按 `c` 弹出评论输入；提交后块旁角标 `¹²³` + 块下着色
   评论行；可攒多条，可删改。
3. **批量反馈组装**：选 "Send feedback"（或带评论时的 No）→ harness 组装
   `> 引用块原文\n评论` × N 为一条反馈进 tool result——与 Claude Code 桌面端
   "选段评论后一起发送"等价，锚点精确到块。
4. **`$EDITOR` 直改**：面板内按 `e` → suspend TUI → 打开审阅临时文件 → 返回 diff：
   - 无改动 → 回到面板；
   - 有改动 → 两个去向让用户选："以修订版批准"（修订版全文作为批准 result 的一部分
     回给模型 + 落盘修订版）或 "作为反馈发回"（diff 作为反馈，继续规划）。
   - 修订全文/反馈**直接进 ledger，不过 blob 门**：plan 是模型后续执行的依据，必须
     完整在上下文里；超长恰恰说明任务复杂，该付的 token 得付。
5. 长 plan 遵守渲染纪律：wrap 缓存按宽度失效、只渲染可见切片。

**Phase 2 测试**：块切分单元测试（md → 块边界）；评论组装文案快照；`$EDITOR` 流程
用注入的 fake editor 命令测 diff 分支。

### Phase 3 — plan sub-agent 与收尾

1. **plan task kind**：`TASK_AGENT_KINDS` 加 `"plan"`、`MODEL_ROLES` 加 `"plan"`、
   新增 `prompts/task-plan-system.md`（架构师人设：读代码 → 权衡 → 产出分阶段实施
   方案，明确"你的产出是草稿，无权批准"）；工具集 = 只读（同 explore 过滤）**且排除
   `exit_plan`**；`[agents.plan]` 钉模型，`/agents` 选择器自动获得。
2. **`/plan` 斜杠命令**（core `commands/plan.rs`）：无参 = 直接进入 plan 模式（比
   shift+tab 循环友好）；`/plan last` = 打开最近一次落盘 plan。
3. `prompts/plan-mode-enter.md` 补委派引导（Phase 1 先写占位一句，此时充实）。

**Phase 3 测试**：task 工具 dispatch plan kind、工具集断言（无 mutating、无
exit_plan）；`/plan` 命令进模式 + note 注入联动。

---

## 已决事项（可推翻，推翻请改本节）

- 工具名 `exit_plan`（与 Claude Code 心智一致，优于 `propose_plan`）。
- "Yes, use auto mode" 上桌（tcode 差异化）。
- plan 落盘在运行时目录，不在仓库内。
- plan 入参是 markdown 字符串，不做结构化 schema（块切分由 md parser 在渲染侧做）。
- plan sub-agent 一次性、不可续；主会话是规划主线。
- 运行中允许切模式：staged 提交，批次边界生效（§1）；模式切换不承担刹车语义。
- 退出 Plan 不注入 note：exit_plan 自愈错误 + 用户 prompt 本身兜底，状态行显示服务
  于人（§2）。
- plan 全文与 `$EDITOR` 修订版不过 blob 门，完整进 ledger（超长说明任务复杂，
  token 该付）。

## 未决事项

（暂无）
