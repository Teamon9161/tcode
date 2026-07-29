# 图表数据绑定（`$file`）— 未实现，本文是执行计划

`show` 的第二阶段。第一阶段（`show(路径)` + 扩展名注册表 + 内联渲染）已实现，见 `AGENTS.md` 硬规则 13/14。本文写给**没参与过那次对话的人**：先读"这是不是个真问题"，同意了再往下做。

## 先做前提检查（可能的结论是"不做"）

第一阶段之后，token 成本**已经是常数**：模型写脚本 → 脚本吐出完整的 echarts option（数据内联在 JSON 里）→ `show(chart.json)`。数据从头到尾没进对话。

所以 `$file` 引用买到的**不是** token，只有两样：

1. **图表规格可以手写**。今天要画图必须写脚本：读 CSV → 组装 option dict → dump JSON。有了引用，模型把 `df.to_csv("pnl.csv")` 写完就够了，图表本身是 15 行手写 JSON。省掉一整个脚本往返和它的失败模式。
2. **降采样归 harness**。今天靠模型记得在脚本里降采样；它不记得的时候，webview 就吃 50 万个点。这一条是模型结构上做不好、harness 结构上做得好的事。

**如果先用一段时间发现"让模型写脚本吐完整 option"根本不别扭，就别做这个。** 它的价值全在第 1 条的摩擦上，而摩擦的大小只有真用过才知道。先攒几次真实使用再回来判断。

## 承重约束：绑定必须发生在沙箱之外

沙箱是不透明源（`sandbox="allow-scripts"`，无 `allow-same-origin`，见硬规则 11），**里面读不到任何文件、也够不到 IPC**。所以 `src/sandbox/echarts.ts` 不可能自己去解引用。

解引用只能在**父窗口**做：`Shown.tsx` 已经握着 `invoke("shown_file")`，在那里把引用换成真数据，再把一份已经内联好的 option 字符串 postMessage 进去——沙箱那侧的代码一行不用改。

这条要先写在这里，否则接手的人一定会先去改 `sandbox/echarts.ts`，撞一次 CORS 才发现。

## 语法：复用 echarts 的 `dataset`，不发明映射 DSL

```json
{
  "dataset": { "source": { "$file": "pnl.csv" } },
  "xAxis": { "type": "category" },
  "yAxis": { "type": "value" },
  "series": [{ "type": "line" }, { "type": "line" }]
}
```

**评估过并否掉的方案**：自造 `"data": "$col:pnl"` 这种列引用。否掉的理由是 echarts 本来就有 `dataset` + `encode`，列到系列的映射是它的问题不是我们的问题；自造一套等于在 echarts 之上再叠一层要维护、要写文档、还会和 `encode` 打架的语言。

落地成一条规则：**option 里任何位置出现 `{"$file": "路径"}`，就替换成那个文件解析后的内容**。递归走一遍树，一条替换规则，没有第二种语法。CSV/TSV → 二维数组（复用 `show.ts` 的 `parseRows`），`.json` → `JSON.parse`。

## 必须一起决定的细节

- **相对路径以被 show 的那个文件为基准**，不是 cwd。`chart.json` 写 `"pnl.csv"` 指的是它旁边那个。这是唯一符合直觉的答案，但要显式实现，因为 `shown_file` 的相对路径当前是相对 cwd 的。
- **数值强制转换**。CSV 单元格全是字符串，echarts 的 value 轴要数字。规则：**从第 1 行起**（跳过表头），能解析成有限数的转成数字，其余保持字符串。跳过表头是承重的——表头写 `2026` 不能变成数字，否则 echarts 会把它当数据行。
- **降采样，v1 不自己做**。给 line 系列默认补 `sampling: "lttb"`（option 自己写了就不覆盖），行数上限继续吃 `shown_file` 现成的 `VIEWER_TEXT_BUDGET` 截断（4 MB，切在行边界上，已有"showing the first part of a N 文件"提示）。**不要为此在 Rust 侧再写一个 CSV parser**——两个解析器必然漂移，而真正的降采样管线只有在实测卡住之后才值得上。
- **只解一层**。被加载进来的文件里再出现 `{"$file": ...}` 一律当字面数据，不递归解析。这样不需要环检测，也不需要深度限制。
- **引用数量上限**（8 个足够），超了报错而不是发 50 个 IPC。
- **只对 chart option 生效**，不要让 `.html` 也支持 `$file`——那是模板引擎，是另一个产品。

## 安全

路径来自模型，仍然**全部走 `shown_file` 这一个命令**，因此自动过 `tcode_tools::is_viewable_path`（cwd 或 `~/.tcode` 之内）。不新增边界、不新增命令。解引用之后的 option 照旧交给同一个不透明源 iframe——它仍然是数据，不是代码。

## 工作拆分

1. `ui/src/bind.ts`：`collectRefs(option)` / `applyRefs(option, loaded)` + 数值转换。纯函数，先写测试（替换、表头不转数、缺文件、超上限、`$file` 出现在嵌套位置）。
2. `Shown.tsx` 的 echarts 分支：加载 option → 收集引用 → 逐个 `shown_file`（basedir = 被 show 文件的目录）→ 替换 → 交给 `Sandbox`。
3. line 系列补 `sampling: "lttb"` 默认值。
4. 缺文件/超限时**内联报错并写出路径**（自愈），不要画一张空图。
5. **改 `ShowTool::description`**，把引用形态告诉模型——最容易忘、价值最高的一步。功能建好了没人用，就等于没建。
6. `AGENTS.md` 硬规则 13 补一句"绑定在父窗口做，永远不在沙箱里做"。

规模估计 ~250 行含测试，一个会话能完。

## 相关位置

- `crates/tcode-app/ui/src/show.ts` — 扩展名注册表 + `parseRows`（复用它）
- `crates/tcode-app/ui/src/Shown.tsx` — 唯一读磁盘的检视视图
- `crates/tcode-app/ui/src/sandbox/echarts.ts` — 沙箱侧渲染器，**本次不改**
- `crates/tcode-app/src/commands.rs::shown_file` — 字节服务
- `crates/tcode-tools/src/show.rs` — 工具本体与描述
