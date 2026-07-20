# tcode 与 pi 的 token 开销对比

对比对象：[earendil-works/pi](https://github.com/earendil-works/pi)（badlogic 的 TypeScript agent harness），
commit 为 2026-07-21 的 main HEAD。

**方法与可信度说明**：本文的 tcode 数字是在本仓库（`C:\code\rust\tcode`）用一次性测试实测的字节数，
token 数按 4 B/token 粗估（英文散文与 JSON 的经验值，误差 ±15%）。pi 的数字是读源码后按同样口径手算的，
**没有实际跑起来测量**，因此 pi 侧标注为"估算"。两者都**没有做同任务的端到端 token 消耗对照实验**——
本文比较的是架构上的成本结构，不是某个 benchmark 的分数。

---

## 决策

**值得改** —— tcode 每次请求的固定前缀实测约 **10.4k tokens**，pi 估算约 **1.0k tokens**，
差了一个数量级；且 tcode 前缀里最大的一块（项目指令 16 KB）是被静默截断的，即"付了钱还没拿到货"。

---

## 一、实测数据

在本仓库跑 `builtin_tools()` + `project_map()` 得到（未提交的一次性测试）：

| 组成部分 | 字节 | ≈tokens | 备注 |
|---|---:|---:|---|
| 工具描述 + JSON schema（14 个内建工具） | 13,250 | 3,312 | 不含 `task` / MCP / `update_progress` 等 |
| system prompt (`interactive-agent-system.md`) | 10,299 | 2,574 | 固定，与工具集无关 |
| 项目指令（CLAUDE.md，经 `INSTRUCTION_CAP`） | 16,000 | 4,000 | **本仓库 CLAUDE.md 实为 30,368 B，被砍掉一半** |
| 项目树 + environment + git | 702 | 176 | 这块很克制，没问题 |
| **合计（开局前缀）** | **~40,251** | **~10,062** | 尚未说一句话 |

工具开销明细（desc + schema，字节）：

```
     skill  2384   |   bash        868
   monitor  1923   |   read        767
 web_fetch  1250   |   exit_plan   720
     shell  1220   |   append      448
      grep  1188   |   glob        421
      edit  1116   |   kill_task   301
                   |   web_search  294
                   |   write       265
```

pi 的同口径估算：

| 组成部分 | ≈字节 | ≈tokens |
|---|---:|---:|
| system prompt（`buildSystemPrompt` 默认分支，不含 AGENTS.md） | ~1,600 | ~400 |
| 4 个默认工具（read/bash/edit/write）desc + schema | ~2,200 | ~550 |
| **合计** | **~3,800** | **~950** |

pi 也会拼 AGENTS.md（本仓库的 CLAUDE.md 等价物），所以"项目指令"那一项两边一样贵——
**差距全部来自 harness 自身的固定成本：约 5.9k vs 约 0.95k，6 倍。**

---

## 二、pi 为什么便宜：四个结构性原因

### 1. 默认只有 4 个工具，且工具集决定 system prompt 长度

```
createCodingTools() → [read, bash, edit, write]
```

`grep` / `find` / `ls` 都存在，但**不是默认装配**——默认走 `bash`：

```ts
if (hasBash && !hasGrep && !hasFind && !hasLs) {
    addGuideline("Use bash for file operations like ls, rg, find");
}
```

一行 guideline 顶掉了三个工具的 desc + schema。tcode 的 `grep`(1188) + `glob`(421) = 1,609 B，
换成一行"用 shell 跑 rg/fd"是 ~60 B。

更关键的是 pi 的 system prompt 是**从活的工具集拼出来的**：

```ts
// 工具自带 promptSnippet（一行）与 promptGuidelines（几条）
promptSnippet: "Read file contents",
promptGuidelines: ["Use read to examine files instead of cat or sed."],
```

`_rebuildSystemPrompt(toolNames)` 只收集当前启用工具的 snippet 与 guideline。
**摘掉一个工具，它在 prompt 里的成本同时归零。**

tcode 是反的：`interactive-agent-system.md` 是 10 KB 的固定块，
不管这次会话有没有 `monitor`、有没有 `web_fetch`、是不是 sub-agent，全额付。

### 2. `read` 不加行号

pi 的 `read` 直接返回文件原文，只在末尾追加一行 `[Showing lines 1-500 of 1200. Use offset=501 to continue.]`。

tcode 的 `read` 走 `numbered_capped`，每行前缀 `行号\t`。
按每行 3–5 个 token 的行号开销算，读一个 500 行文件多付 **1.5k–2.5k tokens**；
一次会话读 20 个文件就是 **30k–50k tokens 纯行号**。

行号的用途只有两个：`edit` 定位、跟用户对话时引用位置。
但 tcode 的 `edit` 和 pi 的 `edit` **都是精确字符串匹配，根本不用行号**。
所以这笔钱买的只是"引用行号"的便利。

### 3. 输出上限低一半以上，且没有第二次机会

| | pi | tcode |
|---|---|---|
| read/bash 单次输出 | 2000 行 **或 50 KB** | 2000 行 **或 128 KB** |
| grep 单行上限 | 500 字符 | 16,384 字符 |
| 超限行为 | 截断 + 落 temp 文件 + 告知续读 offset | blob 门 + 落 scratch |

tcode 的 `MAX_READ_OUTPUT_BYTES = 128 KiB` ≈ **32k tokens 一次读进来**。
这个上限的设计意图（CLAUDE.md 里写着"逐行上限是防单行 minified 巨行，不是第二道预算门"）是对的，
但 128 KiB 的总门槛本身就太松——它默许模型一次性把一个大文件整个吞进永久上下文。
pi 的 50 KB 更接近"一屏工作集"的量级。

`MAX_LINE_CHARS = 16384` 同理：一条 grep 命中行最多 16k 字符 ≈ 4k tokens，
pi 是 500 字符。grep 结果 100 行，最坏情况差 400k vs 12.5k tokens。

### 4. 每回合零额外请求、零注入

pi 的 `convertToLlm` 全文读一遍就是：把 message 数组原样转成 LLM message，
compaction summary 包一层 `<summary>`，bash 执行包一层 markdown。**没有 system-reminder，
没有环境快照 diff，没有每回合注入**（`grep -rn "reminder"` 在 pi 全仓零命中）。

tcode 相比之下有三条**额外请求**通道：

| 功能 | 频率 | 成本 |
|---|---|---|
| `suggest`（猜下一句 prompt） | 每回合 1 次 | 已按"只读最后一轮"优化，但仍是 1 次请求 |
| Auto Mode 分类器 | 每个工具批次 1 次 | 独立前缀 + 全量转录 |
| 自动记忆维护 | 视配置 | 额外请求 |

这些在 CLAUDE.md 里已经有专门纪律约束（"辅助模型角色必须像顺手一样便宜"），
设计是清醒的。但**"便宜"和"没有"仍差一个量级**，而且这三条 pi 一条都没有。

---

## 三、tcode 侧发现的两个实际问题

### 问题 1：项目指令被静默截断一半（真 bug）

`INSTRUCTION_CAP = 16_000`，而本仓库自己的 `CLAUDE.md` 是 **30,368 B**。
也就是说 tcode 在自己的仓库里跑时，模型看到的 CLAUDE.md 只有前 16 KB——
从"配置与运行时路径"整节开始的内容**模型从来没读到过**，
包括 `~/.tcode/config.toml` / `state.toml` 的优先级规则、磁盘回收规则、`[agents.*]` 钉模型规则。

这是最坏的一种花费：付了 4k tokens，拿到的是一份中途断掉的指令，
且模型不知道自己看的是残本。

**建议**：
- 截断处必须留自释标记（"CLAUDE.md 从此处截断，剩余 N 字节，用 read 取回"）——
  这正是 tcode 对 read/grep 已经强制执行的纪律（`read_marker`），指令加载路径漏掉了。
- 更好的做法是**分层加载**：只把项目根的 `AGENTS.md` 常驻前缀，
  子目录的按工具目标路径懒加载（tcode 已有这个机制，但根文件本身没分层）。
- 顺带：本仓库的 CLAUDE.md 该瘦身了。30 KB 的"改动勿回退的硬规则"更适合作为
  `.tcode/skills/` 下按需读取的技能，而不是每次请求都付费的前缀。

### 问题 2：CLAUDE.md 里记的项目地图预算与代码不符

CLAUDE.md 写"项目地图（80 项/目录、20 子项、**16 KiB**）"，
但 `grounding.rs` 里只有 `TREE_MAX_ENTRIES=80` / `TREE_MAX_PER_DIR=20`，**没有任何 16 KiB 的门**。
实测 `project_map()` 返回 18,105 B，其中树只有 702 B，其余 17,403 B 全是 memory/指令。
文档描述的边界不存在，容易让后来的改动误以为有兜底。

（这块本身不贵——树只有 702 B，很克制。问题只是文档不准。）

---

## 四、建议改法（按 ROI 排序）

### A. `read` 去掉行号，或改成可选（预计省 10–20% 总消耗）

最大单点收益。三个选项：

1. **默认不加行号**，`read` 增加 `line_numbers: bool = false` 参数，
   模型需要引用位置时显式要。
2. **只在小窗口加行号**（比如 `limit <= 200` 时加），大范围浏览不加。
3. **稀疏行号**：每 10 行标一次。省 90% 行号开销，仍可定位。

推荐 1。`edit` 是精确匹配，行号对 harness 的正确性零贡献；
展示给用户的行号可以在 TUI 侧渲染时补上——**转录是唯一事实源，
但发给模型的和画给用户的本来就不必是同一份文本**。这一点 tcode 目前混在了一起。

### B. system prompt 按工具集拼装（预计省 1–2k tokens/请求）

把 `interactive-agent-system.md` 拆成：

- **不变的核心**（Trust and authority、Harness protocol、Working style）：~4 KB，永远付。
- **工具相关段落**：下沉到各个 `Tool` 实现，加一个 `fn prompt_guidelines(&self) -> &[&str]`
  的 trait 方法（与现有 `batch_policy()` / `gates_output()` 同形，注册表插拔，符合项目第 3 条设计约束）。

收益不只是省钱：sub-agent（explore 是只读工具集）现在也在付 `write`/`edit`/`monitor`
相关的 prompt 段落，而它连这些工具都没有。

### C. 收紧输出上限

```rust
MAX_READ_OUTPUT_BYTES: 128 * 1024  →  48 * 1024
MAX_LINE_CHARS:        16384       →  2048
```

`MAX_LINE_CHARS` 当初从 500 调到 16384 是为了修"6 行小文件的正常长行被截"的问题，
方向对但过头了。2048 字符（~512 tokens）对 prose/config/markdown 的自然行长足够宽，
对 minified 巨行仍是有效防线。

### D. 工具集瘦身

- `append` (448 B)：`edit`/`write` 能覆盖的场景，且 CLAUDE.md 已说"prefer edit"。考虑删。
- `glob` (421 B)：`grep` 有 `glob` 过滤参数，`shell` 有 `fd`/`Get-ChildItem`。考虑合进 `grep`。
- `monitor` (1923 B) / `web_fetch` (1250 B) / `skill` (2384 B)：
  这三个是最大头，但都是真能力。建议**按需装载**——
  `skill` 工具只在项目真有 skills 时装（已经这么做了），
  `monitor` 可以做成"用过一次才进工具表"或配置开关，
  会话不需要它时省 1.9 KB。

参考 pi 的判断标准：**一个工具值不值得占前缀，看它是否比"让模型写一条 shell 命令"更可靠**。
`monitor`、`web_fetch` 通过（shell 做不到）；`glob`、`append` 不太通过。

### E. 给 `INSTRUCTION_CAP` 加自释标记（正确性修复，不是省钱）

见上面问题 1。这条优先级实际上应该最高，因为它是**功能缺陷**而非成本问题。

### F. 可选：抄 pi 的 cache-waste 诊断

pi 有 `cache-stats.ts`：逐个 assistant message 比对"上一次请求的 prompt tokens"
与"本次 cache_read"，把差额换算成美元，并区分是空闲超时（>5min TTL）
还是模型切换导致的。tcode 的 CLAUDE.md 里写着"真实 API 端到端验证时盯状态行 cache_read 占比"——
这件事目前靠人眼盯，做成自动统计（`/cost` 之类）能把缓存回归变成可测的，而不是靠纪律。

---

## 五、明确不建议抄的

- **pi 没有权限系统**（README 直说，靠容器化兜底）。tcode 的 Auto Mode + 合并审批 + 信任边界
  是实打实的价值，不该为省 token 削弱。分类器的成本是这个能力的定价，不是浪费。
- **pi 的 read 不做 freshness 去重**。tcode 的重复读返回 stub 是净收益，pi 这块反而更费。
- **pi 没有 blob store**，超限就落 temp 文件让模型自己 `sed` 取——
  tcode 的 blob 分页更省。
- **pi 的脱敏为零**。tcode 的 `redact.rs` 是安全特性，与 token 无关。

---

## 六、一句话总结

pi 省 token 靠的不是某个精巧算法，是**默认什么都不给**：
4 个工具、一句话的工具描述、无行号的 read、无每回合注入、无辅助模型请求。
tcode 的每一项额外开销背后都有真实能力（权限、监控、记忆、审批），
但成本结构上有两处是纯浪费——**read 的行号**和**固定 10 KB 的 system prompt**——
这两项加起来就能拿回 pi 那 6 倍差距里的大半，且不损失任何能力。

优先做 A（read 行号）、E（指令截断标记）、B（prompt 按工具拼装）。
