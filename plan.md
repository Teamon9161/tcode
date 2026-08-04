# tcode — Rust Agent Harness CLI 设计文档

> 权威设计文档。改动涉及架构决策时先读它；已实现细节见代码与 `CLAUDE.md`，本文只留仍指导设计的原则、机制与未决项。


## 第一性原则：零猜测原则

**模型不应该花任何 token 去获取 harness 本来就知道的信息。** 模型的注意力应全部在任务上，而非推断 harness 状态。下面多个特性都是这一条原则的实例：

| 实例 | 消灭的浪费 |
|---|---|
| 中断契约 | 中断后模型自发重新验证文件状态 |
| 文件新鲜度追踪 | 长会话重复读未变动的文件 |
| **自愈式工具错误** | 工具失败后模型花额外 turn 定位原因：edit 的 old_string 不唯一 → 直接附候选位置上下文；read 路径不存在 → 附相近路径；命令不存在 → 附建议。**省一个 turn = 省一次完整前缀读取**，最大宗的 token 节约 |
| **开局项目地图** | 每会话开头仪式性的 ls / git status / 读 README：启动采集目录树两层 + git 状态 + scratch 路径，注入 system prompt 尾部，进缓存前缀，一次成本 |
| **尾部自知一行** | 模型不知剩余上下文与当前权限模式：每条最新用户消息附 `<tcode-status>context ~61% of 200k tokens · permission-mode: default</tcode-status>`（另有 `background running` 与未批准 draft 计划提醒），高占用时改派 sub-agent 的依据；附在尾部所以缓存安全。compact 由 harness 按阈值自动触发，不属模型决定 |

## 差异化设计（本项目特有）

1. **类型强制的 append-only Ledger**——缓存命中是编译期保证：历史只有三个合法操作 `append` / `truncate_tail`（rewind）/ `compact`（显式断点原子重写），全部缓存友好。
2. **中断契约**——Esc 中断时注入一条精确状态说明：哪些 tool call 完成、哪些被取消、文件是否被改动。
3. **文件新鲜度追踪**——记录每个已读文件的 (path, mtime, hash, 读取范围)。重复读未变动文件 → 返回一行 stub；被外部改动 → 才返回新内容并附说明。


## 三个贯穿全局的机制

### 1. Append-only Context Ledger

```rust
pub struct Ledger { entries: Vec<Entry>, compaction_base: usize }

impl Ledger {
    pub fn append(&mut self, e: Entry);
    /// rewind: 截断尾部。前缀不动, 缓存仍命中。
    pub fn truncate_tail(&mut self, to: EntryId);
    /// 唯一"改前文"的操作: 原子替换 [0, n) 为 Summary,
    /// 一次性付缓存代价, 之后前缀重新稳定。
    pub fn compact(&mut self, summary: Summary, upto: usize);
    pub fn as_messages(&self) -> Vec<Message>;
}
```

- System prompt + 工具定义会话内定死不变。
- Anthropic：`cache_control` 断点——system+tools 后固定一个，消息尾部滑动一个，控制在 4 断点预算内。
- OpenAI 兼容：隐式前缀缓存，append-only 天然命中。
- Compact 仅显式触发（`/compact` 或 token 逼近上限），子请求生成摘要。**一次压缩要么替换历史、要么什么都没发生，`compact()` 返回的就是这个事实**：没产出摘要时 ledger 一字未动，触发它的阈值仍然成立，所以自动路径必须停手（写一条 Note 说明，`/compact` 手动仍可再试），否则每个安全边界都会重来一次——而那是这个会话最贵的一种请求，且恰好在前缀最大的时刻发出。同理，**队列里的用户消息在压缩之后才投递**：先投再压等于把用户刚打完的那句话压成别人的转述。
- **Progress 文件是外部可变状态，不是历史**：`~/.tcode/projects/<id>/progress/*.md` 可以被用户随手改，ledger 里只记模型发过的 `progress` 工具调用。二者不冲突——"文件可改"不等于"历史可改"，这份文件记的是**现在为真**的东西，ledger 记的是**当时发生过**的事。用户改过之后下一次工具调用返回自愈冲突（附他们的原文），而不是悄悄覆盖。
- **Compact 移出模型上下文的条目进 `archived`，不销毁**：`entries()` / `as_messages()` 语义一字不变（模型只见 Summary），但 transcript 与 `/export` 走 `history()` = archived + entries，resume 后仍看得到压缩前的对话。archived 没有合法 `truncate_tail` 索引（rewind 进不去被压缩的历史）；`truncate_tail(0)`（`/clear`）连它一起清空。

### 2. Stream Watchdog + 永远知情的状态行

- chunk 级 idle 超时（默认无字节 → 取消 → 指数退避重试，429/5xx/超时可重试）。
- 状态行实时显示：`thinking 12s · ↑3.2k` / `writing · ↑1.8k tok`（流式 delta 实时累计）/ `↻ retrying (2/3) in 4s` / `running: cargo build 45s`。无任何静默状态。
- **所有重试统一在 agent 层**（`agent.rs::stream_step` 的 `'retry` 循环）：连接失败与流中途 stall 同一处理，每次发 `AgentEvent::Retrying`，因此都可见。provider 只做单次尝试并分类返回错误。

## 核心抽象

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    /// 统一流事件: TextDelta / ThinkingDelta / ToolUseStart / ToolUseDelta /
    /// Usage / Done / Error。两家 API 差异在 provider 内部消化。
    async fn stream(&self, req: Request, cancel: CancellationToken)
        -> Result<BoxStream<'static, StreamEvent>>;
    fn cache_strategy(&self) -> CacheStrategy;  // ExplicitBreakpoints | ImplicitPrefix
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn permission(&self, input: &serde_json::Value) -> PermissionRequest;
    async fn run(&self, input: serde_json::Value, ctx: &ToolCtx) -> ToolOutput;
}

/// UI 是事件流的消费者。inline/全屏/transcript 导出/resume 重放走同一接口。
pub trait Renderer { fn on_event(&mut self, e: &SessionEvent); }
```

`ToolCtx`：cwd、Freshness Tracker、checkpoint 写入、blob store、cancellation token、事件上报通道。

### Agent Loop

```
loop {
    req = ledger.as_messages() + tools + cache 断点
    stream = provider.stream(req)            // watchdog 包裹
    渲染 deltas (状态行实时累计 token); 收集 tool_use
    if 无 tool_use: break
    for call in tool_uses:                   // 独立只读调用可并行
        权限 (模式 → 规则 → 交互, 可带 Tab 意见)
        hooks.pre_tool_use
        checkpoint (若为写操作)
        output = tool.run()                  // freshness 去重 + token 预算门
        hooks.post_tool_use
        ledger.append(result + 可选用户意见)
}
// Esc: cancel → 中断契约注入精确状态 → ledger 保持对 API 合法
```

### 权限系统

**模式**（Shift+Tab 循环，状态行常显）：

| 模式 | 行为 |
|---|---|
| `default` | 按规则匹配，未命中则逐个询问 |
| `accept-edits` | 文件编辑自动放行，shell 等仍询问 |
| `auto` | 全部放行（deny 规则仍生效） |

**规则**：global + project 两级 `config.toml`，`allow`/`deny` 列表，匹配 `工具名(参数 pattern)`（`*` 是唯一通配），如 `shell(cargo *)`、`edit(src/**)`。交互中选 "Yes, don't ask again" 自动写入 project 规则。

**Tab 补充意见**：确认对话任意选项上按 Tab 展开内联输入框。"Yes + 意见" → 批准并把意见作为 user message 追加在 tool result 后；"No + 理由" → 拒绝原因进上下文。纯 append，缓存安全。

**模式切换即时生效于下一次权限判定**：shift+tab 切的模式只改闸门、不入 ledger。闸门在每个权限判定点提交（回合始、`run_tools` 入口、串行批每个待批调用前、批边界、回合末），所以切到 unsafe/auto 后，模型这一步吐出的调用、乃至同批后续调用立刻按新模式判定，不必等一整个来回到下一批边界。**模式从不作为 note 写进 ledger**：模型看到的当前模式由 `status_block` 每回合重新导出到 user turn 的 tail，所以裸切键既不用追加什么、也没有什么需要撤回。曾经的 plan enter/exit note 是唯一的例外，随 plan mode 一起删除。

**读工具永不询问**：read/grep/glob 的 `permission()` 返回 `None`，任何模式直接放行；外部路径门控只拦 mutating 工具。

**plan mode 已删除，规划改由 Progress 文件承载**。它曾占着 `PermissionMode` 的一格，但"我还在决定做什么"和"我愿意承担多大风险"是正交的两件事，合并的代价是规划期选不了 auto/unsafe——而探索恰恰是最该放开的阶段。它在 `decide()` 里也从来没有自己的臂（与 default 同一段代码），即**在权限表里本来就没有语义，却霸占着权限表的一格**。现在"先别动手"这个约束来自**文件处于 `draft`** 这一事实：`status_block` 每回合从文件重新导出，不是一次性注入后靠模型自觉记住的指令，批准后自动消失。唯一保留的权限语义是 `PermissionRequest::PlanReview` **在任何模式下都 Ask**（含 unsafe）：用户要求看计划，与他执行时愿担多大风险是两个问题、两个答案，正好在同一个对话框里分别回答。

**`scratch` 没有任何模式例外**：scratch/memory 的本地放行只发生在 auto mode，别的模式一律照常审批。

**子 agent 继承父会话的模式与规则，能力天花板由 def 自己声明**。两个正交旋钮，别混：

- **继承（`ToolCtx::delegated_permissions`，由 `forward_delegates` 在每次委派调用期间安装并在结束时清除）**：委派出去的活仍是本会话的活，用户为它选的模式、写的 allow/ask/deny 一并适用。读的是**调用时**的状态，所以 turn 中途切模式对下一次委派立刻生效；resume 追问同样重新取，不重放当初 park 时的旧姿态。修掉的是一个真 bug：子 session 曾用 `PermissionRules::default()`，**用户的 deny 规则对子 agent 完全不生效**（`deny=["run(*)"]` 实测拦不住），而权限表明写着 deny 连 unsafe 都能穿透——委派曾是它唯一的静默缺口。同理，父在 default 时子 agent 不再靠分类器自我批准。
- **天花板（def 的 `readonly`）**：mutating 工具在 `sub_tools` 里就被摘掉。它比模式更强，因为模式可以被用户点 yes 抬高，而这里连请求都不存在——所以 `explore` 在父是 `unsafe` 时依然动不了项目（有测试钉住）。

`plan` 据此**去掉了 `readonly`**：它的职责本来就包含"先 clone 参考仓库再出计划"，而 def 正文一直在教它这么做、工具集里却没有 shell——承诺了一个结构上不存在的能力。现在它拿到 write/append/edit/shell，全部经继承来的模式把关。**注意 `readonly` 仍重载着另外两件事**，所以摘掉它连带两个可观察变化（各有测试钉住）：`agent(agent='plan')` 的派生本身现在要审批一次（`permission()` 只对 read-only def 返回 `None`），且不再走 `ParallelReadOnly` 批次而是 `Isolated`（对一个能写的 agent 反而更合适）。真嫌派生那次审批多余，就得把 `readonly` 拆成"能力天花板 / 派生免审 / 可并行"三个字段——继承落地后派生审批已是双重把关，但那是独立的一次改动。

**提交计划结构性地不发给任何子 agent**：`progress` 根本不在 `builtin_tools()` 里，它由三个前端在主会话装配点单独 push（`tcode-frontend/src/agent.rs`、TUI harness）。提交计划意味着**父会话**的权限模式迁移，这不是被委派方能做的决定。`sub_tools` 里那条按 `PermissionRequest::PlanReview` 过滤的 retain 留作兜底，防的是以后有扩展工具带上同一请求类型。

**审批桥对所有委派运行安装**，不再由 `questionPolicy` 把关。`questionPolicy` 管的是 `ask_user` 这个**工具**（能不能问开放式问题），与"它要动手时人有没有权决定"是两件事；绑在一个字段上，继承一个会询问的模式就会静默变成一个会拒绝的模式（`NeverAsk`）。

**scratch 目录必须在第一个工具跑起来前就存在**（`ToolCtx::with_scratch_dir` / `rebind_scratch_dir` 里 best-effort `create_dir_all`）：它以前只是个算出来的路径，靠 `write` 建父目录顺带成形，于是 `shell(cwd=scratch)` 撞 `cwd does not exist`——我们把一个路径塞进项目地图承诺给模型，就得保证它真的能用。

**auto mode 本地放行 memory 写入**：`~/.tcode/projects/<id>/memory/` 既不在项目根也不在 scratch 内，但 policy.md 本来就声明该目录写入合法——每次记忆维护都付一次分类器请求只是让模型给自己盖章。`AutoModePolicy::with_memory_root` 把它变成本地快速路径（仅文件编辑工具；shell 在该目录跑命令、或经重定向间接写，仍走分类器，故 policy.md 的 `${TCODE_MEMORY_DIR}` 条款保留）。

**教训——保护路径检查曾误伤 scratch 自己**：`is_protected_path` 把任何含 `.tcode` 组件的路径算作保护路径，而生产环境 scratch 就在 `~/.tcode/projects/…/scratchpad/` 下，于是 auto mode 写 scratch 的快速路径**实际从不生效**（临时目录做的单元测试测不出来）。修法是分层判定：scratch 与 memory 先判、且不做保护检查（`AllowInScratch` 与 plan-mode 例外本来就不查，三者立场一致），项目路径才走 `!is_protected_path`。回归测试直接用 `.tcode` 下的 scratch 路径钉住。

### Hooks

`config.toml` 按事件 + 工具 matcher 触发外部命令，JSON 走 stdin/stdout（对齐 Claude Code）：`pre_tool_use`（可 block/改参）、`post_tool_use`、`turn_end`、`session_start`。

## 工具集

| 工具 | 要点 |
|---|---|
| `read` | offset/limit；**无行号 gutter**（内容逐字返回，引用行时从窗口首行数起——省掉每行的行号 token）；经 Freshness Tracker 去重；识别图片按 magic bytes 归一化后返回 image block（文本模型自愈指向 `view_image`）；大输出/后台日志落 scratch 文件用 read 分页 |
| `view_image` | 以独立 cache scope 调用 `[agents.vision]`（或主模型）按需理解最多 8 张图片，文本结论回流主会话，图片不驻留 ledger |
| `write` / `edit` / `append` | edit = 精确字符串替换；write 覆盖已有文件要求**完整**读过当前版本（partial 视图得到列出已见行段的自愈错误）；append = 末尾原样追加（部分读过即可、缺失文件直接创建、不自动补换行）；三者执行前存 checkpoint；渲染红绿 diff |
| `shell` | Windows: PowerShell 为主 + 检测到 Git Bash 时提供 `bash`；`run_in_background` 进后台注册表，日志流到文件，`kill_task` 停 |
| `monitor` | 后台监视（对齐 claude-code 的 Monitor）：跑平台主 shell 脚本，stdout 每行即一个事件（512B 截断），安全边界作为 `Entry::Note` 注入、空闲时前端按 quiet 合流窗口唤醒 `monitor_turn`（每次空闲唤醒 = 一次完整前缀 cache read，合流即省钱）；事件是 Note 不是 User，Auto Mode 授权判定天然不把事件当用户授权（claude-code 靠 prompt 纪律，这里靠类型）；洪水自动停（120 事件/60s，附"收紧过滤器"自愈提示）；与 shell 共用注册表、日志管道、`kill_task` 与权限规则域（`run(...)`）；默认 5min 超时，`persistent` 免超时；resume 时未终结的任务/监视注入一条"未恢复"Note（零猜测） |
| `grep` / `glob` | 内嵌 grep-searcher/ignore/globset；每行截 512B、`max_filesize` 上限、并行 + 按 (path,line) 排序、deadline 兜底给 partial 标记、剪 VCS/缓存目录、搜 dotfiles + offset 分页 |
| `task` | sub-agent：注册表选类型（`general` + 只读 `explore`），独立 ledger，受限工具集；`background: true`（仅主 agent）不阻塞派发，完成时 report 作为 fenced Note 唤醒主 agent（见改进 5），非交互 |
| `web_fetch` / `web_search` | 见下 Web 节 |
| `progress` | 一个任务一份耐久的工作分解文件（`~/.tcode/projects/<id>/progress/`，`draft`/`active`/`done`），也是前端进度面板的数据源。**它是该文件唯一的写入者**——模型不得用 `edit` 改它。返回值只回显刚翻成 `[>]` 的那一阶段的 `detail`，所以十二阶段的计划任何时刻只有一个阶段的正文在上下文里。`state: "active"` 就是"提交给用户审批"这一动作本身，因此 `permission()` 是输入的纯函数；用户手改文件后下次更新返回自愈冲突（附他们的原文）。不可代替方案、结论或交接记录。 |
| `ask_user` | 必须由用户选择才能继续的阻塞分歧；支持多问题分页。不可用于可由代码、项目上下文或现有用户要求确定的细节。 |
| `add_note` | 当前 Ledger 的一条高价值交接记录：仅记录用户决策、已验证约束或未完成工作的边界，供后续步骤延续。不是进度跟踪，不写入跨会话自动记忆；compact 后是否保留由摘要决定。 |

## 配置与运行时路径

- `~/.tcode/config.toml`：provider profiles、全局权限规则（手写，首启向导生成初版）。
- `~/.tcode/state.toml`：当前 profile/model/effort 选择（程序只写这个）。优先级 CLI flag > state > config。
- `.tcode/config.toml`：项目级 hooks、权限规则、MCP server（`[mcp_servers.名字]`，工具注册为 `mcp__名字__工具`）。
- `[agents.<kind>]`（`explore`/`plan`/`general`/`auto`/`suggest`/`vision`/`fetch`）：给 sub-agent 与辅助角色钉模型，`profile`/`model`/`effort` 三个可选字段，未写的继承父模型选择；也可写成一个字符串（`"off"` / `"inherit"` / 模型名）。`fetch` 是唯一"未钉即关"的角色（web_fetch 返回原文而非回退主模型）。Codex CLI 凭证与动态模型缓存由 `tcode-providers` 在加载配置后补全，core 只解析已规范化的 profile 模型。
- **`[presets.<name>]` 是"整套模型编排"，与 `[profiles.*]`（怎么连到 provider）正交**：主模型 + 每个角色跑什么，整体切换。三层解析次序 `[agents.*]` → 活跃 preset → `[tcode_state]` 的临时 pick，`Config::apply_active_preset` 是唯一汇合点。**切 preset 会清空 `[tcode_state]` 里的临时 pick 与主模型**——不清就永远没有一个 preset 能完整描述"现在跑的是什么"；想留住微调就 `/model save <name>` 存成新 preset（这也是程序唯一一处写 `[tcode_state]` 之外的表，只增/替换那一张）。
- 持久上下文两类禁止混写：**人维护指令**（项目根→cwd 分层，每层 `.tcode/AGENTS.md` > `AGENTS.md` > `CLAUDE.md` 取第一个）；**模型维护自动记忆**（`~/.tcode/projects/<id>/memory/`，`MEMORY.md` 只做精简索引）。
- 会话/checkpoint/blob/scratch：`~/.tcode/projects/<project-id>/{sessions,checkpoints,blobs,scratchpad}/`。scratch 暴露给模型（project_map 的 `scratch:` 行 + 系统 prompt 引导），溢出输出与后台日志落 `scratchpad/tool-output/`，7 天清理。
- **`<project-id>` 用路径本身而非 hash**（`c:\code\rust\tcode` → `c--code-rust-tcode`）：这个目录名是人去翻会话日志、存档 plan、项目记忆时唯一的线索，不可读的 hash 让每个目录都无法辨认。折叠分隔符不是单射（`C:\code\rust-tcode` 会撞上），代价是两个项目共用一份状态，接受。旧 hash 目录在 `store::project_dir_in` 里懒迁移（rename，目标已存在则并入），迁完即可删那段代码。
- **voice sidecar has an independent release version**: `tcode-voice-protocol` is the shared source of `SIDECAR_VERSION` and the `voice-v<version>` release tag. Normal tcode releases reuse `~/.tcode/voice/tcode-voiced-<sidecar-version>`; bump and publish this version only when the sidecar itself changes. The sidecar’s `Cargo.toml` version is tested against the shared version, while the `voice-v*` workflow validates both manifests before building platform assets.
- API key 经 `api_key_env` 指环境变量，不落盘。

## 验证方式

- Ledger / 缓存断点 / 预算门 / Freshness Tracker：纯单元测试。
- Agent loop：MockProvider 脚本化 tool_use 序列做集成测试，不打真 API。
- provider SSE/wire 格式：`tcode-providers/tests/wire.rs`。
- 每里程碑用真实 API 跑端到端，盯状态行缓存命中数字（对"省 token"的持续验收）。


## 改进
1. 图表数据绑定（`show` 第二阶段，`{"$file": "pnl.csv"}`）——**计划已写，未实现**，执行细节与"可能不做"的前提检查见 `crates/tcode-app/DATA-BINDING.md`。
2. gpt订阅有图片生成模型吗
3. acp支持
4. plan模式比如execute in a new session, detail的内容感觉是在太少了,不像以前plan mode那样,plan是特别详细的,这是为什么呢, 没什么detail的plan感觉作用不大吧,等于new session又要重新探索.
5. read：读取 Curves.tsx 时，工具返回“文件未变更，内容已在上下文中”，但压缩后的可见上下文并没有该文件内容，导致必须追加一次 force=true 读取。若上下文经过压缩，read 应返回所请求的片段而非“已在上下文”提示，可避免这次额外调用。
6. app askUser我在给选项增加note的时候,回答比较长, note的框不会自动拓展.
7. app background sub-agent finished怎么还会显示到主页面上,这个应该是给主agent看就好了吧.
8. tui和app都保留记录unsafe状态吧,之前好像特殊实现,unsafe状态不会被记录,每次要重新选.
9. show html的时候记录中能不能自动放缩下,比如有的html里面只有一张交互式图片,但是现在在主对话中都看不全,这块能不能看下怎么调整更好
10. app terminal窗格, 类似ide, 主题跟随tcode app, 支持ctrl+j调出.
11. app ai给的markdown返回中可能有链接,可能是网页, 图片, csv等,这个能不能点击打开,目前点击没反应,因为我们已经有对应的窗格可以渲染了.
