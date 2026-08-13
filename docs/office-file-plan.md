# Excel / Word 文件支持计划

**状态：设计中**
**目标：tcode-app 支持 xlsx/docx 的本地预览与轻量编辑，同时提供"用云文档打开"的逃生舱**

## 设计决策

### Excel — IronCalc

**选择 IronCalc 而非 Univer / Handsontable 的理由**：

- Rust 原生引擎（`ironcalc` crate）+ WASM 前端（`@ironcalc/wasm`），与 tcode 的 Rust 后端 + webview 架构天然契合
- 后端可直接用 Rust crate 做 xlsx 解析/回写/单元格提取（工具侧也能用），前端用 WASM + React 组件渲染编辑
- Apache-2.0，无商业限制
- Univer 的 xlsx 导入导出在商业付费墙后面；Handsontable 商业授权 $899+/开发者/年
- IronCalc 覆盖 300+ Excel 函数、多 sheet、格式化、命名区域，对"预览+轻量编辑"足够

**已知局限**：无图表渲染、无条件格式（开发中）、无数据透视表、无 VBA。这些是"用云文档打开"存在的理由。

### Word — docx-preview（只读）

目前没有成熟的纯前端 docx 编辑方案。`docx-preview` 可将 .docx 渲染为 HTML，保留大部分格式。编辑需求通过"用云文档打开"解决。

### 云文档逃生舱

当本地预览不够用时（复杂图表、条件格式、VBA、完整 Word 编辑），提供菜单让用户选择外部服务打开：
- 腾讯文档
- 飞书文档
- 金山文档（WPS）
- 或用户系统默认应用（`xdg-open` / `open`）

这只是一个"上传到 X 并打开浏览器"或"用系统应用打开本地文件"的动作，不在 webview 内嵌入第三方服务。

---

## 架构适配

### 现有模式（follow it）

tcode-app 已有完整的文件查看架构：

```
show.ts          VIEWS 表：ext → { load, workspace, as }
                 决定"这个扩展名怎么加载、怎么画"

FileBody.tsx     switch(view.as) 分发到具体组件
                 sandbox / framed / paper / image / doc / table / text

inspect.ts       Inspect 类型：pane 里能展示什么
                 file / paper / workspace-file / shown / ...

PaperView.tsx    PDF 的先例：后端 serve_url → 前端渲染组件
Framed.tsx       HTML 报告的先例：loopback origin iframe

serve.rs         后端：为文件提供 loopback HTTP URL
```

**新增 xlsx/docx 要做的就是在这个表里加两个条目，然后写两个渲染组件。**

### 数据流

#### Excel (xlsx)

```
用户打开 .xlsx（workspace tree 或 show 工具）
  → show.ts: ext["xlsx","xls"] → { load: "served", workspace: "spreadsheet", as: { as: "spreadsheet" } }
  → inspect.ts: { kind: "spreadsheet", path }
  → SpreadsheetView.tsx:
      1. invoke("serve_url", { session, path }) → 拿到 loopback URL
      2. fetch(url) → ArrayBuffer
      3. @ironcalc/wasm Model.from_xlsx(bytes) → 加载到 WASM 引擎
      4. 用 @ironcalc/workbook React 组件渲染
      5. 编辑后 Model.to_xlsx() → invoke("write_file", ...) 回写
```

后端侧（Rust sidecar）：
- `ironcalc` crate 加入 tcode-app 的 Cargo.toml
- 工具侧可选：让 agent 在不打开 UI 的情况下提取/修改单元格值（通过 Rust crate 直接操作）

#### Word (docx) — 只读

```
用户打开 .docx
  → show.ts: ext["docx","doc"] → { load: "bytes", workspace: "document", as: { as: "document" } }
  → inspect.ts: { kind: "document", path }
  → DocumentView.tsx:
      1. 拿到文件的 ArrayBuffer（data: URL 或 serve）
      2. docx-preview renderAsync(arrayBuffer, container) → 渲染到 DOM
      3. 只读；编辑提示"用云文档打开"
```

#### 云文档打开

```
SpreadsheetView / DocumentView 的工具栏右侧按钮：
  "用外部应用打开" → invoke("open_external", { path })  // xdg-open / open
  "上传到云文档"    → 弹出选择菜单 → 打开对应服务的上传页面
```

"上传到云文档"第一版可以简单地用系统浏览器打开对应服务的首页/上传入口，不做 API 集成。

---

## 具体变更清单

### Phase 1: Excel 预览与编辑（IronCalc）

**前端（`crates/tcode-app/ui/`）**

1. **`package.json`** — 添加依赖：
   - `@ironcalc/wasm`
   - `@ironcalc/workbook`（如果它的 React 组件可用且够用）
   - 或者只用 `@ironcalc/wasm` + 自己写一个轻量 grid（视 workbook 组件的成熟度）

2. **`src/show.ts`** — VIEWS 表新增条目：
   ```ts
   { ext: ["xlsx", "xls"], load: "served", workspace: "spreadsheet", as: { as: "spreadsheet" } },
   ```

3. **`src/show.ts`** — `Shown` 类型新增：
   ```ts
   | { as: "spreadsheet" }
   ```

4. **`src/show.ts`** — `WorkspaceRoute` 新增 `"spreadsheet"` 分支

5. **`src/inspect.ts`** — `Inspect` 类型新增：
   ```ts
   | { kind: "spreadsheet"; path: string }
   ```

6. **`src/SpreadsheetView.tsx`** — 新建组件：
   - 加载 WASM：`import __wbg_init, { Model } from "@ironcalc/wasm"`
   - 从 loopback URL fetch xlsx bytes
   - `Model.from_xlsx(bytes)` 加载
   - 渲染：如果 `@ironcalc/workbook` 可用，直接用；否则自己画 grid（单元格值 + 基础格式）
   - 工具栏：sheet 切换 tab、保存按钮、"用外部应用打开"按钮
   - 保存：`Model.to_xlsx()` → invoke 写回

7. **`src/FileBody.tsx`** — switch 新增 `case "spreadsheet"` 分支

8. **`src/Panes.tsx`** — 处理 `inspect.kind === "spreadsheet"` 的渲染

**后端（`crates/tcode-app/src/`）**

9. **`Cargo.toml`** — 添加 `ironcalc` 依赖（可选，用于工具侧数据提取）

10. **`serve.rs`** — xlsx 已经是 `load: "served"`，现有 serve 机制应该直接可用（serve raw bytes）

11. **`commands.rs`** — 如果需要，添加 `write_file` 命令让前端回写编辑结果（或复用现有机制）

**工具侧（可选，Phase 1 之后）**

12. 在 `tcode-tools` 中利用 `ironcalc` crate，让 agent 能直接读取/修改 xlsx 单元格，不需要打开 UI

### Phase 2: Word 只读预览（docx-preview）

**前端**

1. **`package.json`** — 添加 `docx-preview`

2. **`src/show.ts`** — VIEWS 表新增：
   ```ts
   { ext: ["docx"], load: "bytes", workspace: "document", as: { as: "document" } },
   ```
   注意：`.doc`（旧格式）不支持，只支持 `.docx`。

3. **`src/show.ts`** — `Shown` 新增 `| { as: "document" }`

4. **`src/inspect.ts`** — `Inspect` 新增 `| { kind: "document"; path: string }`

5. **`src/DocumentView.tsx`** — 新建组件：
   - `docx-preview` 的 `renderAsync(arrayBuffer, container, options)` 渲染
   - 只读，工具栏只有"用外部应用打开"
   - 样式：白色纸面背景 + 居中，类似 PaperView 的视觉风格

6. **`src/FileBody.tsx`** / **`src/Panes.tsx`** — 同 Phase 1 加分支

### Phase 3: 云文档 / 外部应用打开

1. **`src/commands.rs`（后端）** — `open_external` 命令：
   ```rust
   // Linux: xdg-open <path>
   // macOS: open <path>
   // Windows: start <path>
   ```

2. **UI** — SpreadsheetView 和 DocumentView 的工具栏加"用外部应用打开"按钮
   - 点击 → `invoke("open_external", { path })`
   - 可选：小菜单列出已知的云文档服务入口链接

---

## 依赖评估

| 依赖 | 用途 | 大小（估） | 许可 |
|------|------|-----------|------|
| `ironcalc` (Rust crate) | 后端 xlsx 解析/回写 | 编译时，不影响包大小 | Apache-2.0 |
| `@ironcalc/wasm` | 前端 WASM 引擎 | ~2-3 MB | Apache-2.0 |
| `@ironcalc/workbook` | React 表格组件 | 需确认 | Apache-2.0 |
| `docx-preview` | docx → HTML 渲染 | ~200 KB | MIT |

## 风险与待确认

1. **`@ironcalc/workbook` 的成熟度**：它是否能直接嵌入 React 19 项目？如果 peer dep 冲突或组件不够稳定，退而求其次用 `@ironcalc/wasm` + 自写轻量 grid。这需要先 `npm install` 试一下。

2. **WASM xlsx I/O**：调研显示 `@ironcalc/wasm` 可能不含 xlsx 读写，需要后端 Rust crate 解析后序列化给前端。实际看一下 WASM 暴露的 API（`Model.from_xlsx` 是否存在）。

3. **大文件性能**：IronCalc WASM 加载一个 10MB xlsx 的体验如何？需要实测。如果太慢，考虑后端预处理（Rust crate 解析，只传可见区域数据给前端）。

4. **docx-preview 的样式隔离**：它往 DOM 里注入的 CSS 是否会影响 app 的其余部分？可能需要 shadow DOM 或 iframe 隔离。

## 实施顺序建议

1. 先装 `@ironcalc/wasm` 和 `@ironcalc/workbook`，在 UI 的 dev 环境里跑通一个最小 demo（加载 xlsx、显示、编辑、导出），确认可行性和 API
2. 接入 show.ts / inspect.ts / FileBody.tsx 的分发机制
3. 写 SpreadsheetView 组件
4. docx-preview 类似流程
5. 最后加"外部打开"按钮
