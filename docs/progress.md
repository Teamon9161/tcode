# Progress：把"规划"内化成标准工作流

状态：设计与实施计划，未开工。落地后把结论段并入 `plan.md`，本文可删。

## 1. 为什么动

今天有两套讲同一件事的机制：

- `update_progress`（`crates/tcode-tools/src/interaction.rs:12`）——会话内可见的多阶段执行状态，纯内存，关窗即失。
- plan mode + `exit_plan` + `PlanDraft`（`crates/tcode-core/src/plan_draft.rs`）——把计划落成 `plans/<ts>-<slug>.md`，经审批后切权限模式。

两者的内容是同一种东西（一份有序的工作分解），差别只在**耐久性**与**有没有人点头**。

真正促成这次改动的是 plan mode 的一个结构性错误：**它占用了 `PermissionMode` 那一格，但"我在规划"和"我愿意承担多大风险"是正交的两件事。** 后果是规划期无法选 auto / unsafe——探索阶段每个 shell 都要手点，而探索恰恰是最该放开的阶段。而 `plan.md:121` 已经承认 plan mode 在 `decide()` 里没有自己的臂，和 default 走同一段代码：**它在权限表里本来就没有语义，却霸占着权限表的一格。**

所以结论不是"删掉规划"，而是：规划是一种工作方式，把它内化进工具与文件的生命周期，从权限模式那一格里搬出来。

## 2. 目标状态

一个概念：**Progress**。一个对象、一个文件、三个状态。

```
draft ──审批──▶ active ──全部完成──▶ done(归档)
  ▲               │
  └───退回修改────┘
```

- **简单任务**：模型觉得该分阶段，就直接建 `active` 边做边记，做完标 `done`。没有人点头这一步。不需要阶段的任务根本不建文件。
- **复杂任务**：用户 `/plan`，模型产出 `draft`，人审阅后点头，在审批对话框里**同时选定执行姿态**（default / accept-edits / auto / unsafe）、是否清空上下文、是否换模型。
- 一个 session 生命周期内可以顺次做好几个 progress；任一时刻只有一个是 **current**。
- `PermissionMode::Plan` 删除。规划期的权限姿态由用户自由选择，与规划无关。
- "先别动手"这个约束不再来自 mode 位，而来自**文件处于 `draft`** 这一事实——每回合都成立，且是 harness 已知的事实（零猜测原则）。

## 3. 可直接复用的既有设施

改动比看上去小，下面这些不用重写：

| 已有 | 位置 | 用途 |
| --- | --- | --- |
| 审批时选执行模式 | `crates/tcode-tui/src/approval.rs` `PLAN_OPTIONS` | 已有 default/accept-edits/auto，补一条 unsafe 即可 |
| 全新会话执行计划 | `PlanOption::fresh_session` → `start_fresh_plan_execution`（`app/turn.rs:254`） | "清空上下文再开始" |
| 执行前换模型 | `choose_plan_execution_model` / `StartPlanExecution{index, effort}` | 计划归计划模型、实现归实现模型 |
| 计划评审面板（逐块评论、`$EDITOR` 改写、diff 回传） | `approval.rs` `PlanReview` | 原样保留，只换数据来源 |
| 非委派语义 | `sub_tools` 按 `PermissionRequest::PlanReview` 过滤（`plan.md:132`） | 新工具沿用同一请求类型，自动继承 |
| 计划目录 | `store::plans_dir` | 改名为 `progress_dir`，懒迁移 |
| 任意 summary 的压缩 | `Ledger::compact(summary, upto)`（`ledger.rs:288`） | "清空上下文"的合法实现路径 |

## 4. 文件格式

位置：`~/.tcode/projects/<project-id>/progress/`（即今天的 `plans/`，改名 + 懒迁移）。
**不放仓库**：agent 往仓库写文件就是 git 噪音和误提交。要放仓库时用 `/plan export <path>` 显式导出。

命名沿用今天的 `<timestamp>-<slug>.md`（时间戳保证排序与不撞名），但**一切面向人和模型的展示都用 `title`，不用文件名**。

```markdown
---
title: 重写 ledger rewind 路径
state: draft            # draft | active | done
created: 2026-07-29T10:12:00Z
---

## [x] 1. 勘查 resume 与 truncate_tail 的调用面
只读。确认 rewind 之后 aux 事件的重放顺序。

## [>] 2. 改 truncate_tail 的归档语义
动 crates/tcode-core/src/ledger.rs:288 附近。
风险：compact 之后再 rewind 会跨越 Summary 边界。

### [ ] 2.1 先补一个跨 Summary 的回归测试
### [ ] 2.2 再改实现

## [ ] 3. 迁移调用方
```

**两级封顶**（`##` 阶段 / `###` 子阶段）。超过两级说明这份计划该拆成两个 progress——这是硬约束，否则模型会生成树状怪物。

`[ ]` / `[>]` / `[x]` 选它是因为三方都认：人一眼能读、解析器好写、**用户能直接手打**。

标题下的正文是描述（为什么、动哪些文件、风险）。**这段正文是全局最值钱的部分，也是最不该常驻上下文的部分**——见第 6 节。

## 5. 工具

`update_progress` 就地演化成 `progress`，注册表里仍是一行（守住"能力靠注册表插拔"）。

```jsonc
{
  "title": "重写 ledger rewind 路径",   // 首次创建时必填，之后可省
  "state": "draft" | "active" | "done", // 省略则保持不变
  "phases": [                            // 整份重发，幂等，不做 diff
    { "phase": "勘查 resume 与 truncate_tail 的调用面",
      "status": "completed",
      "detail": "只读。确认 rewind 之后 aux 事件的重放顺序。",
      "phases": [ /* 至多再嵌一层 */ ] }
  ]
}
```

三条硬规则：

1. **工具是文件的唯一写入者。** 模型永远不用 `edit` 改这个 md。理由：edit 要 read 回来 + 精确匹配 old_string，翻一个阶段的成本从"一次 no-op 调用"涨到"整段文本进两遍上下文"；而且用户一旦手改过文件，模型记的 old_string 必然对不上，edit 必然失败。
2. **返回值携带刚进入阶段的 `detail`。** 今天返回一句 `"progress updated"` 是浪费掉的信道。让它回显新翻成 `[>]` 的那个阶段的正文——于是一份 12 阶段的详细计划，任何时刻只有一个阶段的细节在上下文里，跨 compact、跨会话都不会丢，需要时 harness 精确递过来。
3. **`state: "active"` 的转换请求 `PermissionRequest::PlanReview`**，且仅当当前是 `draft` 时才真正弹审批（模型自建的 `active` 不弹）。沿用这个请求类型即自动继承"不发给任何子 agent"。

`exit_plan` 整个删除，连同 `plan_draft::PLAN_PATH_FIELD` 那套"内部字段夹带进 tool input 再做越权校验"的机制——文件路径由 Session 持有，模型看不见也不需要看见。

子 agent 依旧拿不到这个工具（`crates/tcode-tui/src/app/harness.rs:226` 现在就是这么装配的，保持）。

## 6. 上下文注入策略

**这一节是全局最关键的约束，写错了整个设计就变成一个又贵又啰嗦的 markdown 编辑循环。**

### (a) 只在三个时刻注入

会话内模型自己发过的 `progress` 调用就在 ledger 里，那本身就是记录，重发是纯浪费。真正需要注入的只有那份"没了或过期了"的场合：

1. 新会话开局 / `resume`
2. `compact` 之后
3. 检测到用户手改了文件之后

### (b) 注入摘要，不注入全文

```
<tcode-progress file="rewrite-ledger-rewind.md" state="active">
[x] 1. 勘查 resume 与 truncate_tail 的调用面
[>] 2. 改 truncate_tail 的归档语义   ← 当前
[ ] 3. 迁移调用方
</tcode-progress>
```

只给标题行与状态，**不给 detail 正文**（正文按第 5 节规则 (2) 按需下发）。

### (c) 缓存纪律

- 开局的 inventory（"最近有哪些未完成的 progress"）是稳定的，放 opening context 没问题。
- **每回合变化的进度状态绝不能进 system prompt 或开局前缀**，只能挂在 user turn 的 tail。违反这条就是每翻一个阶段废一次前缀，且正好发生在上下文最大的时候。

### (d) draft 提醒

只要 current progress 处于 `draft`，每回合 tail 追一句"当前计划尚未批准执行"。这取代了原来 plan mode 的 enter note——区别在于它是从文件状态**导出**的事实，不是一次性注入后就靠模型自觉记住的指令。

## 7. 用户手改文件

工具每次写盘记住内容 hash。下次模型再更新时若 hash 已变 → **不覆盖**，返回自愈错误：

> 用户修改了这份计划，当前内容如下：……请基于它继续。

这是相对今天最大的体验增量：**不同意第 3 阶段，直接打开 md 改掉，而不是回聊天框跟模型 argue。** 也正是这一条让"文件"是真的，而不只是一个渲染目标。

## 8. 跨会话

开局注入最近 ≤3 条未完成的：title、文件名、`2/5 完成`、mtime。

**这是清单，不是指令。** 躺在磁盘上的旧 draft 不是用户的请求，模型看见不能自己接着做——这正是 `CLAUDE.md` 信任边界那一节说的"仓库文件由写仓库的人所写，不一定是正在对话的人"，只不过这次那个"别人"是三天前的自己。恢复必须显式：用户开口，或 `/plan resume <n>`。

`resume` 一个会话时，会话记录里可能已带着 progress 引用——此时仍要**重新读文件并校验 hash**，因为期间用户可能改过。记录里的那份是当初的快照，不是现在的事实。

## 9. 生命周期与清理

- `done` **归档，不删除**。它是这次任务唯一值得回头看的产物；`CLAUDE.md` 自己的清理规则就是"绝不删用户可能还想要的东西"。移入 `progress/done/` 或打 `state: done`，从 inventory 里消失即可。
- 陈旧的 `draft`/`active` 超过 14 天自动归档（参照 scratch 的 `SCRATCH_FOR` 老化写法），否则 inventory 会被半途而废的任务淹没。
- **Progress 文件是外部可变状态，不是历史**；ledger 里只记模型发过的工具调用。所以整套设计与 append-only 不变量不冲突，这句要写进 `plan.md`，免得后来者以为"文件可改"等于"历史可改"。

## 10. 与自动记忆的边界

`~/.tcode/projects/<id>/memory/` 是**耐久的事实**；`progress/` 是**在途的任务状态，注定要消失**。两者相邻但不互相吸收：progress 不写 `MEMORY.md`，memory 不记阶段进度。

## 11. `/plan` 命令

`PermissionMode::Plan` 删除后，`/plan` 成为唯一入口：

| 用法 | 语义 |
| --- | --- |
| `/plan [任务描述]` | 本回合产出 `draft` 而不是代码；不改权限模式 |
| `/plan list` | 列出未完成的 progress |
| `/plan resume <n>` | 显式接手某个 progress，成为 current |
| `/plan last` | 保留今天的行为 |
| `/plan export <path>` | 导出到仓库 |

`crates/tcode-core/src/commands/plan.rs` 已经在 core `commands/` 里，语义作用于 Session/文件系统，位置正确，TUI 与 REPL 共享。

## 12. 实施步骤

每步独立可落地、独立可验证，按顺序做。

### 第 1 步：core 的 Progress 对象

- 新增 `crates/tcode-core/src/progress.rs`，取代 `plan_draft.rs`：文件读写、frontmatter、`[ ]/[>]/[x]` 解析与渲染、hash 记录与 reconcile、`state` 转换。
- `store::plans_dir` → `progress_dir`，懒迁移旧 `plans/`（照 `project_dir_in` 的迁移写法）。
- `Session` 持有 `current: Option<Progress>`。
- 验证：单测覆盖往返渲染/解析、两级封顶、hash 变更返回 reconcile 错误、非法 state 转换。

### 第 2 步：`progress` 工具

- `crates/tcode-tools/src/progress.rs` 取代 `interaction.rs` 里的 `UpdateProgressTool`；`builtin_tools()` 之外的主 agent 装配点跟着改（`app/harness.rs:226`、`app/mod.rs:2045`）。
- 返回值回显新 `[>]` 阶段的 detail。
- 验证：`cargo test -p tcode-tools --test agent_loop` 加一条"翻阶段拿到 detail"的用例。

### 第 3 步：注入

- 三个注入点（开局 / compact 后 / hash 变更后）+ tail 摘要 + draft 提醒。
- **务必确认前缀未被污染**：加一个测试断言两次连续请求的 system prompt 与 opening context 逐字节相同，而进度差异只出现在最后一条 user 消息里。这是本设计里最容易悄悄破掉的不变量。

### 第 4 步：删 plan mode

- `PermissionMode::Plan` 变体、`cycle()` 里的位置、mode picker、`plan-mode-enter.md` / `plan-mode-exit.md`、`take_mode_note` 里的两条臂全部删除。
- `exit_plan` 与 `PLAN_PATH_FIELD` 删除；`PermissionRequest::PlanReview` 保留并改由 `progress` 发出，`tool.rs:133` 的 `approval_label` 跟着改名。
- `PLAN_OPTIONS` 补一条 `Yes, and run unsafe`。
- 验证：`cargo test -p tcode-tui`（审批流程）、`cargo test -p tcode-core`。

### 第 5 步：`/plan` 子命令 + inventory

- `commands/plan.rs` 加 `list` / `resume` / `export`；开局 inventory 注入。
- 验证：resume 后 hash 校验、inventory 不触发自动接手（用一条测试钉住"看见 draft 不等于开始做"）。

### 第 6 步：TUI 两级树

- `live_panel.rs` 的 `ProgressPhase` 支持一层嵌套：当前阶段展开子阶段，其余折叠。`visible_phase_range` 的取窗逻辑跟着改。

### 第 7 步：文档

- `plan.md`：改写 plan mode 相关段落（108/117/119/121/123/130/132/158 行附近），补"progress 文件是外部状态不是历史"。
- `crates/tcode-tools/src/skills/builtin/tcode-config/SKILL.md`：如新增任何可配置项（保留天数、inventory 条数）必须同步。
- 新 prompt 一律 `prompts/*.md` + `include_str!`，不在 `.rs` 里写多行字符串。

## 13. 代价

拿一个**永不失败的 30 行工具**，换一个**有生命周期、有磁盘状态、有 reconcile、有陈旧清理**的对象。新增的失败模式是实打实的：孤儿 draft、hash 冲突、恢复错文件。

换来的是三件今天做不到的事：跨会话续做、用户直接改计划、规划期自由选权限姿态。

守住两条，收益成立；破掉任一条，它就退化成一个又贵又啰嗦的 markdown 编辑循环：

1. **工具是文件的唯一写入者。**
2. **只在那三个时刻注入。**

## 14. 未决

- 一个 session 同时开两个 `active` 是否要允许？当前设计是"可以有多个文件，但只有一个 current"。若实测发现并行任务常见，再考虑 current 栈。
- 归档保留期 14 天是拍的，dogfood 后再定，定下来要进 SKILL.md。
