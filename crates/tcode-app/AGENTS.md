# tcode-app — 硬规则

Tauri 桌面前端：Rust 后端（本 crate）+ webview 前端（`ui/`，Vite + React + TS）。后端是持有一个 `Arc<Agent>` 与多个隔离 `Session` 的 supervisor，事件经 Tauri emit 推给 webview。

## 构建与运行

**不在 workspace 里**（理由同 `tcode-voiced`：Tauri 链接平台 webview，Linux 上需要 webkit2gtk + libsoup，`cargo build --workspace` 不能开始要求所有人装这些）。所以命令都在本目录跑：

```bash
cd crates/tcode-app
(cd ui && npm install && npm run build)   # 首次 / 改过前端
(cd ui && npm test)                       # 渲染边界测试（规则 10/11），不碰网络
cargo build && cargo test                 # 后端 + 集成测试
./target/debug/tcode-app                  # 起 app（把 cwd 作为第一个会话）

(cd ui && npm run preview:ui)             # 设计预览：浏览器里看全部界面状态
```

`npm run build` 与 `preview:ui` 都会先跑 `build:sandbox`（三个 IIFE 进 `public/`，被 gitignore）。**只改了 `src/sandbox/` 下的文件时，dev server 不会热更它们**——那是静态产物，得重跑 `npm run build:sandbox` 再刷新。

**改界面先开 `npm run preview:ui`。** 它用 `PREVIEW=1` 把 `@tauri-apps/api/*` 与 dialog 插件别名到 `ui/src/preview/` 下的 fixture，然后加载 `preview.html`，把**真实组件**（不是另画一套 mock）按 launchpad / session / approval / question / plan / split / shown / empty 这些场景摆出来。没有它，"跑起来的会话正在等审批"这类状态要复现一次得起真 provider 打真 API。别把 mock 引进 `main.tsx` 那条路径——别名只在 `PREVIEW=1` 下生效，发布产物里没有它们。

**`tauri.conf.json` 里刻意不配 `devUrl`。** Tauri 在 debug 构建下只要看见 `devUrl` 就去连它，于是 `cargo run` 会撞上 "Connection refused"——而 `cargo run` 正是这里的主流程。不配它，debug 与 release 一样加载 `frontendDist`（`ui/dist`），代价是改前端要重跑一次 `npm run build`。想要 HMR 就临时加回 `devUrl: "http://localhost:5173"` 并同时起 `npm run dev`，别把它留在提交里。

## 不可违背

1. **装配逻辑不在这里重写**。config 加载、`Arc<Agent>` 组装、开会话全部走 `tcode-frontend`（`boot` / `open_session`）。`src/boot.rs` 只放 app 独有的决定（开哪个文件夹、没配置 provider 时报错而不是画向导——这里没有终端可画）。发现自己在抄 `src/main.rs` 的段落时，那段就该下沉到 `tcode-frontend`。
2. **一切逻辑写在 `Emit` 上，不写在 `AppHandle` 上**。跑 turn 的路径必须能在没有窗口时被测试驱动（`tests/bridge.rs` 用 collector 顶替 webview）。要 `AppHandle` 才能做的事只允许出现在 `main.rs` 与 `impl Emit for AppHandle` 里。
3. **webview 传来的一切是数据，不是指令**，`decision` 字符串尤其如此：认不出的决定一律当拒绝（`ApprovalAnswer::into_approval` 的 `_ =>` 分支），有测试钉住。永远不要为了"宽容"给它加 fallback 到放行的分支。
4. **一个会话同时只跑一个 turn，靠所有权保证**：`SessionHandle` 里的 `Session` 被跑 turn 的一方 `take` 走，结束再放回。不许改成"用一个 bool 标记忙"——那会漂移。
5. **事件名是契约**：`bridge.rs` 的 `AGENT_EVENT`/`APPROVAL_REQUEST`/`TURN_FINISHED` 常量与 `ui/src/types.ts` 里的同名常量必须同时改。`AgentEvent` 的 JSON 信封形状（adjacently tagged，`{type, data}`）由 `tcode-core` 的 `event_wire_tests` 钉住，改它就要同时改 `ui/src/types.ts`。
6. **用到新的 Tauri 内建能力，先改 `capabilities/default.json`**。自定义 `#[tauri::command]` 默认放行，但 core 插件的命令（event 的 `listen`/`emit`、window、fs、dialog…）必须显式授权，**未授权时前端那侧只是 promise reject，没有任何报错会自己冒出来**。这条是踩出来的：漏了 `core:default` 时，turn 正常跑完、事件正常 emit，界面却全空，看起来和"卡死"一模一样。
7. **前端不许有静默 reject 的 promise**。`listen()` / `invoke()` 一律接 `catch`，把原因显示成致命错误屏。第 6 条那个 bug 之所以难查，就是因为它当时是个 unhandled rejection。
8. **组件里不许出现字面量颜色/圆角/字号/字体栈，一律 `var(--token)`**。`ui/src/theme/base.css` 是 token 契约（含由 `--bg`/`--ink`/`--brand` 推导的兜底值，本身不含任何字面色），`themes/porcelain.css` 是默认主题包，两者的加载顺序就是覆盖顺序。**换主题 = 换 `main.tsx` 里的一行 import**，包括排版、密度、圆角、阴影，不只是配色。token 的**名字**是契约，主题可以改值不能改名。为什么这么严：写死一个 `#1d201b` 不会报错，只会在换主题那天变成一个找不着的污点。设计依据见 `DESIGN.md`，产品判断见 `PRODUCT.md`。
9. **路径不许用 `direction: rtl` 做前截断**。bidi 重排会把开头的 `/` 挪到结尾——`/home/me/code` 渲染成 `home/me/code/`。这不是外观问题：审批面板里给人看的是一条错的路径。用 `components/Path.tsx`，它按整段省略，一个字符都不改写。

    **进 app 的文件夹路径一律经 `paths::canonical_dir`**，不许直接 `canonicalize()`。Windows 上后者返回 `\\?\C:\…`，而 `store::project_id` 把非字母数字折成 `-`，于是同一个文件夹在桌面端落到 `----c--code-rust-tcode`、在终端落到 `c--code-rust-tcode`：会话与自动记忆分成两份互不知情。这条是踩出来的，`~/.tcode/projects/` 里那个四横线目录就是证据。

9b. **审批不是模态**。`Approval.tsx` 停在**它自己那个窗格**的 composer 上方，不铺 scrim、不抢焦点、不能被关掉。理由是这个 app 的立身之本：一个会话在等审批时，另外三个必须还能读、还能开、还能看谁在等——模态把并行管理直接废掉。分屏之后这条只会更硬：两个会话同屏，其中一个弹模态等于把另一个也冻住。安全性由别处保证：没有任何按键能回答它（旧的模态靠"焦点停在拒绝键上"防误触，一个抢焦点的常驻面板反而会和输入框抢每一次击键）；而"不可关闭"保留了模态真正买到的东西——未答的审批是一个停着的 turn，能被关掉的卡片等于没有回去的路。

    **它是一条横幅，不是一张卡片**：铺满窗格宽度、顶上一条 `--line` 发丝线、不投影，内容宽度走 `--measure`——和 transcript、composer 同一根轴。它曾经是 980px 的浮起卡片，于是既像丢了遮罩的模态，又和它正在询问的那个改动明显不对齐。**新加任何与对话对齐的东西一律读 `--measure`，不许再抄一个数字**，这条就是这么来的。另外它的高度上限是窗格的 50%（flex 的 `max-height`，不是 `vh`，也不是 grid track——grid 会把 `minmax(0, 50%)` 贪心撑满）。

    `ask_user` 走同一个面板的另一条分支，**按 input 形状识别（有 `questions[]`）而不是按工具名**。它有 2–4 个选项、可能多选、可能带 `preview`，答案聚合格式必须与 TUI 的 `QuestionPage::answer` 一致（单问题只发答案，多问题发 `N. 问题 → 答案`；选 "Something else" 时只发用户写的话，不许带上被拒绝的选项标签）——模型读到的是同一条 harness note，两个前端给出不同格式就是给同一个契约两个定义。

9c. **窗口自己画标题栏**（`decorations: false`）。因此 `.topbar` 是 title bar：它带 `data-tauri-drag-region`（`components/drag.ts` 的 `DRAG`），里面所有惰性元素也要带——Tauri 只看 mousedown 命中的那个元素，不看祖先——而所有可点元素绝不能带，否则拖拽吞掉点击。窗口按钮在 `components/WindowControls.tsx`，对应四条 capability 授权（start-dragging / minimize / toggle-maximize / close），少一条的表现是按钮静默无效（规则 6）。topbar 横跨整个窗口宽度而不是待在窗格里：窗口按钮属于窗口的角，不属于某一个窗格的角。

    **topbar 里只放窗口级别的东西**（返回启动台、窗口按钮），不放会话名、路径，也不放作用于某个会话的动作。窗口能同时开几个会话之后，任何"当前会话"的说法在这里都是猜的——文件索引开关就因此下沉到了每个会话窗格自己的 header。它看起来空是对的：它是 title bar，不是工具条。

9e. **转录里的每一步只有一种形状**（`Transcript.tsx` 的 `TraceGroup` + `.trace-*`）。thinking、单个调用、连续读、连续编辑、并发批次、sub-agent run 全走同一个组件与同一套 class。**不许再为某一类步骤新写一个容器**——曾经有五个近乎相同的组件配五套 class，后果是同一个工具时而是圆角卡片、时而只有一条线，取决于它前后有没有同类邻居。步骤之间不画线也不画框，靠 `.transcript-inner` 的节奏分隔（连续步骤 `--s-1`，与散文之间 `--s-4`）。理由不是审美：这些东西**会嵌套**（组里装调用、run 里装整份转录），而嵌套卡片是明令禁止的，行靠缩进可以无限嵌。

    配套的排版规则：**行的标签用 UI 字体，等宽只留给真正的机器文本**（工具名、命令、路径）。"run 2 commands" 是我们写的说明，不是机器输出；全用等宽就退化成 PRODUCT.md 点名反对的终端外观。

9d. **窗格树是纯数据**（`layout.ts`）：`Tiling = {root, focus}`，`Layout = Leaf | Split{dir, ratio, a, b}`。分裂/关闭/聚焦/改比例全是纯函数，由 `layout.test.ts` 钉住；`Panes.tsx` 只负责把这棵树画出来，`ratio` 变成 `flex-grow` 交给浏览器算。**布局判断不许写进组件**——发现自己在 `Panes.tsx`/`Workspace.tsx` 里算"这个窗格该被谁顶掉"时，那条规则属于 `layout.ts`（`show` 的"落在 inspect 窗格上就分裂而不是覆盖"就是这么回来的）。

    **键盘布局操作一律带 `Mod` 修饰键**（Ctrl/Cmd）。手基本一直在 composer 里，裸键就是文字。数字键从 `event.code`（`Digit1`）读，不从 `event.key`——按下 Shift 后 key 在美式布局上是 `!`，换个布局又是别的。方向移焦点靠 DOM 的 box 算（`focus.ts`，纯函数、有测试），不靠树：`row(a, col(b, c))` 里 `a` 往右该去 b 还是 c，树答不了，那是像素的事。

    **窗格必须画成平的一层**（`frames()` 出 rect，`Panes.tsx` 绝对定位，按 leaf id 作 key）。**不许改回嵌套容器**：嵌套时一个窗格的 DOM 深度会随周围的树变化——分裂一次就把原叶子包进新节点——React 会把移动了的子树卸载重建，于是转录的滚动位置、展开的工具输出、正在跑的 artifact iframe 全部丢失。"开个侧栏结果左边窗口跳回底部"就是这么来的，那不是滚动逻辑的 bug。

    `Pane` 的两个变体都带 `session`，这是承重的：关掉一个会话必须连它开出去的 diff、run、artifact 一起收走，`closeSession` 能写成几行全靠它。inspect 窗格持有的是 `Nav`（整条前进后退历史）而不是一个 `Inspect` 值——历史属于窗格，不属于组件，否则窗格一挪位置历史就没了。

10. **模型输出永远不许变成 markup**。`rich.tsx` 只用 marked 取 token，再**构造**白名单内的 React 元素；认不出的 token、原始 HTML token、不在协议白名单里的链接，一律按字面文本渲染。理由不是洁癖：这个 webview 里跑起来的脚本能拿到 `window.__TAURI__`，等于本机任意命令，而模型输出里天然混着文件内容、抓取的网页和 MCP 结果——按信任边界那条，它们是**观察到的数据**，不是指令。

    只有两处豁免，各自在文件里写明理由：`math.tsx`（KaTeX 的产物本身就是 markup 字符串，边界改画在 KaTeX 的 options 上，`trust: false` 是承重的那一条，且 options 被 freeze 且不接受参数）和 `src/sandbox/`（见规则 11）。`src/boundary.test.ts` 机械地扫描 `src/`，除这两处外出现 `dangerouslySetInnerHTML` / `innerHTML =` / `eval` / `new Function` 即失败；`src/rich.test.tsx` 用敌意 markdown 断言输出是文本。**新增第三处豁免之前，先确认它不能改成构造节点。**

11. **artifact 沙箱的三条不变量**（`src/sandbox/`，图表/图示/模型自写 HTML 都在里面渲染）：

    - **iframe 永远只有 `sandbox="allow-scripts"`，绝不加 `allow-same-origin`**。没有它，frame 是不透明源：`parent.document`、`parent.__TAURI__`、`localStorage` 全部抛 DOMException（实测），于是里面爱怎么 `innerHTML` 都只能毁掉它自己。加上它，这一整套设计当场归零。`rich.test.tsx` 钉住了这个属性。
    - **`tauri.conf.json` 的 `script-src` 永远不许出现 `unsafe-inline` / `unsafe-eval`**。它是规则 10 的第二道防线。（`img-src` 里的 `data:` 是给粘贴图片的缩略图开的，与规则 10 不冲突：`rich.tsx` 把模型输出里的 image token 渲染成文本 chip，模型根本没有产出 `<img>` 的路径。）
    - **沙箱里的脚本必须是经典脚本，不能是 module**。实测：不透明源下 `<script type="module">` 走 CORS，请求带 `Origin: null`，除非服务端显式放行否则**根本不执行**；经典 `<script src>` 是 no-cors，无条件可用。所以 `src/sandbox/*` 由 `vite.sandbox.config.ts` 单独构建成 IIFE 进 `public/`，不走主 module graph。改成 module 会在开发机上"看起来能跑"（若 dev server 恰好发了 ACAO）而在装出来的 app 里静默失效。

12. **沙箱拿到的主题值必须是 sRGB**。主题用 OKLCH 写，而沙箱里的第三方库自己解析颜色（mermaid 的 khroma 直接拒绝 `oklch(...)`）。`Sandbox.tsx` 的 `readTheme()` 负责光栅化转换后再发过去——注意只读 `fillStyle` 是不够的，Chrome 会把 `oklch()` 原样序列化回来，必须真画一个像素读回。

13. **`show` 出来的文件与模型自己写的正文同级不可信**（`ui/src/Shown.tsx` + `src/commands.rs::shown_file`）。文件是脚本产出的、不是模型手写的，但那不抬高它的信任等级——它同样是观察到的数据。所以 `.html`/`.svg`/`.mmd`/echarts option **必须走规则 11 那同一个 `sandbox="allow-scripts"` 无 same-origin 的 iframe**，不许因为"这是本机文件"改用 asset 协议、`same-origin` 或直接 `location = file://`。图片走后端返回的 `data:` URL（CSP `img-src` 已有 `data:`），因此也不需要任何新协议。

    **路径来自 webview，所以是数据**（规则 3）：`shown_file` 不因为"模型调 show 时已经查过了"就信它，重新过一遍 `tcode_tools::is_viewable_path`——工具与命令共用**同一个** boundary 定义，两处各写一份必然漂移。`VIEWER_TEXT_BUDGET` 同理：工具说"会被截断"和 viewer 真截断，用的是同一个常量。

    **`Inspect` 的 `shown` 是唯一读磁盘的检视值**，与 `Inspector.tsx` 顶上"一切来自 blocks 而非磁盘"刻意相反：其余每一种都在回答"agent 做了什么"，重读文件会把问题换成"现在是什么"；而 `show` 的文件正是为了**不进对话**才写到磁盘的，transcript 里根本没有它的字节。因此陈旧只靠 reload 按钮说清楚，**不要上文件监听**——为一个按钮换一套常驻机制不划算。

    **未做的第二阶段**：让 option 里能写 `{"$file": "pnl.csv"}` 由前端解引用，见 `DATA-BINDING.md`（含承重约束：解引用只能在父窗口做，沙箱读不到文件）。

    **`ui/src/show.ts` 是 `fences.tsx` 的第二个入口，不是第二张表**：围栏语言与文件扩展名问的是同一个问题，答案重合（mermaid/html/svg/markdown）。分成两套的后果是同一张图内联写和写成文件长得不一样，而 `show` 的全部意义就是"改的只有成本，不是结果"。`.json` 是唯一按内容判定的条目（它是容器不是一种东西），其余在加载任何字节之前就定了。

    **artifact 画在调用点，不自动开窗格**（`toolViews.tsx` 的 `showing.body`）。第一版是调用成功就自己开一个 inspect 窗格，错在没人要求就重排了窗口。现在它和 edit 画 diff 是同一件事：结果就在对话流里，`.shown.is-inline` 只加一个高度上限，**渲染的东西与窗格里一模一样**——两处画得不一样，那个按钮就从放大镜变成了赌博。

14. **"到自己的窗格"只有一个控件：`Transcript.tsx` 的 `PopOut`**。以前是把 summary（那条路径）本身做成 hover 出下划线的按钮，两个毛病：链接语义承诺的是"跳到别处"，而这里其实是**同一个东西、更大**；而且只在指针已经压上去时才出现的控件，没人会主动找到。现在每一条有去处的行——工具调用、exploration 行、sub-agent run——末尾都是同一个图标按钮，语义单一。新增可检视的东西时**用它，别再发明第二种点法**。

14b. **计划有两条写盘路径，不许合并成一条**。progress 文件的语法与"省略 detail 即保留"的规则都在 core（`revise_plan_body`），前端只送结构化 phases，永远不写 markdown。两条路径各有各的语义：

    - **审批期间**：编辑随 `respond_approval` 的 `phases` 回去，由 `into_approval` 用**后端自己留着的 review input** 拼出 `approved_input`（`Pending` 存 `asked` 就是为了这个：让 webview 回传整个 input，等于批准一件事、执行另一件事）。批准由 core 落盘，前端一个字节都不写；退回则不落盘，只把 diff 与评论送回。**校验失败不消费这次请求**——turn 正停在这个问题上，为了一个空标题把唯一的回答入口吃掉，会话就卡死了。
    - **非审批时段**：`write_plan` 直接落盘，且**故意不动会话内存里的 `disk_hash`**，好让模型下一次 `progress` 调用照常报冲突并拿到用户的版本。这是 core 既有的自愈契约，不是遗漏。

    读取只有一条：`plan(session)` 从磁盘重读（`SessionHandle::plan`），因为 progress 文件是外部可变状态，"计划现在说什么"问的是文件不是转录。它是 Inspector"一切来自 blocks 而非磁盘"的第二个例外，与 `show` 同理。**不许上轮询**：计划不会自己变，前端只在 `progress` 的 ToolEnd、审批到达、turn 结束、会话打开这四个时机重读。

    **"新开会话执行"要等 turn 结束再调 `execute_plan_elsewhere`**：`progress` 工具是在审批被回答**之后**才跑的，只有那之后文件才是 active。后端仍然自己校验一遍状态（`hand_off_plan`），不靠前端的时序守规矩。

    **`plan first` 送的是开关不是文本**：`send_message(plan: true)` 让后端取 core 的 `planning_instruction`。指令是 harness 的话，允许 webview 自己写指令正文，等于让它冒充 harness 对模型说话。

15. **模型/preset/role/模式的选择逻辑不在这里重写**（硬规则 1 的具体一例）。`picker.rs` 只做两件事：把 `tcode-frontend` 的 `ModelMenu`/`PresetMenu`/`AgentMenu` 转成 webview 能画的 JSON，和把选择转回那三个结构自带的闭包。切模型、写 `[tcode_state]`、重建 provider、重建全部 role pin、校验 preset 名——全在闭包里，TUI 的 `/model` 与 `/agents` 用的是同一批。**第二份"切模型"实现 = 第二个几乎正确的优先级链**（CLI flag > `[tcode_state]` > preset > config）。

    **闭包只到"建好新 provider"为止，装进共享 `ModelCell` 是调用方的活**（`picker::choose_model` 收 `&ModelCell` 并 `swap`，与 TUI 的 `apply_model` 同构）。preset 的 `apply` 闭包自己持有 cell 会顺手换掉，单选模型的 `switch` 不会——两者不对称，所以丢掉 `switch` 的返回值能编译、能过其余测试，表现只是"chip 弹回旧值、下个请求还是老模型"。`tests/bridge.rs` 有一条钉住它。同理，panel 显示的 effort 必须读 `ModelCell` 快照，不许读 `ModelDef::default_effort`——那是配置里的默认值，不是正在跑的值，读错就把一次生效的选择画成死控件。

    **preset 一换，三张菜单一起换**：`apply` 返回重建后的 `ModelMenu` 与 `AgentMenu`，`choose_preset` 必须两个都装回去。留着旧的 `agents` 等于让 panel 列出已经不存在的 pin。

    **webview 送来的 role 选择要校验两件事**：role 存在，且 `off` 对它合法（只有 `web-fetch` 这类默认关的角色可以关）。不许"宽容"地把不认识的 role 或非法的 off 归到最近的可行分支——那是规则 3 在这条路上的具体形态，`picker.rs` 有单测。

    **模型、effort、preset、role 是一个 chip 一个面板**（`ModelPanel.tsx` + 纯函数 `picker.ts`）。它们本来是并排三个 chip，等于要求读的人已经知道"选 preset 会把旁边那个 chip 也改掉"，而最该配置的两件事（`explore` 跑什么、怎么存一套编排）只能回终端做。面板的形状是承重的，别改回菜单：**列表滚动、旋钮不滚动**（effort 与 preset 钉在底边——面板向上弹，底边才是离刚点的 chip 最近的地方；effort 曾经是长列表的最后一段，也就是离光标最远、经常还在视野外）；**provider 是分组标题不是每行的第二行**；**面板里任何一次选择都不关闭面板**（一条规则没有例外要记：选完当场回读，退出靠 Esc / 点外面 / 再点 chip）；**role 是第二个视图不是第三段**（十行 role 塞在模型下面，是拿常用路径去伺候罕用路径）。它走 portal 到 `document.body`：popover 留在 composer 的 `<form>` 里会被窗格裁掉，里面的文本框还会在 Enter 时把消息发出去。

    **模式名一律显示原始 key**（`default` / `accept-edits` / `auto` / `unsafe`），与 `/mode`、config 文件、`PermissionMode::label()` 同一套词；另起一套好听的名字（"Ask first"）会让见过第一套的人以为是另一组模式。

    **模式按会话，模型按进程**：模式是"这个对话可以不问就做什么"，两个文件夹并排开时天然该不同；模型走共享 `ModelCell`，本来就是全窗口一个。模式仍然记进 `[tcode_state]` 当新会话默认——**除了 `unsafe`**，它刻意不粘（一次性放行不得静默武装以后每个会话），这条与 TUI 的 `/mode` 必须一致。

    **turn 跑着时改模式是 stage 不是丢**（`SessionHandle::staged_mode`）：`Session` 被跑 turn 的一方 take 走了（硬规则 4），所以选择存下来、`put_back` 时落地，chip 上显示 `→`。别改成"忙时报错"——那是把所有权约束当成用户的问题。

## 现有结构

- `src/bridge.rs`：出向事件（`SessionEvent`/`TurnFinished`/`ApprovalRequest`）、入向审批（`ApprovalAnswer`/`Pending`）、`WebviewApprover`、`pump_events`。`Emit` trait 在这里。
- `src/state.rs`：`Supervisor`（agent + `SessionFactory` + 会话表 + 顺序）、`SessionHandle`（会话私有的 session/cancel/pending）、`run_turn`。
- `src/commands.rs`：Tauri command，薄封装，只做参数校验后转 `state`/`projects`。
- `src/boot.rs`：app 的 composition root，外加 `SessionFactory`（开第二个文件夹时**按该文件夹重新加载 config**，因为 `.tcode/config.toml` 是项目级的）。
- `src/projects.rs`：启动台的数据源。`~/.tcode/projects/<id>/` 的目录名是路径的**有损**变换（`store::project_id` 把非字母数字全折成 `-`），反推不回文件夹，所以真实路径只从每条 session log 首行的 `Meta{cwd}` 读——每个项目一行，够便宜；带 preview 的完整重放留给用户真打开的那个项目（`project_sessions`）。
- `tests/bridge.rs`：scripted provider 驱动真实 agent loop，断言事件流、审批往返、fail-closed、双会话隔离、忙会话拒绝第二个 turn。**测试不打真 API。**
- `ui/src/`：`types.ts`（wire 契约）、`blocks.ts` 与 `files.ts`（事件→块树 / 事件→文件清单，都是纯函数 reducer）、`session.ts`（每个会话的 UI 状态，窗格按 id 查）、`layout.ts`（窗格树，纯函数）、`Launchpad.tsx`（第一屏）、`Workspace.tsx`（标题栏 + 会话栏 + 窗格场）、`Panes.tsx`（窗格树的渲染与窗格外框）、`theme/`（token 契约与主题包）、`preview/`（只在 `PREVIEW=1` 下加载的 fixture，`split` 场景是三窗格嵌套，专门用来看递归对不对；`plan` 场景是带评论与改动的审批面板；`model` 场景会自己把模型面板点开，因为静态看一遍看不到它；场景名进 URL 的 `?scene=`，所以一个状态可以直接链接、刷新也还在）。
  - **模型这一摊**：`picker.ts`（wire 类型 + 纯函数：按 profile 分组、pin 的措辞、effort 槽位）、`ModelPanel.tsx`（面板本体，见硬规则 15）、`Chips.tsx`（composer 下面那条：模式菜单 + 面板的触发 chip）。`mock-core.ts` 里的 picker fixture 是**可变的**，与其他静态 fixture 不同：这个面板的验收标准就是"选完能回读"，一个永远回答 `Opus 5 · high` 的 fixture 演示不出来。
  - **计划这一摊**：`plan.ts`（类型 + 全部纯操作：改/增删/换序/状态循环、计划条的行、`planChanges` 结构化比对）、`selection.ts`（选区→引用，纯函数）、`PlanEditor.tsx`（审批面板与计划窗格共用同一个编辑器、同一份 draft）、`ProgressStrip.tsx`（composer 上方那一行）、`components/SelectionBubble.tsx`。**编辑靠 id 认行**（`DraftPhase.id`，只在一次编辑里有效、不落盘），所以"改名"是改名而不是"删一个加一个"；按标题匹配那条路恰好在用户最认真的时候错得最狠。
  - **`blocks.ts` 不叫 `transcript.ts`**：那个名字与 `Transcript.tsx` 只差大小写，在不区分大小写的文件系统上 tsc 会把两个 import 解析成同一个文件，Windows 上直接构建失败。同理，新增模块别取只和某个组件差大小写的名字。
  - **块是树**：`run`（sub-agent）与 `batch` 持有子块。`TaskRunEvent` 裹的是一个完整的 `AgentEvent`，所以 sub-agent 的内容就是同一个 reducer 递归一层——嵌套 run 不需要任何额外代码。
  - **一个 inspect 窗格是单值槽，不是 tab 容器**（`inspect.ts`）。它只持有一个 `Inspect` 值，栈底是文件索引；转录里的路径、文件行、run、artifact 全都只调 `open(...)`。前进/后退因此是白拿的，新增一种可检视的东西 = 加一个 `kind`，不是加一个 tab。**分屏没有削弱这条，反而更纯粹**：想同时看两样东西就分两个窗格，不是在一个窗格里长出 tab 条。每个会话只复用一个 inspect 窗格（`openInspect`），所以连开五样东西是五条历史记录而不是五个窗格。
  - **两张前端注册表**，同构于 core 的 `Tool`/`SlashCommand`/`ToolRenderer`：`fences.tsx`（围栏语言→富渲染，未命中落到高亮代码块）与 `toolViews.tsx`（工具→怎么画 + 点开看什么）。**路由不在前端定**，`tool_views()` 从后端拿，`route` 与 `quiet_output` 现在都由活着的工具派生（`Tool::route` / `Tool::batch_policy`），名字表已经删了。唯一的例外是 `progress`：它的路由**随 input 变**（阶段翻转归计划条，提交的计划是对话要留住的文档），后端送的是工具的默认答案，那一个例外由 `plan.ts::isPlanSubmission` 在调用点认出来——一个字段，紧挨着它读的类型。
  - **批次里的调用没有 summary**：core 的三条并发路径（parallel read / mutation lanes）只发 `ToolBatchStart`，不为每个调用发 `ToolStart`，所以 `(call_id, name, input)` 之外什么都没有。`toolViews.tsx` 的 `describe` 从 input 里取目标补上——展开一个批次看到五行 `read` 而不知道读了什么，等于批次白折叠。真正的修法是 core 把它已经算好的 `summarize_call` 一并放进 `ToolBatchStart`。
  - **`show` 的三个文件**：`show.ts`（扩展名→怎么画的注册表 + CSV 解析，纯函数、有测试）、`Shown.tsx`（唯一读磁盘的检视视图，见硬规则 13）、后端的 `commands.rs::shown_file`（哑字节服务：**要 text 还是 `data:` URL 由前端那张表说了算**，后端不再判一次，否则就是两张要同步的表）。表格只把有限行放进 DOM（`ROW_STEP`），不是分页而是上限——20 万行 DOM 就是一个冻住的窗口。
  - **粘贴/拖入的图片**走 `paste.ts`（长边 1568 以上重采样，模型本来也只看这个分辨率）→ `send_message` 的 `images` → `commands.rs::compose`。模型不支持 vision 时图片**存进 scratch 并告诉模型路径**，不静默丢——用户贴了个东西，丢掉它等于让人对着一张谁也没有的图提问。
  - **语法高亮是自己写的**（`syntax.ts`），因为 Shiki/highlight.js 自带调色板是字面值，主题包改不动它，等于在"chroma 只表示状态"的界面里塞第二套配色。它输出语义 class，颜色由 `base.css` 的 `--syn-*` 契约决定。
- `src/paths.rs`：`canonical_dir`——app 里唯一一处把用户选的文件夹变成键的地方（见硬规则 9）。
- `capabilities/default.json`：webview 的权限授予（见硬规则 6）。现有 `core:default` + 四条 window 授权（自绘标题栏，见 9c）+ `dialog:allow-open`（"打开文件夹"要它）。
- `icons/`：由 `icons/mark.svg` 用 `rsvg-convert` 生成，改标记要重新导出全部尺寸。

## 已知限制

**多文件夹会话共用一条 `ShellFilters` 链。** 它是 boot 时建的、被 agent 的工具集持有，`open_folder` 只能把同一个 `Arc` 再注册一次。后果：A 项目 `.tcode/filters.toml` 里的 shell 输出过滤规则会作用到 B 项目的 shell 输出。影响面是输出裁剪，不涉及权限或安全边界，所以没为它改 core；但这条**不是**"每会话隔离"，别在它上面叠新假设。真要修，得让 shell 工具按 `ToolCtx` 取 filter 链而不是在构造时捕获。

## 排查手册

界面没反应时，**先看 stderr**，它把"没跑起来 / 跑完了但前端没收到"分得很清楚：

- 无 `turn started` → command 里的 spawn 没起来。
- 有 `turn started` 无 `turn finished/failed` → 卡在 provider 请求。
- 两行都有但界面空 → 前端监听侧。九成是 capabilities 或某个没接 catch 的 promise。
- `could not emit '…'` → 事件名非法（Tauri 只收 `[a-zA-Z0-9-/:_]`）或窗口已关。
