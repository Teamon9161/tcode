# tcode-core 硬规则

改动不得破坏（设计层面的"为什么"见 `plan.md`；全局约束见仓库根 `CLAUDE.md`）。

## 上下文与缓存

- 进缓存前缀的内容都有预算：system prompt、项目树（`TREE_MAX_ENTRIES` 80 项、`TREE_MAX_PER_DIR` 20 子项）、项目指令（`INSTRUCTION_CAP` 16 KB）、自动记忆（`AUTO_MEMORY_CAP` 25 KiB / 200 行）、skills 列表（200 字符/6k）——加内容前先看现有上限。**前缀里最大的一块通常是项目指令**，所以根 `CLAUDE.md` 要克制，长条目下沉到分层 `AGENTS.md`（见根文件末尾）。
- 工具输出不可无门直灌 ledger：大输出必须过 blob 预算门并落 scratch 文件（曾因 `gates_output=false` 让 grep 单条巨行撑爆 context）。
- system prompt 会话内不变，唯一例外是 `/dogfood` 这类显式开关：切换时一次性重打前缀（与 compact 同量级），**不得**把同类指令塞进每轮 tail 反复付费。
- token 两个量纲不可混：context 表 = 单次请求的完整 prompt（缓存+未缓存）= 当前窗口占用；turn 汇总 = 本轮**未命中的 `input_tokens`** + cache%。勿用 `total_input()` 把缓存前缀按请求次数重复累加；运行时状态行 `↓ ~N tok` 走 `token_count`。
- 真实 API 端到端验证时盯状态行 cache_read 占比：连续 turn 应接近前缀全长，下跌即缓存回归。
- **辅助模型角色必须像"顺手"一样便宜**：`auto`（分类器）与 `suggest`（下一句 prompt 猜测）都不得重放主会话——分类器读过滤后的转录，suggest 只读最后一轮对话（不是 ledger）。诱惑总是"骑在主前缀上蹭缓存"，但那等于每回合为一个便利功能付一次全窗口 cache read（30k context ≈ $0.009/次），大模型上还要等好几秒，ghost 文本迟到就等于没有。二者都是 `AgentRole` 注册表中可钉的角色，就是为了让人把它们钉到小模型上。
- **一个前缀一个缓存作用域**：共用 provider ≠ 共用缓存键。凡是自带独立前缀的会话——Auto Mode 分类器（policy 前缀）、每个 sub-agent（自己的 ledger）——都必须经 `Session::with_cache_scope` / `Request::cache_scope` 声明作用域；`None` 只留给主会话。新增任何"复用主 provider 打另一套前缀"的能力时，先给它一个 scope，否则两套前缀在同一个键上互相稀释亲和性。

## 投递时机与条目类型

- **turn 运行中用户提交的 prompt 只能在工具批次边界投递**：`Entry::Assistant`(tool_use) 与其 `Entry::ToolResults` 之间不许插任何东西（否则请求非法），所以队列（`Session::pending` / `PendingInput`，前端持克隆句柄）由 agent loop 在"批次结果已提交"那一点 drain，append 成真正的 `Entry::User`——`as_messages` 会把它与同位置的 tool_result 合并进同一条 user message，仍是纯 append，模型下一步就读到。它是 `Entry::User` 而非 `Entry::Note`，因为 Auto Mode 的授权判定只认用户消息。循环没走到边界就结束的（收尾发言期间入队、或 ctrl+c 打断），由前端在 turn 结束时立刻起新 turn 发出去。
- **可逆的 harness 状态不许由控制操作直接写进 `Entry::Note`**：mode、`/memory on|off`、`/cd` 等先立即更新本地运行时状态；连续改动在 `Session` 的 pending 槽内覆盖合并。只有真实用户交互到达合法投递点（新 prompt、批次边界投递的排队 prompt、审批完成）才把**最终**状态 append 给模型；纯 UI 键、命令和 monitor wake 不得产生该类 Note。环境另分"已观察"与"已投递"两个 JSONL snapshot，resume 用后者作为模型已知基线；旧日志的 `EnvironmentChanged` 兼容地视作已投递。工具结果、hooks、monitor、中断、compact 等已发生事实仍立即 append，不能错误延迟。
- **monitor 事件是 `Entry::Note`，不是 `Entry::User`**：Auto Mode 授权判定只认用户消息，所以监控事件在结构上永远不可能被当成用户授权（claude-code 靠 prompt 纪律解决的问题，这里靠类型解决）——不得把事件改成 User 注入。事件注入分两种价位：turn 进行中搭批次边界的车（纯 append，免费）；空闲时每次唤醒 = 一次完整前缀 cache read，所以必须合流——前端等 quiet 窗口（deadline = 首个未投递事件 + quiet_ms，锚定首个事件保证有界延迟）后调 `Agent::monitor_turn`，无事件待投递时它不发任何请求。
- **`SKILL_ECHO_OPEN` 归 core（`ledger.rs`）而非 tools**：`/name` 触发的 skill 正文以 `Entry::User` 进 ledger（省一轮），但正文是仓库文件、不是用户的话。`ClassifierTranscript` 必须能在不反向依赖 tools 的前提下认出它并打成 `<skill-body>` 而非 `<user>`；Auto Mode 的授权判定只认 `<user>`。这条链断一环，clone 来的仓库就能靠一个诱人的命令名（`/test`、`/build`）拿到用户授权。格式本身仍只有 `wrap_skill_echo` / `parse_skill_echo` 知道。
- **Auto Mode 分类器判决按批缓存，不按调用**：`ClassifierRequest` 只有 policy / cache_scope / 整份转录，而承载这一批全部 tool_use 的 assistant 条目在第一次权限检查前就已在 ledger 里——逐个调用发的是字节相同的请求。`BatchClassification` 缓存的是**已解析的 `Decision`** 而非原始判决，这样暂停计数与 mode 变更事件每批恰好发生一次（缓存原始判决会让一批 N 个调用把连续 block 计数放大 N 倍）。

## Progress（规划与执行进度）

- **`progress.rs` 的文件是外部可变状态，不是历史**：ledger 只记模型发过的工具调用，这份 md 记的是"现在为真"。所以它可以被用户随手改，与 append-only 不变量不冲突——**别据此以为"文件可改"等于"历史可改"**。
- **工具是这份文件的唯一写入者**（`apply_call` / `review_copy`）。模型不得用 `edit` 改它：edit 要先 read 回来再精确匹配 `old_string`，翻一个阶段的成本从"一次 no-op 调用"涨到"整段文本进两遍上下文"；而且用户一旦手改过，模型记的 `old_string` 必然对不上。写盘前一律 `reconcile()`，hash 变了就**不覆盖**，返回带用户原文的自愈错误（零猜测：模型不必花一次 read 去查发生了什么）。
- **进度状态绝不进 system prompt 或开局前缀**。每回合变化的东西挂 `status_block`（user turn 的 tail），违反这条就是每翻一个阶段废一次前缀，且正好发生在上下文最大的时候。开局 inventory（哪些 progress 没做完）是会话内稳定的，才可以进前缀——而且它是**清单不是指令**：躺在磁盘上的旧 draft 是三天前的自己写的，不是正在对话的人的请求，恢复必须显式（用户开口，或 `/plan resume <n>`）。
- **只在三个时刻注入摘要**：新会话/resume、compact 之后、用户手改文件之后（后者就是 reconcile 的错误返回）。会话内模型自己发过的 `progress` 调用就在 ledger 里，重发是纯浪费。摘要只给标题行与状态框，**不给 `detail` 正文**——正文按需在"刚翻成 `[>]`"的那次工具返回里下发。
- **`state: "active"` 就是"提交审批"这个动作本身**，这是 `permission()` 能保持为输入纯函数的原因；模型自建的跟踪文件从不提 `state`，因此不会弹框。`PlanReview` 在任何模式下都 Ask（含 unsafe），且结构上不发给子 agent（`progress` 根本不在 `builtin_tools()` 里）。
- 两级封顶（`##` / `###`）是硬约束，在 `Phase::from_json` 里拒绝；放开它模型会生成树状怪物。

## 记忆与截断

- **自动记忆是注入的持久化通道**：写进去的东西下次以开局前缀身份回来，比任何一次性注入都值钱。故 `memory/system.md` 限定来源——第三方内容只能作带出处的观察，"以后总是……"形状的常驻指令只有用户能授权。
- 持久上下文分两类：用户/项目指令由人维护，自动记忆由模型维护，**二者禁止混写**。
- **任何被预算砍掉的入 context 内容都必须自释**：`memory.rs::append_sources` 的项目指令超 `INSTRUCTION_CAP` 时必须说明截了多少、去哪读原文——静默砍半的 `CLAUDE.md` 比不加载更坏，模型会照残本执行且不知道有残缺。（同一条规则在 tools 侧对应 read/grep 的标记自释。）

## compact 与人类记录

- **compact 缩的是模型上下文，不是用户的记录**：被替换掉的条目进 `Ledger::archived`，`entries()` / `as_messages()` 完全不变（索引不偏移，checkpoint 与 rewind 的 `ledger_len` 不受影响），transcript 与 `/export` 走 `history()` = archived + entries。别让前端自己攒一份平行历史——只有 ledger 知道 compact 拿走了什么，攒平行历史必然在 resume 上漂移（这正是原来 resume 后看不到 compact 之前对话的原因）。
- archived 里的条目**没有合法的 `truncate_tail` 索引**：replay 不给它们打 entry tag，rewind 因此进不去被压缩的历史——那本来就是 compact 的语义。反过来 `truncate_tail(0)`（`/clear`）会连 archived 一起清空：summary 一走，它代表的那段历史就不再属于这个会话，否则"清掉的对话"会在 resume 时复活。

## 磁盘回收

启动时 best-effort 扫一遍，失败即忽略。

- `sweep_old_sessions` 保留最近 100 个**有内容的**会话且不超过 30 天，**会话日志与它的 checkpoint 目录同生共死**（还能 resume 的会话必须还能 rewind；反之 checkpoint 没了日志就是无名垃圾），顺带收掉孤儿 checkpoint 目录；空日志（启动了没说话）不占名额、直接删，但有 1 小时宽限期以免删掉另一个正在启动的实例的日志。
- `sweep_scratchpad` 对**整个 scratchpad** 一条规则：7 天没被碰过的文件删掉，空掉的目录随之删掉——harness 的溢出输出和模型自己建的构建目录/探针脚本同一把尺子，**不要再给某个子目录开豁免**（曾经只扫 `tool-output/`，结果模型建的 3 GB cargo target 在缝里躺了下来）。
- checkpoint 的 pre-image **按内容 hash 命名**（128-bit FNV-1a，冲突会还原错文件，故不用 64-bit），同一文件反复编辑只存不同状态各一份。

## 测试

核心机制（ledger、freshness、blobs、权限、hooks）用内联单元测试。测试永不调真实 API。

**测试也不许碰开发者的真实 home。** `ToolCtx` 一被构造就建 scratch 目录并读写自动记忆，所以任何构造 `ToolCtx`/`MemoryManager`/`Config` 的测试必须先经 `tcode_core::home::testing::temp_home()`（用 `ToolCtx::for_test` 即自动生效）把 `TCODE_HOME` 指到临时目录。忘了这一步不会让测试失败——它只是每个临时 cwd 在 `~/.tcode/projects/` 里留一个目录，攒到几万个才被发现。同理，测试的判定不得依赖真实 home 里恰好装了什么（voice sidecar 曾因此在作者机器上过、在别人机器上触发下载）。
