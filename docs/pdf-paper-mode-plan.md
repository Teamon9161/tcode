# PDF Paper Mode 计划

## 1. 目标

为 tcode 增加一个面向论文阅读的 `Paper Mode`，它不只是把 PDF 打开给用户看，而是把 PDF 页面、文字、标注、翻译、重点总结和问答上下文绑定起来。

核心体验是：

- 用户打开本地论文 PDF。
- PDF 出现在 tcode 的文档/浏览器工作区中，可缩放、可搜索、可翻页、可选择文字、可高亮和划线。
- tcode 现有聊天框仍然是唯一 AI 对话入口，不额外复制一个完整 AI 侧栏。
- PDF 面板负责产生结构化上下文，例如当前页、选区、高亮、框选区域和用户标注。
- 聊天框可以引用这些上下文完成翻译、解释、总结和追问。
- 用户的高亮、划线、提问、回答和笔记都绑定到 PDF 的页码、坐标和文本锚点，之后重新打开仍可恢复。

## 2. 结论先行

推荐路线：**不要自己写 PDF 渲染器，也不要只依赖浏览器内置 PDF viewer。**

最佳组合是：

- 前端 viewer：`PDF.js` 或基于 PDF.js 的 React viewer。
- 后端文本、坐标和页面图片提取：`PyMuPDF`。
- 论文结构化增强：按需要引入 `GROBID`、`Docling` 或 `marker`。
- 标注和会话存储：SQLite 起步，后续可迁移到 DuckDB/Postgres。
- 语义检索：SQLite + 向量扩展、LanceDB、Chroma 或 Qdrant，按部署复杂度选择。

浏览器内置 PDF viewer 可以用于快速预览，但不适合作为 Paper Mode 的核心，因为它对外暴露的结构化文本、坐标、标注和可访问性接口不足。

## 3. 为什么不能只用浏览器内置 PDF viewer

实测本地 PDF 通过 HTTP 提供后，Chromium 内置 PDF viewer 可以正常渲染页面，并带有缩略图、页码、缩放、下载、打印等基础能力。

但它有几个关键限制：

1. tcode 当前的 browser 工具不能直接打开 `file://` 本地 PDF，只能打开 `http/https`。
2. PDF 虽然可视化渲染成功，但浏览器 accessibility snapshot 为空，agent 不能可靠地从页面结构中读取 PDF 文本。
3. 内置 viewer 不是为外部程序可控的论文阅读工作流设计的，难以稳定拿到：
   - 用户选区对应的 PDF 坐标。
   - 文字块 bounding boxes。
   - 跨页 selection。
   - 标注对象。
   - 高亮和问答的持久化锚点。
4. 很难把翻译、重点、问答和用户画线稳定绑定起来。

因此，内置 viewer 适合“临时看一眼”，不适合作为可扩展的论文助手基础。

## 4. 总体架构

```text
┌────────────────────────────────────────────────────────────┐
│                         tcode Paper Mode                    │
├──────────────────────────────┬─────────────────────────────┤
│ Existing tcode Chat           │ PDF Document Workspace      │
│ - 用户自然提问                 │ - PDF.js 渲染                │
│ - 引用当前 PDF 上下文           │ - 翻页、缩放、搜索             │
│ - 翻译、解释、总结、追问          │ - text layer                 │
│ - 展示带页码和标注引用的回答      │ - highlight/underline/ink    │
│                              │ - 图表框选                    │
├──────────────────────────────┴─────────────────────────────┤
│                    Paper Context Bridge                     │
│ - 将当前页、选区、标注、框选区域注册为聊天可引用上下文             │
│ - 将聊天回答中的引用映射回 PDF 页面和标注                         │
│ - selection 到 PDF 坐标映射                                      │
│ - PDF 坐标到文本块映射                                           │
│ - annotation 持久化                                              │
│ - page/chunk/section 上下文组装                                  │
├────────────────────────────────────────────────────────────┤
│                    Document Processing Backend               │
│ - PyMuPDF 提取 text blocks 和 bbox                           │
│ - 页面渲染成图片                                             │
│ - 段落切分、章节识别                                         │
│ - 图表和表格区域识别，可选                                   │
│ - OCR，可选                                                  │
├────────────────────────────────────────────────────────────┤
│                       Knowledge Layer                        │
│ - document metadata                                           │
│ - pages, blocks, chunks                                       │
│ - annotations                                                 │
│ - conversations                                               │
│ - embeddings                                                  │
└────────────────────────────────────────────────────────────┘
```

关键原则：**tcode 已经有聊天框，Paper Mode 不应该再实现一个平行的 AI 聊天产品。PDF 面板负责可视化和上下文采集，现有聊天框负责 AI 交互。**


## 5. 推荐技术选型

### 5.1 前端 PDF viewer

优先级：

1. `PDF.js` / `pdfjs-dist`
2. `@react-pdf-viewer/core`
3. `react-pdf`
4. 商业 SDK，例如 Apryse WebViewer、Nutrient/PSPDFKit、Foxit PDF SDK

建议：如果 tcode 未来要深度定制“划线提问”“图表框选”“聊天上下文引用”，优先直接使用 PDF.js 或选择一个能暴露 PDF.js 底层能力的 viewer。

#### PDF.js 优点

- 开源成熟。
- 浏览器内运行。
- 支持 text layer。
- 支持 selection。
- 可做自定义 annotation layer。
- 可以做坐标转换。
- 社区和案例丰富。

#### PDF.js 缺点

- 需要自己实现较多产品级交互。
- 标注持久化、评论、协作和复杂 annotation 需要自己设计。
- PDF 里的复杂版面、公式、图表语义理解仍需要后端处理。

### 5.2 后端 PDF 处理

首选：`PyMuPDF`。

它适合承担：

- 提取每页文本。
- 提取文字块、行、span 及 bbox。
- 渲染页面图片。
- 读取元数据。
- 写入部分 PDF annotation。

可选增强：

- `pdfplumber`：更偏版面和表格提取。
- `pypdf`：轻量元数据和简单文本操作。
- `GROBID`：将学术论文解析成标题、作者、摘要、章节、引用等结构。
- `Docling` / `marker`：PDF 转结构化 Markdown。
- `ocrmypdf` + Tesseract 或 PaddleOCR：扫描版 PDF OCR。

### 5.3 存储

MVP 建议使用 SQLite。

理由：

- 本地工具友好。
- 部署简单。
- 易于备份和迁移。
- 足够支撑个人论文阅读库。

后续如果需要复杂分析、批量处理或多用户协作，可以迁移到：

- DuckDB：适合本地分析和大批量文档索引。
- Postgres：适合服务化和多用户。
- Qdrant / Chroma / LanceDB：适合向量检索。

### 5.4 AI 与检索

需要两类上下文：

1. 精确锚点上下文：用户当前选中的文字、页码、前后段落、所在章节。
2. 全文语义上下文：通过 embeddings 找到相关段落、公式、图表说明、引用附近文本。

MVP 可以先不做复杂 RAG：

- 用户选区提问：只传选区、当前页、前后若干段。
- 当前页重点：传当前页文本。
- 全文总结：传文档结构化 chunks 分批总结。

第二阶段再加 embeddings 和跨文档关联。

## 6. 核心功能设计

### 6.0 与 tcode 现有聊天框的边界

这是 Paper Mode 设计里最重要的产品边界。

tcode 已经有一个 AI 聊天框，所以 Paper Mode 不应该再做一个独立 AI 侧栏，否则会变成插件式产品，产生两套入口、两套会话和两套上下文状态。

推荐边界如下：

| 模块 | 负责什么 | 不负责什么 |
|---|---|---|
| tcode 现有聊天框 | 自然语言对话、翻译、解释、总结、追问、展示回答、维护会话历史 | PDF 页面渲染、鼠标选区、坐标转换 |
| PDF Document Workspace | PDF 渲染、翻页、缩放、搜索、选区、高亮、划线、框选图表、缩略图、outline | 维护第二套聊天历史、生成完整 AI 对话 UI |
| Paper Context Bridge | 把 PDF 中的当前页、选区、标注、框选区域转换成聊天可引用上下文 | 直接替代聊天框 |
| Document Processing Backend | 提取文本、bbox、章节、chunks、图片区域、OCR 和 embeddings | 用户交互 UI |

因此，所谓“翻译选区”不是在 PDF 旁边开一个新 AI 面板，而是：

1. 用户在 PDF 中选中文字。
2. PDF 面板产生一个 `PaperContextEvent`。
3. 现有聊天框收到事件，自动填入或执行“翻译这段”。
4. AI 回答出现在原本的聊天流里。
5. 回答中的页码、标注和引用可以反向跳回 PDF。

这样 Paper Mode 是 tcode 的文档上下文能力，而不是一个脱离 tcode 的 PDF 插件。

### 6.1 打开 PDF

用户可以通过以下方式打开论文：

- 本地文件路径。
- 拖拽 PDF 文件。
- tcode 命令引用当前目录的 PDF。

内部流程：

1. 将 PDF 登记到 document store。
2. 计算文件 hash，判断是否已经导入过。
3. 为前端 viewer 提供安全的本地 HTTP URL，或通过应用自己的文件接口提供 PDF bytes。
4. 后端异步启动解析任务。
5. 前端先展示 PDF，解析完成后启用智能交互。

注意：不要直接暴露整个目录。应该只暴露当前文档文件，最好使用短期 token URL。

### 6.2 文字选择后动作菜单

用户选中文字后，浮出动作菜单：

- 翻译
- 解释
- 总结
- 问这段
- 加高亮
- 加下划线
- 加入笔记
- 复制原文
- 复制双语

动作菜单需要拿到：

- document id
- page number
- selected text
- selection rects
- PDF coordinate rects
- surrounding context

### 6.3 划线和高亮提问

用户高亮或划线后，可以直接对该标注提问。

流程：

1. 用户选择文字或用画笔划线。
2. 前端得到 screen rects。
3. viewer 将 screen rects 转为 PDF page coordinates。
4. 根据 text layer 或后端 bbox 找到命中的文本。
5. 创建 annotation。
6. 用户输入问题。
7. 后端组装上下文：选区文本、前后段落、页码、章节、相关 chunks。
8. LLM 回答。
9. 将问答绑定到 annotation。

关键点：标注不能只存屏幕像素，必须存 PDF 坐标和文本锚点。

### 6.4 当前页重点

当前页重点应该由现有聊天框承接，而不是固定放在一个新的 AI 侧栏里。

交互方式可以是：

- PDF 面板显示一个轻量按钮，例如“总结本页”或“Explain this page”。
- 点击后，PDF 面板把 current document、page number、visible selection 和 page context 注入聊天框。
- 聊天框中生成当前页重点，并带可点击页码引用。
- 用户也可以直接在聊天框输入“总结当前页”“解释这一页的公式”。

当前页重点内容包括：

- 本页主要讲什么。
- 本页与全文主线的关系。
- 本页关键定义、假设和结论。
- 本页公式解释。
- 本页图表解释。
- 建议回看或跳转的相关页面。

MVP 中可以由当前页文本直接生成。

增强版中可以结合：

- 所在章节。
- 前后页。
- 文章摘要和 introduction。
- 用户历史高亮。
- RAG 检索到的相关段落。

### 6.5 论文地图 Paper Map

为每篇论文生成结构化导航：

```text
Paper Map
1. Problem
2. Motivation
3. Contributions
4. Related Work
5. Method
6. Key Equations
7. Experiments
8. Results
9. Limitations
10. My Highlights
11. My Questions
```

Paper Map 的作用：

- 快速理解论文结构。
- 从 AI 总结跳回 PDF 页面。
- 将用户问题和高亮组织起来。
- 为之后跨论文比较提供结构化入口。

### 6.6 翻译模式

建议提供三种翻译模式。

#### 选中翻译

用户选中一句或一段，触发浮动菜单或快捷键，把选区发送到现有聊天框并生成翻译。

优点：

- 最稳定。
- 成本低。
- 不破坏 PDF 排版。
- 适合精读。

#### 段落双语

在聊天框中以段落为单位显示原文和译文，PDF 面板只负责定位当前段落和跳转。

优点：

- 适合连续阅读。
- 比覆盖式 PDF 翻译更可靠。
- 可以保留公式、引用和术语。

#### 页面摘要翻译

不逐句翻译，而是解释当前页内容。

优点：

- 适合快速读论文。
- 能处理双栏和复杂图表。
- 比全文机器翻译更有阅读价值。

不建议 MVP 做 PDF 原页面覆盖式翻译，因为排版复杂且维护成本高。

### 6.7 图表框选提问

论文阅读里图表非常重要。建议作为第二阶段功能。

流程：

1. 用户框选图或表。
2. 前端记录 page 和 PDF rect。
3. 后端将该 rect 渲染成局部图片。
4. 同时提取附近 caption 和正文引用。
5. 将图片、caption、附近文本交给视觉模型或多模态模型。
6. 回答用户问题。

问题示例：

- 这个图说明了什么？
- 横轴纵轴分别是什么？
- 哪个方法最好？
- 这张表里的显著性是什么意思？
- 作者用这个图支持了什么结论？

## 7. 数据模型草案

### 7.1 Document

```ts
type Document = {
  id: string;
  filePath: string;
  fileHash: string;
  title?: string;
  authors?: string[];
  year?: number;
  pageCount: number;
  importedAt: string;
  lastOpenedAt?: string;
  processingStatus: "pending" | "processing" | "ready" | "failed";
};
```

### 7.2 Page

```ts
type Page = {
  id: string;
  documentId: string;
  pageNumber: number;
  width: number;
  height: number;
  rotation: number;
  text: string;
  thumbnailPath?: string;
};
```

### 7.3 TextBlock

```ts
type TextBlock = {
  id: string;
  documentId: string;
  pageNumber: number;
  blockIndex: number;
  kind: "title" | "paragraph" | "caption" | "equation" | "reference" | "unknown";
  text: string;
  bbox: [number, number, number, number];
  readingOrder: number;
};
```

### 7.4 Chunk

```ts
type Chunk = {
  id: string;
  documentId: string;
  sectionId?: string;
  pageStart: number;
  pageEnd: number;
  text: string;
  sourceBlockIds: string[];
  embeddingId?: string;
};
```

### 7.5 Annotation

```ts
type Annotation = {
  id: string;
  documentId: string;
  pageNumber: number;
  type: "highlight" | "underline" | "ink" | "box" | "note" | "question";
  pdfRects: Array<[number, number, number, number]>;
  selectedText?: string;
  textAnchor?: {
    pageNumber: number;
    startOffset?: number;
    endOffset?: number;
    prefix?: string;
    exact?: string;
    suffix?: string;
  };
  color?: string;
  comment?: string;
  createdAt: string;
  updatedAt: string;
};
```

### 7.6 Conversation

```ts
type Conversation = {
  id: string;
  documentId: string;
  annotationId?: string;
  pageNumber?: number;
  selectedText?: string;
  title?: string;
  createdAt: string;
  updatedAt: string;
};
```

### 7.7 Message

```ts
type Message = {
  id: string;
  conversationId: string;
  role: "user" | "assistant" | "system";
  content: string;
  citations?: Array<{
    documentId: string;
    pageNumber: number;
    textBlockId?: string;
    quote?: string;
    pdfRect?: [number, number, number, number];
  }>;
  createdAt: string;
};
```

## 8. 坐标与锚点设计

这是 Paper Mode 的关键。

不要只存 DOM selection，也不要只存屏幕像素。推荐同时存：

1. PDF 坐标 rects。
2. selected text。
3. 文本锚点。
4. page number。
5. 文件 hash。

### 8.1 坐标系统

前端会遇到至少三套坐标：

- viewport/screen 坐标：鼠标事件和页面显示使用。
- PDF.js viewport 坐标：当前缩放和旋转后的页面坐标。
- PDF 原始坐标：不随缩放变化，适合持久化。

保存时应转换到 PDF 原始坐标。显示时再从 PDF 原始坐标转换回当前 viewport。

### 8.2 文本锚点

文本锚点用于坐标失效或 PDF 版本轻微变化时恢复定位。

推荐采用类似 Web Annotation TextQuoteSelector 的结构：

```json
{
  "prefix": "the paragraph before the selected text",
  "exact": "selected text",
  "suffix": "the paragraph after the selected text"
}
```

对于论文 PDF，还可以加：

- page number
- block id
- reading order
- section title

## 9. 后端处理流程

### 9.1 导入任务

```text
Input PDF
  ├─ compute hash
  ├─ read metadata
  ├─ count pages
  ├─ extract page size
  ├─ extract text blocks and bbox
  ├─ detect sections
  ├─ build chunks
  ├─ generate thumbnails
  ├─ optional: extract figures/tables
  ├─ optional: OCR scanned pages
  └─ optional: build embeddings
```

### 9.2 PyMuPDF 提取示例

```python
import fitz

def extract_pdf(path: str):
    doc = fitz.open(path)
    result = []

    for page_index, page in enumerate(doc):
        page_dict = page.get_text("dict")
        blocks = []

        for block_index, block in enumerate(page_dict.get("blocks", [])):
            if block.get("type") != 0:
                continue

            lines_text = []
            for line in block.get("lines", []):
                for span in line.get("spans", []):
                    lines_text.append(span.get("text", ""))

            text = "".join(lines_text).strip()
            if not text:
                continue

            blocks.append({
                "blockIndex": block_index,
                "bbox": block.get("bbox"),
                "text": text,
            })

        result.append({
            "pageNumber": page_index + 1,
            "width": page.rect.width,
            "height": page.rect.height,
            "text": page.get_text("text"),
            "blocks": blocks,
        })

    return result
```

实际实现需要额外处理：

- 双栏阅读顺序。
- 页眉页脚去除。
- 连字符换行合并。
- 参考文献区域识别。
- 公式和图表 caption。
- block 类型分类。

## 10. AI 上下文组装

### 10.1 选区提问

输入：

- selected text
- page number
- previous text block
- next text block
- section title
- paper abstract
- user question

提示词目标：

- 先解释选区本身。
- 再说明它在论文中的作用。
- 如果涉及术语，给出术语解释。
- 如果涉及公式，逐项解释变量。
- 如果回答依赖上下文，要引用页码或段落。

### 10.2 当前页重点

输入：

- current page text
- previous page summary
- next page summary，可选
- section title
- paper map

输出：

- 本页一句话概括。
- 关键点 3 到 7 条。
- 需要注意的公式、图表、假设。
- 本页和全文问题的关系。

### 10.3 全文问答

输入：

- user question
- top-k retrieved chunks
- paper map
- citations

输出要求：

- 回答必须引用页码。
- 不确定时说明缺少证据。
- 不要编造论文中没有的结果。
- 对方法、实验、结论分开说明。

## 11. UI 草案

### 11.1 主布局

Paper Mode 应该嵌入 tcode 现有界面，而不是做成独立插件式阅读器。

```text
┌────────────────────────────────────────────────────────────────────┐
│ Existing tcode Chat                                                │
│ 用户可以直接问：翻译选区、总结当前页、解释刚才框选的图              │
│ 回答里带 page citation，点击引用可以跳转 PDF 页面或标注              │
├────────────────────────────────────────────────────────────────────┤
│ PDF Document Workspace                                             │
│ Top Bar: file title | search | page | zoom | Paper Map | settings   │
│                                                                    │
│ [page 1 rendered by PDF.js]                                        │
│ [page 2 rendered by PDF.js]                                        │
│                                                                    │
│ Floating selection menu: Translate | Explain | Ask | Highlight      │
│ Optional local panel: thumbnails | outline | highlights list        │
└────────────────────────────────────────────────────────────────────┘
```

这里的关键是：聊天框不搬家、不复制。PDF 工作区只提供文档操作、选区操作、标注列表、缩略图、outline 等 PDF-native UI。

### 11.2 选区菜单

```text
┌──────────────────────────────────────────────┐
│ Translate | Explain | Ask in Chat | Mark     │
└──────────────────────────────────────────────┘
```

选区菜单不直接变成一个新的问答窗口。它的动作应该生成聊天上下文事件，例如：

```ts
type PaperContextEvent = {
  kind: "selection" | "page" | "annotation" | "region";
  documentId: string;
  pageNumber?: number;
  selectedText?: string;
  pdfRects?: Array<[number, number, number, number]>;
  suggestedAction?: "translate" | "explain" | "ask" | "summarize";
};
```

聊天框收到事件后，可以自动填入一条可编辑 prompt，或者直接执行用户点击的动作。

### 11.3 PDF 工作区局部面板

PDF 工作区可以有局部面板，但它们不承担聊天职责：

- Thumbnails：缩略图跳页。
- Outline：章节导航。
- Highlights：当前文档的高亮列表。
- Notes：用户自己的短笔记。
- Paper Map：论文结构导航。

这些面板的目标是“定位和管理文档对象”，不是复制 AI conversation。

## 12. MVP 范围

第一版只做最能改变论文阅读体验的部分。

### 必做

1. 打开本地 PDF。
2. PDF.js 页面渲染、翻页、缩放。
3. 用户选择文字。
4. 选区动作：翻译、解释、提问，并接入 tcode 现有聊天框。
5. 高亮保存和恢复。
6. Paper Context Bridge：把当前页、选区、标注和框选区域注册为聊天可引用上下文。
7. PyMuPDF 后端提取每页文本和 bbox。
8. SQLite 保存 document、page、annotation、conversation、message。
9. 当前页重点总结。

### 暂不做

1. PDF 原文覆盖式全文翻译。
2. 多人协作。
3. 写回原 PDF 文件。
4. 复杂表格抽取。
5. 扫描版 OCR。
6. 跨论文知识库。
7. 移动端适配。

## 13. 实施阶段

### Phase 1：可控打开和基础 viewer

目标：替代浏览器内置 PDF viewer，拥有可控的 PDF.js viewer。

任务：

- 建立 Paper Mode 前端页面。
- 接入 PDF.js。
- 支持本地 PDF 安全加载。
- 支持页码、缩放、滚动和搜索。
- 提供 document id 和 page number 状态。

验收：

- 能打开本地 PDF。
- PDF 渲染稳定。
- 切换缩放后页面正常。
- 能知道当前页。

### Phase 2：文本选择和坐标绑定

目标：用户选中文字后，系统能知道选中了哪段文本和它在 PDF 中的位置。

任务：

- 启用 PDF.js text layer。
- 捕获 selection。
- 计算 selection rects。
- 将 screen rects 转换为 PDF 坐标。
- 建立 selected text、page number、pdfRects 的数据结构。

验收：

- 用户选择一句话后，PDF 面板能产生准确的 PaperContextEvent，聊天框能引用该原文。
- 缩放后同一高亮位置仍能正确重绘。
- 跨行 selection 能保存多个 rect。

### Phase 3：后端 PDF 解析

目标：后端能提取每页文本、文本块和 bbox，为 AI 上下文提供可靠来源。

任务：

- 引入 PyMuPDF。
- 建立 import document pipeline。
- 提取 page text、text blocks、bbox、page size。
- 写入 SQLite。
- 提供按 page、block、rect 查询文本的 API。

验收：

- 导入 PDF 后能查询每页文本。
- 能根据 page 和 rect 找到附近文本块。
- 双栏论文的基本 reading order 可用。

### Phase 4：聊天上下文桥接 MVP

目标：选区翻译、解释和提问通过 tcode 现有聊天框完成，而不是新增一个平行 AI 侧栏。

任务：

- 建立 PaperContextEvent 协议。
- 实现 Translate、Explain、Ask actions，将选区、页码和 PDF 坐标注入聊天框。
- 组装选区、前后文、页码和章节上下文。
- 保存 conversation 和 messages，并记录它们关联的 document、page、annotation。
- 回答中带 page citation，点击引用可以跳回 PDF 页面。

验收：

- 选中文字后可一键把“翻译这段”发送到现有聊天框。
- 可以在现有聊天框针对选区追问。
- 问答历史绑定到当前 PDF，但 UI 上不出现第二套聊天系统。

### Phase 5：高亮、划线和笔记

目标：用户能将理解过程沉淀为可恢复的标注。

任务：

- 实现 highlight annotation layer。
- 实现 underline annotation layer。
- 存储 pdfRects、selectedText、textAnchor、color、comment。
- 重新打开 PDF 时恢复标注。
- 标注点击后打开相关问答和笔记。

验收：

- 高亮在刷新后仍存在。
- 缩放后高亮位置正确。
- 点击高亮能看到当时的问题、回答或笔记。

### Phase 6：当前页重点和 Paper Map

目标：从“问选区”扩展到“理解页面和整篇论文”。

任务：

- 实现当前页重点总结。
- 提取或生成论文标题、摘要、章节结构。
- 生成 Paper Map。
- 从 Paper Map 跳转到 PDF 页面。
- 将用户高亮和问题纳入 Paper Map。

验收：

- 每页能生成可读重点。
- Paper Map 能帮助快速定位方法、实验、结论、限制。
- 用户问题和高亮可按章节聚合。

### Phase 7：图表框选提问

目标：支持对论文图表和公式区域提问。

任务：

- 实现区域框选工具。
- 将框选区域转为 PDF rect。
- 后端渲染该区域为图片。
- 提取附近 caption 和正文引用。
- 将图片和文本上下文发送给多模态模型。

验收：

- 框选图表后可以问“这张图说明什么”。
- 回答能结合 caption 和正文解释图表作用。

### Phase 8：检索和跨页问答

目标：支持全文语义问答，而不只回答当前选区。

任务：

- 建立 chunking 策略。
- 生成 embeddings。
- 选择向量存储。
- 实现 top-k 检索。
- 回答中引用页码和原文片段。

验收：

- 用户能问“这篇论文的核心贡献是什么”。
- 用户能问“实验结果支持了哪些结论”。
- 回答带引用，不编造。

## 14. 风险和难点

### 14.1 PDF 文字顺序

论文常见双栏排版，PDF 提取出来的文本顺序可能错乱。

缓解：

- 使用 bbox 和列检测重建 reading order。
- 对常见双栏页面按 x 坐标分栏。
- 对标题、摘要、正文、参考文献使用规则和模型混合分类。

### 14.2 公式理解

PDF 里的公式经常不是结构化 LaTeX。

缓解：

- MVP 先把公式作为附近图片或文本片段解释。
- 后续引入公式 OCR 或 LaTeX 识别。
- 对公式提问时传页面截图局部图。

### 14.3 图表理解

图表的语义通常在 caption 和正文引用里，不只在图片里。

缓解：

- 框选图片时同时抓取附近 caption。
- 检索正文中 “Figure 1”、“Table 2” 等引用。
- 多模态模型只作为补充，文本上下文仍然重要。

### 14.4 标注恢复

PDF 缩放、旋转、版本变化都可能影响标注位置。

缓解：

- 用 PDF 原始坐标保存 rect。
- 同时保存 TextQuote anchor。
- 使用 file hash 区分版本。

### 14.5 OCR 和扫描版

扫描版 PDF 没有 text layer。

缓解：

- MVP 检测是否有文本，无文本则提示需要 OCR。
- 第二阶段支持 OCR pipeline。

## 15. 自研与商业 SDK 对比

### 自研：PDF.js + PyMuPDF

优点：

- 灵活。
- 开源。
- 能深度适配 tcode 和 AI 工作流。
- 本地优先，容易保护用户文件。
- 成本低。

缺点：

- 标注系统、局部 PDF 面板、存储、坐标映射都要自己做。
- 产品级细节需要打磨。
- 跨浏览器和复杂 PDF 需要测试。

### 商业 SDK：Apryse / Nutrient / Foxit

优点：

- PDF 标注、评论、搜索、表单、写回能力成熟。
- 移动端和复杂 PDF 支持好。
- 产品级稳定性高。

缺点：

- 授权成本。
- AI 工作流仍需自己实现。
- 深度定制可能被 SDK 约束。
- 对个人或内部工具可能过重。

建议：

- 如果目标是 tcode 自用或内部研究工具，选 PDF.js + PyMuPDF。
- 如果目标是商业协作型 PDF 产品，再评估 Apryse/Nutrient。

## 16. 最小技术原型建议

可以先做一个独立 prototype，而不是直接塞进 tcode 主流程。

目录草案：

```text
paper-mode-prototype/
  frontend/
    src/
      PdfViewer.tsx
      AnnotationLayer.tsx
      AiSidebar.tsx
      selection.ts
      coordinates.ts
  backend/
    app.py
    extract_pdf.py
    database.py
    ai.py
  data/
    paper_mode.sqlite
```

原型技术栈：

- Frontend：Vite + React + PDF.js。
- Backend：FastAPI + PyMuPDF + SQLite。
- API：REST 或 WebSocket。

API 草案：

```http
POST /documents/import
GET  /documents/{id}
GET  /documents/{id}/file
GET  /documents/{id}/pages/{pageNumber}
POST /documents/{id}/annotations
GET  /documents/{id}/annotations
POST /documents/{id}/ai/translate
POST /documents/{id}/ai/explain
POST /documents/{id}/ai/ask
POST /documents/{id}/ai/page-summary
```

## 17. 推荐优先级

如果按投入产出排序：

1. PDF.js viewer 打开本地 PDF。
2. selection 捕获和选区翻译。
3. PyMuPDF 提取 page text 和 bbox。
4. 高亮保存和恢复。
5. 选区问答。
6. 当前页重点。
7. Paper Map。
8. 图表框选提问。
9. embeddings 全文检索。
10. OCR。
11. 写回 PDF annotation。

## 18. 最终推荐

建议先走 **PDF.js + PyMuPDF + SQLite + Paper Context Bridge**。

不要从零写 PDF 渲染器；不要把浏览器内置 PDF viewer 当核心；也暂时不需要商业 SDK。

先实现一个能做到以下事情的 MVP：

- 打开本地 PDF。
- 选中文字。
- 一键翻译和解释。
- 对选区追问。
- 保存高亮。
- 当前页重点。

这会立刻覆盖论文阅读中最常见、最有价值的场景，并且为后续图表提问、Paper Map、全文检索和跨论文知识库打好基础。


## 19. 结合 tcode-app 现状后的修正版接入方案

这一节基于 `C:\code\rust\tcode\crates\tcode-app` 的现有实现修正前面的方案。核心结论是：**Paper Mode 应该成为 tcode 现有 pane/inspect/workspace 体系里的一个文档检视能力，而不是新增一个独立插件式应用，也不应该放进现有 native WebPane。**

### 19.1 tcode-app 当前架构事实

`tcode-app` 不是一个普通 web app，而是 Electron 桌面前端：

- Rust sidecar 后端在 `crates/tcode-app/src/`。
- Electron 壳在 `crates/tcode-app/electron/`。
- React + Vite 前端在 `crates/tcode-app/ui/src/`。
- 它不在 workspace build 里，需要从 `crates/tcode-app` 单独构建和运行。

当前 UI 的核心结构是：

- `Workspace.tsx`：唯一主屏幕，左边 rail，右侧是 tiled panes。
- `layout.ts`：pane tree 的纯数据模型。
- `Panes.tsx`：把 pane tree 渲染成一层绝对定位的 panes。
- `SessionPane`：现有聊天转录、审批、队列、composer 都在这里。
- `InspectView`：文件、diff、run、artifact、shown、workspace file、plan 等“可检视对象”的统一入口。
- `WebPane`：窗口级 native browser，不属于任何 session。
- `TermPane`：窗口级 terminal，不属于任何 session。

`layout.ts` 里的 pane union 当前是：

```ts
type Pane =
  | { kind: "session"; session: string }
  | { kind: "inspect"; session: string; nav: Nav }
  | { kind: "web" }
  | { kind: "terminal" };
```

这对 Paper Mode 很关键：tcode 已经有一套成熟的“把一个东西打开到 pane 里”的机制，不需要新增一个应用级侧栏或插件 shell。

### 19.2 为什么不应该把 Paper Mode 放进现有 WebPane

`WebPane.tsx` 只画浏览器 chrome。真正网页是 Electron main process 里的 native `WebContentsView`，不是 React DOM 的一部分。

这带来几个限制：

1. React 不能直接在网页内容上覆盖 selection menu、annotation layer、PDF text layer 或自定义高亮。
2. `WebPane` 是窗口级单例，不带 session；而 Paper Mode 的上下文需要能明确注入某个会话的 composer 和 transcript。
3. 浏览器 tab 是面向普通网页、dev server、登录站点和 agent browser 的能力，不是文档对象管理能力。
4. `browser` tool 明确拒绝 `file://` 和 tcode 自己的 viewer origin，避免模型把浏览器变成绕过 `read/show` 边界的文件读取器。
5. 浏览器内置 PDF viewer 即使能渲染 PDF，也不给 tcode 稳定的文本层、坐标、标注和上下文桥接接口。

所以：**WebPane 可以继续用于网页和临时预览，不应该成为 Paper Mode 的技术底座。**

### 19.3 Paper Mode 应该挂在哪里

最贴合现状的方案是新增一个 session-scoped inspect value，例如：

```ts
type Inspect =
  | ExistingInspectValues
  | { kind: "paper"; path: string; documentId?: string };
```

然后在 `Inspector.tsx` 里增加：

```tsx
case "paper":
  return <PaperView path={value.path} />;
```

它应该像 `workspace-file` 一样是一个 inspect pane，而不是像 `web` 一样是窗口级 singleton。

原因：

- Paper Mode 需要把“当前 PDF、当前页、选区、标注”注入某个 session 的 composer。
- `Inspect` pane 已经天然带 `session`，可以安全知道目标会话是谁。
- 关闭一个 session 时，相关 PDF inspect pane 跟着关闭，这和 workspace file、diff、shown artifact 的行为一致。
- 如果用户想同时看两篇论文或同一论文的两个位置，tcode 已经用 split panes 表达，不需要给 paper view 再做 tab strip。
- `Inspect` 的设计原则就是“一个 pane 一个值，不是 tab 容器”；Paper Mode 应该遵守这一点。

### 19.4 打开入口应该复用现有 workspace/show 路线

当前有两条和文件显示相关的入口：

1. 工作区文件树：`WorkspaceFiles` 选择文件后打开 `{ kind: "workspace-file", path }`。
2. `show` 工具：模型显示产物后打开 `{ kind: "shown", path, label }`。

现在 `.pdf` 在 `show.ts` 里没有专门条目，因此：

- `show` 出来的 PDF 会落到默认 text 路线，不合适。
- workspace tree 点开的 PDF 也会按普通 UTF-8 editor 路线处理，不合适。

修正方案：

- 在 `show.ts` 的扩展名注册表里加入 `.pdf` 的语义，但不要简单归到现有 `framed`。
- workspace route 对 `.pdf` 返回一个新的 route，例如 `{ load: "served", as: "paper" }` 或直接通过 `Inspect` 打开 `{ kind: "paper" }`。
- `show` 工具显示 `.pdf` 时也应打开 `paper` inspect，而不是普通 `shown`。

这保持了 tcode 的注册表风格：新增文件类型 = 修改扩展名事实表和新增 renderer，而不是在各组件里写分散的 `if path.endsWith(".pdf")`。

### 19.5 文件 bytes 应该复用 serve.rs，但要补 PDF.js 访问方式

`serve.rs` 已经提供了一个很接近 Paper Mode 所需的能力：

- 绑定 `127.0.0.1:0`。
- 每个 root 一个不可猜 token。
- 边界复用 `tcode_tools::viewable_within`。
- 底层用 `tower_http::ServeDir`，天然支持 Range、HEAD、MIME、条件请求。

这对 PDF 很重要，因为 PDF viewer 经常需要 Range request。

但是 Paper Mode 如果用 PDF.js 在 app renderer 里解析 PDF，就会遇到一个和 `Framed.tsx` 不同的问题：

- `Framed.tsx` 是 `<iframe src="http://127.0.0.1:port/token/file.html">`，页面自己加载自己，跨源没问题。
- PDF.js 通常需要由 app renderer fetch PDF bytes；从 `app://tcode` 或 Electron app origin 去 fetch `127.0.0.1` 可能需要 CORS。

所以这里有三种技术路线：

#### 路线 A：iframe 打开 PDF，使用 Chromium 内置 PDF viewer

优点：

- 最少代码。
- `serve.rs` 基本可直接用。
- Range/MIME 都由 `ServeDir` 处理。

缺点：

- 仍然拿不到稳定 text layer、坐标和 annotation layer。
- 画线提问、选区绑定和 AI 上下文仍然困难。

结论：只适合临时 preview，不适合 Paper Mode 核心。

#### 路线 B：PDF.js fetch `serve_url`

优点：

- 保留 PDF.js 的 text layer、viewport、selection、annotation layer。
- PDF bytes 仍然走现有安全文件服务。

需要补：

- `serve.rs` 对 app renderer origin 增加受限 CORS，或者提供一个专门给 app renderer 的 PDF byte/range IPC。
- 保持 token + workspace boundary，不扩大文件读取范围。

结论：这是比较适合 MVP 的路线。

#### 路线 C：PDF.js 通过 IPC range transport 读 bytes

优点：

- 不需要 CORS。
- 文件读取权限完全在 sidecar command 里。
- 可以做精细的 range、缓存、权限和错误提示。

缺点：

- 代码量比路线 B 大。
- 需要实现 PDF.js custom range transport 或类似机制。

结论：适合后续 hardening，不一定适合第一版。

MVP 推荐路线 B：**先复用 `serve.rs`，给 PDF.js 一个安全可 fetch 的 URL；必要时再演进到 IPC range transport。**

### 19.6 PDF.js 组件应该放在 React pane 内

建议新增前端组件：

```text
ui/src/PaperView.tsx
ui/src/paper/types.ts
ui/src/paper/coordinates.ts
ui/src/paper/selection.ts
ui/src/paper/annotations.ts
ui/src/paper/context.ts
```

`PaperView` 的职责：

- 加载 PDF.js。
- 渲染 page canvas。
- 渲染 text layer。
- 渲染 annotation layer。
- 维护当前页、缩放、搜索状态。
- 捕获 selection。
- 把 selection 转换成 PDF 坐标。
- 产生 PaperContextEvent。

它不负责完整 AI 对话 UI。AI 对话仍然在现有 `SessionPane` 的 composer/transcript 中完成。

### 19.7 聊天桥接应该接入 composer draft，而不是新增聊天侧栏

`tcode-app` 现在发送消息的路径是：

- `Composer.tsx` 持有输入。
- `Panes.tsx` 的 `PaneContext.onSend` 调 `App.tsx` 的 `send`。
- `App.tsx` 调后端 `send_message`。
- `commands.rs::send_message` 组装 `ContentBlock` 并交给 session turn。

Paper Mode 的“翻译这段”“解释这页”“问这个图”应该复用这个路径。

建议新增一个前端上下文注入函数，语义类似现有 workspace tree 的 `mention`：

```ts
type PaperContextEvent = {
  kind: "selection" | "page" | "annotation" | "region";
  documentId: string;
  path: string;
  pageNumber?: number;
  selectedText?: string;
  pdfRects?: Array<[number, number, number, number]>;
  suggestedAction?: "translate" | "explain" | "ask" | "summarize";
};
```

MVP 可以先把它转成可编辑 draft 文本，例如：

```text
Translate the selected text in @paper("End_to_End_Cross_Asset_Futures_Timing_AI_2026.pdf", page 3, selection sel_12).
```

更好的第二阶段是让 `send_message` 支持结构化附件或上下文块，而不是把所有上下文编码进自然语言字符串。

### 19.8 后端 PDF 解析不应该塞进 Electron main.js

`tcode-app` 的规则很明确：Electron shell 只负责窗口、IPC 转发、native browser view，不放业务逻辑。

因此 PDF 解析应该放在 Rust sidecar 或独立 helper 中，而不是 `electron/main.js`。

可选路线：

1. Rust 内直接集成 PDF 解析库。
2. Rust sidecar 调一个 Python helper，用 PyMuPDF 解析。
3. 先只用前端 PDF.js 做页面 text layer，后端解析放到第二阶段。

结合 tcode 是 Rust 项目，长期更稳的是 Rust-side API；但 PDF 生态里 PyMuPDF 的实用性很高。MVP 可以先走：

- 前端 PDF.js 负责 selection 和 text layer。
- Rust sidecar 只负责安全 serving 和 annotation/context 存储。
- 第二阶段再引入 PyMuPDF 或其他解析服务做更强的 page text、bbox、OCR、图表裁剪。

### 19.9 标注和阅读状态应该按 document 存，但 UI 入口按 session 走

`tcode-app` 的 inspect pane 带 session，这是 UI 和聊天上下文的归属；但 PDF 的标注和阅读状态更像文档状态。

建议区分：

- UI session ownership：哪个会话打开了这个 paper pane，PaperContextEvent 注入哪个 composer。
- Document identity：由 file hash 或 canonical path 标识，用于恢复高亮、笔记、阅读位置。
- Conversation link：某条聊天消息可以引用某个 document/page/annotation。

MVP 存储可以先简单：

```ts
type PaperDocument = {
  id: string;
  path: string;
  fileHash: string;
  pageCount: number;
  title?: string;
};

type PaperAnnotation = {
  id: string;
  documentId: string;
  pageNumber: number;
  pdfRects: Array<[number, number, number, number]>;
  selectedText?: string;
  color?: string;
  note?: string;
};

type PaperChatLink = {
  sessionId: string;
  messageId?: string;
  documentId: string;
  annotationId?: string;
  pageNumber?: number;
};
```

### 19.10 具体文件级改动草案

MVP 可能涉及这些文件：

#### 前端

- `ui/src/show.ts`
  - 让 `.pdf` 成为已知文档类型。
  - workspace route 不再把 PDF 当普通 editor text。

- `ui/src/inspect.ts`
  - 增加 `{ kind: "paper"; path: string; documentId?: string }`。
  - `inspectTitle` 对 PDF 显示文件名或论文标题。

- `ui/src/Inspector.tsx`
  - 分发 `paper` 到 `PaperView`。

- `ui/src/PaperView.tsx`
  - 新增 PDF.js viewer。
  - 负责页面渲染、selection、高亮和 PaperContextEvent。

- `ui/src/Panes.tsx`
  - 给 `PaneContext` 增加 `onPaperContext` 或类似回调。
  - 把 paper view 的 selection action 写入对应 session draft。

- `ui/src/WorkspaceFiles.tsx`
  - 点击 PDF 时打开 `{ kind: "paper" }`，不是 `{ kind: "workspace-file" }`。

- `ui/src/preview/Preview.tsx`
  - 增加 paper scene，符合 tcode-app “改界面先开 preview:ui” 的现有验证方式。

- `ui/src/app.css` 和 theme token
  - 新增样式只能用 token，不能写字面颜色、圆角、字号。
  - 响应式规则放在文件末尾对应 media query 区域。

#### 后端 Rust sidecar

- `src/commands.rs`
  - 可能新增 `paper_document`、`paper_annotations`、`paper_save_annotation` 等 command。
  - 如果 PDF.js 需要 URL，复用 `serve_url` 或新增更明确的 `paper_url`。

- `src/serve.rs`
  - 若采用 PDF.js fetch URL，补受限 CORS 或专门 PDF response header。
  - 保持 `viewable_within` boundary，不新增第三套路径判断。

- `src/dispatch.rs`
  - 注册新增 commands。

#### 测试

- `ui/src/show.test.ts`
  - PDF extension route。

- `ui/src/layout.test.ts`
  - 如新增 pane kind 才需要；如果只新增 inspect kind，通常不需要改 pane union。

- `ui/src/Panes.test.tsx` 或 `PaperView.test.tsx`
  - selection event 到 composer draft 的桥接。

- `src/commands.rs` tests 或独立 Rust tests
  - PDF URL/path boundary。
  - annotation 存取。

### 19.11 修正后的 MVP 顺序

结合现状，MVP 顺序应该调整为：

1. **让 PDF 成为 workspace inspect object**
   - `.pdf` 从 workspace tree 打开为 `{ kind: "paper" }`。
   - 不走 `workspace-file` editor，不走 `WebPane`。

2. **在 React pane 内渲染 PDF.js**
   - 先实现翻页、缩放、当前页。
   - PDF bytes 先复用 `serve.rs`。

3. **选区到 composer draft**
   - 选中文字后出现轻量 selection menu。
   - 点击 Translate/Explain/Ask，把 prompt 写入当前 session composer。
   - 不新增 AI 侧栏。

4. **高亮和恢复**
   - 存 page、pdfRects、selectedText。
   - 重开 PDF 后恢复。

5. **结构化聊天上下文**
   - 从“把上下文写成自然语言 draft”升级为真正的 structured context。
   - 后端 `send_message` 可以接收 paper context blocks，模型看到的是明确的引用和原文。

6. **后端解析和更强问答**
   - 引入 PyMuPDF 或 Rust/Python helper。
   - 做 page text、bbox、chunk、figure crop、OCR、embeddings。

### 19.12 当前计划中需要避免的旧假设

前面章节里凡是提到“右侧 AI 侧栏”的旧想法都应理解为已经废弃。tcode-app 已经有聊天框和 pane system，正确方向是：

- 不新增第二套聊天。
- 不把 Paper Mode 做成 WebPane 里的网页插件。
- 不给 inspect pane 加 tab strip。
- 不让 Electron main.js 承担 PDF 业务逻辑。
- 不绕过已有 workspace path boundary。
- 不把 PDF 当普通 text file/editor 打开。

最终修正建议：**`Inspect(kind: "paper") + React/PDF.js PaperView + serve.rs 安全文件 URL + PaperContextEvent 注入现有 Composer`。**
