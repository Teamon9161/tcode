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
| `CohortUpdated`/`CohortChannelMessage` | 完全没有位置 | 成员名册 + 共享频道，是天然的多列视图 |
| `Usage`/`RateLimits` + `estimate_context_tokens` | 一个数字 | 上下文占用量表，逼近 auto-compact 阈值转琥珀；成本累计 |
| `ModeChanged` + `PermissionMode::cycle` | 循环键 + 猜当前是什么 | 分段控件，每个模式一句话说明；staged→committed 有可见的 pending 态 |
| `Compacting`/`Compacted` | "compacting…" | 进度 + 压缩后 summary 可读可回看 |
| `ToolBatchStart` | 五个相同的头 | 一个头收起并发调用 |
| `QueuedInput` | 排队时看不见 | 跑 turn 时排的消息显示成"待发"气泡 |
| `ReferencesExpanded`（`@path`） | 只留标记 | 附带上下文 chip，可点开看快照 |
| `AutoClassifierUnavailable`/`AutoModePaused` | 一行 note | Auto Mode 健康状态条，说明为何回退 |
| `UserNote`、`StepLimitReached` | 文字 | 带"继续"按钮的行内提示 |

---

# 第一部分：渲染模型（本轮新增的核心决策）

## 1.0 决策：富渲染值得做，`innerHTML` 一次都不许有

**问题**：既然摆脱了终端，能不能让模型直接输出 HTML，公式图表一步到位？

**结论：能力要，路径换。** 模型产出的富内容分三层，层与层之间由**执行边界**隔开，不是由纪律隔开。

### 第一层 —— 受控节点（主 realm）

Markdown 结构（段落 / 标题 / 列表 / 表格 / 链接 / 引用 / 行内与围栏代码）解析成 AST，再构造成 **React 节点**。`rich.tsx` 现在就是这么干的，只是只认围栏——把它扩成完整 markdown，规则不变：

- 解析器只产出**受控节点类型的白名单**，未知节点降级为纯文本，绝不透传。
- 链接的 `href` 走白名单协议（`https:` / `file:` / 内部 `tcode:`），其余渲染成不可点的文本。`javascript:` 在主 realm 里就是执行。
- 图片只允许指向本会话已知的 blob / 附件 id，不接受任意 URL。

这一层覆盖 95% 的日常输出，且**零新信任面**。

### 第二层 —— 沙箱 artifact（iframe，无 same-origin）

图表、图示、以及"模型真的想写一段 HTML"——全部走这一层：

```
<iframe sandbox="allow-scripts" src="sandbox.html" />
        ↑ 故意不给 allow-same-origin
```

没有 `allow-same-origin`，iframe 是**不透明来源**。实测（本轮已验证，不是推断）：里面访问 `parent.document`、`parent.__TAURI__`、`localStorage` 全部抛 DOMException，而它自己的脚本在 `default-src 'self'` 下正常加载。里面爱怎么 `innerHTML` 怎么 `innerHTML`，最坏结果是那个 iframe 自己画花了。

**两条实测出来的形态约束**（原计划这里写错过，记下来免得再走一遍）：

1. **不能用 `srcdoc`。** srcdoc frame 继承父页 CSP，叠加不透明源之后 `'self'` 匹配不到任何东西——结果是它**一行脚本都跑不了**。必须是 `src` 指向一个真实文档，那样它才拿自己的 CSP。
2. **里面的脚本必须是经典脚本，不能是 module。** 模块脚本一律走 CORS 模式，不透明源发出的请求带 `Origin: null`，服务端不显式放行就**根本不执行**。对照实验：

   | 沙箱内脚本形态 | 是否执行 |
   |---|---|
   | 经典 `<script src>` | ✅ |
   | `<script type="module">` | ❌ |
   | `<script type="module">` + `Access-Control-Allow-Origin` | ✅ |

   依赖 CORS 等于赌 Tauri 自定义协议会发 ACAO，赌错的表现是**开发机正常、装出来的 app 静默失效**。所以沙箱由 `vite.sandbox.config.ts` 单独构建成 IIFE 落在 `public/`，完全不进主 module graph。

   IIFE 不能 code-split，所以拆成"1.2 KB 的 boot + 按需注入的每类渲染器"：boot 自己处理 HTML artifact，遇到 chart/diagram 才注入对应的经典脚本。单一 bundle 会是 4.6 MB，每个 frame 解析一遍。

具体接法：

| 围栏语言 | 渲染 | 位置 | 默认视图 |
|---|---|---|---|
| ` ```math ` / `$$…$$` / `$…$` | KaTeX | 第一层（见下方例外说明） | — |
| ` ```mermaid ` | mermaid，`securityLevel: 'strict'` | 第二层 | preview |
| ` ```echarts ` / ` ```chart ` | echarts，主题色板从 token 派生 | 第二层 | preview |
| ` ```html ` / ` ```svg ` | 原样投喂沙箱 | 第二层 | **source** |
| ` ```diff ` / ` ```patch ` | 统一 diff 组件 | 第一层 | — |

`html` 默认显示**源码而非渲染**，是唯一一处默认值上的分歧，理由：模型写 ```html 通常是在展示一段标记，不是请求执行它；而这恰恰是"猜错了会真的执行东西"的那一类。两种视图都在，只是不预设。

**关于公式的诚实说明**：KaTeX 的 `renderToString` 返回 HTML 字符串，本质上需要一次 `innerHTML`。两条路，二选一，实现时定：

- **稳妥**：把公式也放进第二层 iframe。代价是每个行内公式一个 iframe，性能与基线对齐都糟——所以行内公式实际不可行，只适合独立公式块。
- **务实**：主 realm 用 KaTeX，但**锁死配置**：`trust: false`（默认，禁 `\href`/`\htmlData`）、`strict: true`、`output: 'html'`，并在 `katex.ts` 里把 options 定义成不可传参的常量——调用方无法放宽。同时依赖现有 CSP（`script-src` 继承 `default-src 'self'`，内联事件处理器被浏览器拒绝执行）作为第二道。

推荐**务实**方案，条件是把它写成硬规则：*KaTeX 是全 app 唯一被允许把字符串变成 DOM 的地方，配置常量不许接受参数；`script-src` 永远不许出现 `unsafe-inline`/`unsafe-eval`*。这条规则值得进 `AGENTS.md`——它正是"能用结构挡的不许退化成 prompt 纪律"，只是这里结构挡不住，所以必须把边界钉死并写明。

### 第三层 —— 禁止

模型输出 → 主 realm 的 `innerHTML` / `dangerouslySetInnerHTML` / `new Function` / `eval`。任何形态、任何"就这一处"。加一条 ESLint 规则和一个 grep 断言进 CI，比记住它可靠。

### 为什么这个分层是对的

它不是安全妥协，而是**把"谁能执行"变成了结构事实**。第二层的存在意味着以后无论模型想画什么——3D 图、动画、一个小 REPL——都不需要重新讨论安全性：扔进沙箱，完事。这是"消除特例"：不再有"这个富内容该不该允许"的逐例判断。

## 1.1 前端注册表之一：围栏渲染器

同构于 core 的三张注册表（`Tool` / `SlashCommand` / `ToolRenderer`），前端也不许在渲染主逻辑里按语言 `if`：

```ts
export type FenceRenderer = {
  languages: string[];
  /** 主 realm 受控节点，或第二层沙箱卡片。二选一，由渲染器自己决定。 */
  render(body: string, ctx: FenceCtx): ReactNode;
};

export const FENCES: FenceRenderer[] = [mermaid, math, vegaLite, diffFence, /* … */];
// 未命中 → 语法高亮代码块（默认，永远存在）
```

新增一种富内容 = 新写一个文件 + 数组里加一行。`rich.tsx` 与 `Transcript.tsx` 不动。

## 1.2 前端注册表之二：工具视图

`Transcript.tsx` 迟早会长出 `if (name === "edit")`。先建注册表挡住它，形状照抄 core 的 `ToolRenderer`：

```ts
export type ToolView = {
  /** 改动预览：edit → diff，write → 内容，read → 无。 */
  body?(input: unknown): ReactNode;
  /** 结果区：多数工具用 preview，read/shell 折叠进 Inspector。 */
  result?(res: ToolResult, input: unknown): ReactNode;
  /** 点这张卡片打开 Inspector 的什么。 */
  inspect?(input: unknown, res?: ToolResult): Inspect | null;
};
```

**关键：`route` 不在前端定义。** core 的 `CallRoute`（`Transcript` / `Progress` / `Silent`）和 `quiet_output` 是从活的 `Tool::batch_policy()` 派生的，前端手抄一份必然漂移。加一个后端 command：

```rust
#[tauri::command] fn tool_views() -> Vec<ToolViewMeta>
// { name, route, quiet_output, hide_success_result }
```

boot 时取一次。前端注册表只负责**怎么画**，路由语义永远来自 core。这样 `update_progress` 走进度面板、`ask_user` 静默，这些判断在 app 里天然与 TUI 一致，改 core 不需要想起 app。

---

# 第二部分：信息架构

## 2.0 三个区，三个问题，互不重叠

你提的东西（项目 / 模式 / 模型 / plan 预览 / 文件预览 / diff / sub-agent / agent_tree / progress）如果全塞右侧栏，右侧栏就变成杂物抽屉，每样东西都要先"切到那个 tab"。**按提问方式分区**才是没有重叠的划法：

| 区域 | 回答的问题 | 常驻性 | 放什么 |
|---|---|---|---|
| **左 rail** | 我有哪些活？ | 常驻 | 打开的会话 + 状态灯；回启动台 |
| **顶栏** | 这个会话此刻怎么跑？ | 常驻，一行 | 模式 / 模型 / 上下文量表 |
| **live panel**（composer 上方） | 它现在在干什么？ | 常驻，可收起成一行 | agent tree + progress |
| **右 Inspector** | 我正在看什么？ | 按需，可拖宽，可关 | **当前选中的那一个东西** |

于是：

- **项目选择** → 启动台 + 左 rail 已经答完了，右栏再来一遍是第三处重复。不做。
- **模式 / 模型 / profile 选择** → 它们是**瞬时决策**，存在两秒就该消失。放常驻侧栏是把一次性动作永久占地。顶栏控件（显示当前值）+ ⌘K（改）。不做侧栏。
- **plan 预览 / 文件预览 / diff / sub-agent 完整流 / artifact** → 这些是**检视对象**，看的时候要大面积。全部进 Inspector。

一句话：左栏是"我的活"，顶栏是"这局的规则"，live panel 是"它在跑什么"，右栏是"我在看什么"。

## 2.1 Inspector = 单值槽，不是 tab 容器

这是本设计里第二个消除特例的地方。Inspector 不持有若干面板，它持有**一个值**：

```ts
export type Inspect =
  | { kind: "file";       path: string; at?: string }      // at = call_id，看那次读到的快照
  | { kind: "diff";       callId: string }                 // 某次 edit/write 的改动
  | { kind: "changes";    since: number }                  // 从某条用户消息至今的聚合改动
  | { kind: "run";        run: string }                    // 某个 sub-agent 的完整流
  | { kind: "artifact";   callId: string; source: string } // 沙箱预览
  | { kind: "doc";        path: string }                   // plan 草案 / md 文档 / 富文档
  | { kind: "output";     callId: string }                 // 工具完整 content
  | { kind: "cohort";     id: string };                    // 名册 + 共享频道
```

`<Inspector value={inspect} />` 按 `kind` 分派，一个 `useState<Inspect | null>`。全部交互点——转录里的路径、文件行、run 卡片、工具卡片、审批弹窗里的"看全文"——都只做一件事：`setInspect(...)`。

由此**免费得到**：

- **导航栈**。`Inspect[]` 一压，前进/后退就有了。你说的"sub-agent 的跳转"就是这个：从 run 跳到它改的文件，再退回来。
- **零 tab 管理**。没有"哪个 tab 是激活的"这种状态。
- **深链**。`tcode://inspect/...` 形态的内部链接可以直接出现在转录文本里（第一层的白名单协议之一），点了就是 `setInspect`。模型说"改动在 `src/main.rs:42`"时，那就是一条真链接。
- **一致的空态**。Inspector 关着时是关着，不是"打开但空白"。

宽度可拖，记 `localStorage`；`Esc` 关闭；`⌘\` 开关。

---

# 第三部分：逐能力设计

## 3.1 转录：块的形状要先扩

`transcript.ts` 现在是**平的** `Block[]`。三个需求同时要求它长出维度：sub-agent 分组、批次收拢、progress 路由。改成：

```ts
export type Block =
  | { kind: "user"; text: string; entryIndex?: number }   // entryIndex 供 rewind / changes-since
  | { kind: "assistant"; text: string }
  | { kind: "thinking"; text: string; collapsed: boolean }
  | { kind: "tool"; callId: string; name: string; summary: string; input: unknown; result?: … }
  | { kind: "batch"; label: string; calls: string[] }     // ToolBatchStart，收拢并发调用
  | { kind: "run"; run: string; meta: TaskRunMeta; blocks: Block[] }  // 递归
  | { kind: "queued"; text: string; attachments: string[] }
  | { kind: "note" | "error"; text: string };
```

`{ kind: "run" }` 内部就是 `Block[]`，`TaskRunEvent{ run, event }` 直接把内层 `event` 喂给同一个 `applyEvent` 递归下去——**sub-agent 展示不需要第二套 reducer**，这是事件契约本来就设计好的形状，白拿。

保持 `applyEvent` 是纯函数：resume 一个会话 = 重放事件，界面自然重建，什么都不用持久化。这条性质现在有，别丢。

## 3.2 diff：一个组件，四处复用

`components/Diff.tsx` 已在。把它做成**唯一的 diff 呈现**，四处调用同一个：

1. **审批弹窗** —— 决定前必须看见全文（core 的 `approval_detail` 语义）。
2. **转录内联** —— edit/write 卡片下方的改动预览，折叠。
3. **Inspector `kind: "diff"`** —— 大面积看，带行号、语法高亮、hunk 折叠、并排/统一切换。
4. **Inspector `kind: "changes"`** —— 聚合多文件。

数据来源全是 `ToolStart.input`（core 明确写了"Raw call input, e.g. for rendering edit diffs in the UI"），不需要新契约。

桌面级增量（终端做不到）：**并排 diff**、**hunk 级折叠**、**改动导航（n/N 跳下一处）**、**同一文件多次编辑叠加成一条时间线**。

## 3.3 历史改动：每条用户消息旁的"从这里到现在"

`CheckpointStore` 在改文件前把原文按**当时的 ledger 长度**存了副本，并提供 `dirty_since(len)` / `restore_to(target_len)`。这意味着"从第 N 条消息到现在，文件变成了什么样"是**可算的**：取所有 `len >= N` 的存档原文，与当前磁盘内容做 diff。

设计：转录里每个 `{kind:"user"}` 块 hover 出两个入口——

- **看改动** → `Inspect{ kind:"changes", since: entryIndex }`，Inspector 里聚合 diff。
- **回到这里** → rewind（`truncate_tail` + `restore_to`），这是 core 已有的合法历史操作之一。桌面上它该带一个明确的确认，并在确认框里直接展示将被回滚的文件清单。

需要的后端 command（新增，都是薄封装）：

```rust
fn changes_since(session: &str, entry_index: usize) -> Vec<FileChange>  // {path, before, after}
fn rewind_to(session: &str, entry_index: usize) -> RewindReport
```

`dirty_since` 决定"看改动"入口是否点亮——没脏过就不显示，避免每条消息都挂一个死按钮。

> 待确认：`CheckpointStore` 的存档是否保留了 `Option<String>`（`None` = 文件当时不存在）以区分"新建"。`load` 的签名 `Vec<(usize, String, Option<String>)>` 暗示是的，实现时核对。

## 3.4 read 的文件：快照而非重读

点转录里 read 卡片 → `Inspect{ kind:"file", path, at: callId }`。**关键判断：显示的是那次读到的内容，不是当前磁盘内容。** 理由是零猜测原则的同一条逻辑——你要判断的是"模型当时看到了什么"，重新读一遍磁盘会掩盖"文件后来被改了"这个事实。

`ToolEnd.content` 已经带了完整的 gated 输出（core 注释：`Complete gated output for UI detail views`），够用，不需要新契约。

顶部给一条对比条：*"读于 3 步前 · 此后被 edit 修改过 2 次 →"*，点击切到 diff。这是终端完全给不了的东西。

从文件侧栏点 → 显示当前内容（因为那里问的是"这个文件现在怎样"）。同一个 `kind:"file"`，`at` 有无区分，不是两个特例。

## 3.5 sub-agent：泳道 + 跳转

- **一个 run = 转录里一条卡片**，比工具卡片重。收起显示 `kind · model · summary · 状态 · 用量 · 工具调用数`（全部来自 `TaskRunStarted` / `TaskRunFinished`）。
- **展开 = 就地迷你转录**（递归 `Block[]`，见 3.1）。
- **并行 run 并排泳道**，不交错进正文——这正是终端做不到、而并行是这个 app 立身之本的地方。每条泳道各自 running 脉冲。
- **点标题 → `Inspect{kind:"run"}`**，Inspector 里是这个 run 的完整流、完整用量、它碰过的文件清单。这就是"sub-agent 跳转"：从 run 里的文件继续跳到 diff，导航栈管返回。
- **它碰的文件汇进同一个文件侧栏，按 run 打标签**（`ToolStart.call_id` 与 run 有关联，core 已给），这样"是主 agent 还是某个子任务改了这个文件"一眼可分。
- `files.ts` 的路径提取要能穿透嵌套事件——现在它只看顶层 `ToolStart`，改 reducer 时一起。

**Cohort（`CohortUpdated` / `CohortChannelMessage`）**：TUI 里几乎没有位置，webview 里它天然是多列——成员名册一列，共享频道一列，每个成员的活跃状态一个灯。`Inspect{kind:"cohort"}`。这项优先级低于 task run，但形状是现成的。

## 3.6 agent tree + progress：live panel，不进侧栏

TUI 的 `live_panel.rs` 已经把这件事想清楚了：**它是持续状态，要在余光里，且要紧挨"我要不要打断"这个决策**。所以位置是 **composer 正上方**，不是右侧栏。

- 收起态一行：`● 2 agents · phase 3/5 — 校对导出路径 · 1m42s`。
- 展开：主 agent 一行 + 每个 run 一行（状态灯 / 当前工具 / 用量），行可点 → `Inspect{kind:"run"}`。
- **progress 来自 `update_progress` 的 `input.phases`**（`{phase, status}`，status ∈ pending/in_progress/completed），路由由 `tool_views()` 给的 `route: Progress` 决定，**不进转录**——与 TUI 语义一致，且这个一致性是从 core 派生的而非手抄的。
- 兼容旧会话的 `plan`/`step` 字段名，照 TUI 的做法。
- 空 `phases` 数组 = 清空进度显示（工具描述明确要求支持）。

桌面增量：phase 之间用连接线画成真的时间轴；已完成的 phase 折叠但保留耗时；in_progress 的 phase 下面挂它正在跑的工具调用。

## 3.7 plan 预览

`plan_draft.rs`：plan 是**写进真实文件**的，`plan_path_in(input, cwd)` 能从工具 input 里取出路径，被拒的下一版覆盖同一个文件。

设计：plan 审批到来时，弹窗不是塞一大段文字，而是——

- 弹窗保持紧凑（决定的地方要小）；
- Inspector 自动打开 `Inspect{kind:"doc", path}`，以完整 markdown 渲染（第一层受控节点，含表格/列表/代码）。

**逐段批注**（你提到的）在这里落地：Inspector 的 doc 视图给每个块一个 hover 批注钮，收集的批注在"拒绝并附意见"时拼成结构化反馈。这是终端根本做不到的交互，且**不动任何后端契约**——批注最终只是审批 answer 里的 comment 文本。

## 3.8 ask_user：`preview` 就是给这个 app 准备的

`ask_user` 的 option 带 `preview` 字段，描述里明说"shown in a panel beside the options and re-rendered as the user moves between them"。TUI 只能给一小块。桌面上：选项列在弹窗，**preview 渲染进 Inspector**，键盘上下移动即时切换。这是现有契约里已经写好、但只有桌面能兑现的设计。

## 3.9 顶栏控制条

**a. 审批模式**（plan / default / accept-edits / auto / unsafe）
分段控件，每个模式并列 + 一句话说明（"plan：只读""auto：分类器把关，其余不问"）。**不是终端的循环键。** turn 中途切换显示 pending 标记，等 `ModeChanged` 到达再落定（core 的 staged→committed 语义）。unsafe 要有视觉重量。后端加 `set_mode(session, mode)`。

**b. 模型 / preset**
`build_menu` / `build_preset_menu` 已经给出带闭包的菜单数据，包成 `model_menu()` + `switch_model()` / `apply_preset()` 即可。桌面增量：把 `[agents.*]` 各角色的钉法一眼列出；preset 切换**预览会改哪些角色**；effort 用滑杆。

**c. 上下文量表**
`estimate_context_tokens` 给占用，auto-compact 阈值画一道线，逼近转琥珀。**`DelegatedUsage` 记进成本但不记进上下文表**——core 已经区分了，前端别混。

## 3.10 ⌘K 命令面板

`CommandRegistry::builtin().dispatch()` 已有 /compact /cost /resume /clear /export /note /memory /mode 等。后端加一个薄 command `run_command(session, line)` 转 dispatch，输出走已有事件通道，**一次性把一大批终端命令搬进桌面**，投入产出比最高的一条。

/mode /model 上升为顶栏控件（3.9），其余留面板。面板里每条命令带一句说明、可搜索。`hidden()` 的命令（如 `/dogfood`）照 core 语义不进面板。

## 3.11 其余富展示

- **`QueuedInput`** → "待发"气泡，`entry_index` 让它和普通 prompt 一样可 rewind。
- **`ReferencesExpanded`** → `@path` 变成可点 chip，点开 Inspector 看快照；`added_tokens` 计进量表。
- **`Compacting`/`Compacted`** → 进度 + summary 可展开回看（core 特意把文本带在事件里，就是为了让人能读它站在什么之上）。
- **`ToolBatchStart`** → 一个头收起并发调用（`{kind:"batch"}`）。
- **Auto Mode 健康** → `AutoClassifierUnavailable`/`AutoModePaused` 变状态条，说明为何回退到人工审批。
- **图片/附件** → `ViewImageTool` 与粘贴的图内联显示（走 blob id，不是任意 URL，见 1.0 第一层）。
- **富文档（ppt/docx）预览** → `Inspect{kind:"doc"}` 的一个后端分支，转换渲染，技术路线待定。优先级最低。

## 3.12 Provider setup 与桌面通知

- **首启向导**：现在 `boot()` 没配 provider 就报错停。`tcode-frontend::setup` 状态机（UI 无关的 `setup::Key`）已备好，webview 里走向导（选 provider → 填 key/登录 → 写 config），`/login` 的 `CodexLogin`/`LoginUpdate` 契约也在里面。让桌面能独立冷启动。
- **OS 通知**：某个后台会话卡在审批、或 turn 结束时发系统通知。这是"并行管理多任务"承诺的最后一块——你在别的窗口时现在完全无感。Tauri 通知插件 + 每会话状态变迁触发，**记得先改 `capabilities/default.json`**（硬规则 6）。

---

# 第四部分：后端需要新增的契约

全部是薄封装，逻辑留在 core / `tcode-frontend`：

| command | 用途 | 依赖的已有能力 |
|---|---|---|
| `tool_views()` | 工具路由语义表 | `Tool::batch_policy()` |
| `set_mode(session, mode)` | 审批模式 | `PermissionMode`，`ModeChanged` 已 emit |
| `model_menu()` / `switch_model()` / `apply_preset()` | 模型与 preset | `build_menu` / `build_preset_menu` |
| `run_command(session, line)` | ⌘K | `CommandRegistry::dispatch` |
| `context_usage(session)` | 上下文量表 | `estimate_context_tokens` |
| `changes_since(session, i)` | 历史 diff | `CheckpointStore` |
| `rewind_to(session, i)` | 回到某点 | `truncate_tail` + `restore_to` |
| `read_doc(path)` | plan / 文档预览 | 受 cwd scope 约束 |

**约束**：每个新 command 都要能在没有窗口时被测试驱动（AGENTS.md 硬规则 2），逻辑写在 `Emit` / 纯函数上。`read_doc` 尤其要走 `cwd_scope`——不能因为是"预览"就绕过路径边界。

---

## 实施顺序

按"解锁后续的程度"排，不是按大小：

1. ~~**渲染管线与两张注册表**（1.1 / 1.2 + `tool_views`）~~ ✅ 已落地
2. ~~**Inspector 单值槽 + 导航栈**（2.1）~~ ✅ 已落地（根视图是文件索引）
3. ~~**transcript reducer 扩维**（3.1）~~ ✅ 已落地（`blocks.ts`，run/batch 递归）
4. ~~**diff 四处复用 + read 快照**（3.2 / 3.4）~~ ✅ 已落地
5. **live panel（agent tree + progress）**（3.6）与 **sub-agent 泳道**（3.5）。progress 现在暂时以行内相位表出现在转录里（`toolViews.tsx` 的 `Phases`），面板建好后把它挪过去。
6. **顶栏控制条 + ⌘K**（3.9 / 3.10）。
7. **历史改动与 rewind**（3.3）、**plan 批注**（3.7）、**通知与首启向导**（3.12）。
8. 富文档预览、cohort 视图。

### 1–4 落地后的遗留

- **`route` 仍是名字表**。`quiet_output` 真从 `Tool::batch_policy()` 派生，但 `CallRoute` 住在 `tcode-tui`，app 不能依赖它，所以 progress/silent 目前是 `commands.rs` 里的常量数组。终局是把 `route()` 提升为 core 的 `Tool` trait 方法，TUI 与 app 都读它——这正是 CLAUDE.md 说的"该由 trait 方法表达的能力"。做这一步要同时动 core / tools / tui。
- **edit diff 没有绝对行号**。edit 工具匹配的是片段，调用里没有它在文件中的位置；宁可不显示也不显示一个看着可信的错行号。真要有，得把片段在 read 快照里定位——那属于 3.4 的延伸。
- **图片仍是占位 chip**。webview 没有 asset protocol，`<img>` 一律加载失败，所以引用按引用显示。要真显示得先加资产协议。
- **`sandbox-mermaid.js` 3.4 MB**。只在真出现 mermaid 围栏时注入，但单个文件仍偏大；mermaid 支持按图类型裁剪，需要时再说。

---

## 复用清单（不要重造）

- 驱动会话：`Agent::user_turn`、`compact_with_focus`、`estimate_context_tokens`。
- 事件/审批契约：`AgentEvent`（信封形状由 `event_wire_tests` 钉住）、`Approver`、`PendingInput`/`PendingMode`。
- 模型/preset/provider：`tcode-frontend` 的 `build_menu`/`build_preset_menu`/`build_provider_setup` + 各 `*Fn` + setup 状态机。**别在 app 里重写装配。**
- 模式：`PermissionMode::cycle()` 与 label。
- 斜杠命令：`CommandRegistry::builtin().dispatch()`。
- 持久化与历史：`SessionStore::{list,resume,create}`、`CheckpointStore::{save,dirty_since,restore_to}`、`PlanDraft`。
- 工具路由语义：core 的 `Tool::batch_policy()`；渲染分工参考 `crates/tcode-tui/src/render.rs` 的 `ToolRenderer` / `CallRoute`。
- 参考实现：`src/printer.rs`（事件→渲染最小映射）、`crates/tcode-tui/src/app/turn.rs`（spawn/own-Session/drain 完整版）、`crates/tcode-tui/src/live_panel.rs`（agent tree + progress 的既有模型）、`crates/tcode-tui/src/overlay.rs`（模型/provider overlay 怎么消费共享菜单）。

## 验证

- **后端**：`crates/tcode-app/tests/` 用 MockProvider 脚本化 tool_use，断言事件桥输出与并发隔离，不打真 API。新加 command 各配一个往返测试。
- **渲染安全（新增，必须有）**：
  - CI grep 断言：`ui/src` 下不存在 `dangerouslySetInnerHTML` / `innerHTML` / `eval` / `new Function`，唯一豁免是 `katex.ts`（若选务实方案）。
  - 单测：恶意 markdown（`javascript:` 链接、`<img onerror>`、`<script>`、外部图片 URL）经 `rich` 后**只产出文本节点**。
  - 单测：artifact iframe 的 `sandbox` 属性不含 `allow-same-origin`。
- **前端**：`npm run build`（tsc 严格）+ `npm run preview:ui` 逐场景肉眼核对。新界面（控制条、live panel、sub-agent 泳道、Inspector 各 kind、命令面板）各加一个 preview 场景——**这是这份计划里唯一能低成本复现"三个 run 并行、其中一个卡在审批"的手段**。
- **端到端**：起 app，两个项目并排各发任务、各触发审批，确认流式、隔离与桌面通知。
