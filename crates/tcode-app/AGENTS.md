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

**改界面先开 `npm run preview:ui`。** 它用 vite 的 `--mode preview` 把 `@tauri-apps/api/*` 与 dialog 插件别名到 `ui/src/preview/` 下的 fixture，然后加载 `preview.html`，把**真实组件**（不是另画一套 mock）按 launchpad / session / approval / question / plan / split / shown / empty 这些场景摆出来。没有它，"跑起来的会话正在等审批"这类状态要复现一次得起真 provider 打真 API。别把 mock 引进 `main.tsx` 那条路径——别名只在这个 mode 下生效，发布产物里没有它们。

**开关是 `--mode` 而不是环境变量，这条是踩出来的**：原来写的是 `PREVIEW=1 vite`，那是 POSIX shell 给单条命令设环境变量的语法，在 cmd 与 PowerShell 里是**解析错误**——于是"改界面先开预览"这条规矩在 Windows 上整整一段时间根本执行不了，而失败长得像 npm 坏了。新加需要开关的 script 一律用 CLI flag。

**这个 mode 下 `/` 就是预览本身**（`vite.config.ts` 的 `previewRoot`）。不做这个 rewrite 时根路径照旧发 `index.html`——真 app 的壳，fixture 是别名进来了但没有后端回答它启动时发的那些命令，于是画出一个空窗加一行 console warning，**和"预览坏了"完全分不清**。而它离一个错 URL 永远只有一步：刷新时丢了路径、存了个书签、或者照着 vite 启动时打印的那个 host 直接敲进去。所以 `--open` 不带路径也是对的。

**`tauri.conf.json` 里刻意不配 `devUrl`。** Tauri 在 debug 构建下只要看见 `devUrl` 就去连它，于是 `cargo run` 会撞上 "Connection refused"——而 `cargo run` 正是这里的主流程。不配它，debug 与 release 一样加载 `frontendDist`（`ui/dist`），代价是改前端要重跑一次 `npm run build`。想要 HMR 就临时加回 `devUrl: "http://localhost:5173"` 并同时起 `npm run dev`，别把它留在提交里。

## 不可违背

1. **装配逻辑不在这里重写**。config 加载、`Arc<Agent>` 组装、开会话全部走 `tcode-frontend`（`boot` / `open_session`）。`src/boot.rs` 只放 app 独有的决定（开哪个文件夹、没配置 provider 时报错而不是画向导——这里没有终端可画）。发现自己在抄 `src/main.rs` 的段落时，那段就该下沉到 `tcode-frontend`。
2. **一切逻辑写在 `Emit` 上，不写在 `AppHandle` 上**。跑 turn 的路径必须能在没有窗口时被测试驱动（`tests/bridge.rs` 用 collector 顶替 webview）。要 `AppHandle` 才能做的事只允许出现在 `main.rs` 与 `impl Emit for AppHandle` 里。
3. **webview 传来的一切是数据，不是指令**，`decision` 字符串尤其如此：认不出的决定一律当拒绝（`ApprovalAnswer::into_approval` 的 `_ =>` 分支），有测试钉住。永远不要为了"宽容"给它加 fallback 到放行的分支。
4. **一个会话同时只跑一个 turn，靠所有权保证**：`SessionHandle` 里的 `Session` 被跑 turn 的一方 `take` 走，结束再放回。不许改成"用一个 bool 标记忙"——那会漂移。
5. **事件名是契约**：`bridge.rs` 的 `AGENT_EVENT`/`APPROVAL_REQUEST`/`TURN_FINISHED` 常量与 `ui/src/types.ts` 里的同名常量必须同时改。`AgentEvent` 的 JSON 信封形状（adjacently tagged，`{type, data}`）由 `tcode-core` 的 `event_wire_tests` 钉住，改它就要同时改 `ui/src/types.ts`。
6. **用到新的 Tauri 内建能力，先改 `capabilities/default.json`**。自定义 `#[tauri::command]` 默认放行，但 core 插件的命令（event 的 `listen`/`emit`、window、fs、dialog…）必须显式授权，**未授权时前端那侧只是 promise reject，没有任何报错会自己冒出来**。这条是踩出来的：漏了 `core:default` 时，turn 正常跑完、事件正常 emit，界面却全空，看起来和"卡死"一模一样。

    **`core:default` 不等于"窗口能力都有了"，它只给了只读的那一半**：`is-maximized`、`title`、显示器列表、`internal-toggle-maximize` 在里面，而 `minimize` / `toggle-maximize` / `close` / `start-dragging` 不在。这条也是踩出来的，而且比上一条更难看见：自绘标题栏（规则 9c，`decorations: false`）意味着**没有系统标题栏可以退回去**，于是那三个按钮按下去 reject 成一句 `console.warn`、窗口也拖不动，界面其余部分完全正常。所以标题栏调的每一个"会动手的"窗口命令都必须在这个文件里点名——`browser.rs::the_title_bar_is_granted_the_commands_it_calls` 直接从 `WindowControls.tsx` 里扫这些调用来对，新加一个控件要么带着它的授权，要么在那儿失败。
7. **前端不许有静默 reject 的 promise**。`listen()` / `invoke()` 一律接 `catch`，把原因显示成致命错误屏。第 6 条那个 bug 之所以难查，就是因为它当时是个 unhandled rejection。
8. **组件里不许出现字面量颜色/圆角/字号/字体栈，一律 `var(--token)`**。`ui/src/theme/base.css` 是 token 契约（含由 `--bg`/`--ink`/`--brand` 推导的兜底值，本身不含任何字面色），`themes/porcelain.css` 是默认主题包，两者的加载顺序就是覆盖顺序。**换主题 = 换 `main.tsx` 里的一行 import**，包括排版、密度、圆角、阴影，不只是配色。token 的**名字**是契约，主题可以改值不能改名。为什么这么严：写死一个 `#1d201b` 不会报错，只会在换主题那天变成一个找不着的污点。设计依据见 `DESIGN.md`，产品判断见 `PRODUCT.md`。
9. **路径不许用 `direction: rtl` 做前截断**。bidi 重排会把开头的 `/` 挪到结尾——`/home/me/code` 渲染成 `home/me/code/`。这不是外观问题：审批面板里给人看的是一条错的路径。用 `components/Path.tsx`，它按整段省略，一个字符都不改写。

    **进 app 的文件夹路径一律经 `paths::canonical_dir`**，不许直接 `canonicalize()`。Windows 上后者返回 `\\?\C:\…`，而 `store::project_id` 把非字母数字折成 `-`，于是同一个文件夹在桌面端落到 `----c--code-rust-tcode`、在终端落到 `c--code-rust-tcode`：会话与自动记忆分成两份互不知情。这条是踩出来的，`~/.tcode/projects/` 里那个四横线目录就是证据。

9b. **审批不是模态**。`Approval.tsx` 停在**它自己那个窗格**的 composer 上方，不铺 scrim、不抢焦点、不能被关掉。理由是这个 app 的立身之本：一个会话在等审批时，另外三个必须还能读、还能开、还能看谁在等——模态把并行管理直接废掉。分屏之后这条只会更硬：两个会话同屏，其中一个弹模态等于把另一个也冻住。安全性由别处保证：没有任何按键能回答它（旧的模态靠"焦点停在拒绝键上"防误触，一个抢焦点的常驻面板反而会和输入框抢每一次击键）；而"不可关闭"保留了模态真正买到的东西——未答的审批是一个停着的 turn，能被关掉的卡片等于没有回去的路。

    **它是一条横幅，不是一张卡片**：铺满窗格宽度、顶上一条 `--line` 发丝线、不投影，内容宽度走 `--measure`——和 transcript、composer 同一根轴。它曾经是 980px 的浮起卡片，于是既像丢了遮罩的模态，又和它正在询问的那个改动明显不对齐。**新加任何与对话对齐的东西一律读 `--measure`，不许再抄一个数字**，这条就是这么来的。另外它的高度上限是窗格的 50%（flex 的 `max-height`，不是 `vh`，也不是 grid track——grid 会把 `minmax(0, 50%)` 贪心撑满）。

    **那根轴由一个盒子持有，不许按子元素逐个授予。** 读 `--measure` 只是第一半；写成 `.某容器 > * { max-width: var(--measure); margin-inline: auto }` 的那一半是个陷阱——任何子元素只要有一条同特异性、位置更后的规则就能把它丢掉。progress strip 就是这么破的：`.strip-phases` 为 `<ol>` 的 UA margin 写了一句 `margin: 0`，于是展开的阶段列表贴到窗格左边缘，而它上面那行还老实待在 composer 的轴上，窗格越宽（比如折起会话栏）两根轴分得越远。现在 `.strip-body` 一个盒子持有轴，里面所有东西继承，没有第二处可以丢。

    **滚动区里的轴要 `scrollbar-gutter: stable both-edges`。** 占宽度的滚动条只从一侧拿走宽度，于是 transcript 里居中的 `--measure` 列比不滚动的 composer 与 progress strip 偏左半个滚动条（实测 5px）。这不是滚动条审美问题，是"一根轴"这句话的成立条件。平台画覆盖式滚动条时它什么也不占；属性不认识时被忽略，退回到原来那个偏移。

    **面板正文来自 call 自己的 input，不是 core 那两个字符串**（`Approval.tsx::readCall`）。core 对每次授权都描述两遍：`descriptor` 是"永久允许"会落盘的规则（`run(git status)`），`summary` 是它的句子形态（`run: git status`）。两条叠着印，读的人要逐字比对才发现它们相等，而那对括号是权限规则的语法、不是给人读的东西。所以正文改成从 input 取：有 `command` 就整条画进代码井（`--sunken` + `pre-wrap`，**换行必须留着**——一行式 summary 天生丢 heredoc），否则画 registry 给的 target（裸路径/URL，不带 `edit(...)` 外壳）。`descriptor` 挪进 "show the exact call" 与两个持久化按钮的 `title`，因为它在那儿才是事实：那是**按下去会写进文件的那一行**。同理 `cwd` 只在 call 自己指定时画——沉默就是会话目录，窗格 header 已经在说了。

    **命令用共享的 `Code` 块画，不另造一个代码井。** 它和对话里一个 ```sh 围栏拿到的是同一个组件：同一套高亮、同一个复制按钮。原来那块无高亮的等宽灰板是为"全 app 唯一一处必须逐字读懂代码"的地方另发明的、更差的第二种代码呈现——而一条 `rm` 正是藏在管道中段的，高亮在这儿不是装饰，是"你要同意的东西"的结构本身。三处按需覆盖：语言 chip 隐掉（"Run this?" 已经说了）、字号提到 `--text-sm`（这是屏幕上的那句话，不是句子里的引文）、`pre-wrap` 不横向滚动（尾巴在屏幕外的命令等于没读就批了）。还有 **`.tok-punct` 在这里提到 `--syn-plain`**：它在别处 muted 是对的（3.76:1，过 3:1 的次要文本底线），但这里被弱化掉的字符是把输出送去别处的管道和区分 `-v` 与 `--nocapture` 的横杠——它们不是意义周围的语法，它们就是意义。

    **header 只能命名一次动作。** 标题按 shape 选（edit → "Change a file?"，有 command → "Run this?"，其余 → "Allow this?"），工具名 chip **只在标题落到那句泛问时才出现**——`shell` 的 `display_name()` 就是 "Run"，标题已经说了动词时再挂一个 chip 就是同一个词印两遍。反过来，web_fetch 与 MCP 工具正是标题猜不出动词的场合，那时 chip 就是这次调用的全部身份。判定在 `named()`，被 `consent.test.ts` 钉住（fixture 里的 descriptor/summary 必须是 core 真发的那两串，见规则 9f 的同一条教训）。

    `ask_user` 走同一个面板的另一条分支，**按 input 形状识别（有 `questions[]`）而不是按工具名**。它有 2–4 个选项、可能多选、可能带 `preview`，答案聚合格式必须与 TUI 的 `QuestionPage::answer` 一致（单问题只发答案，多问题发 `N. 问题 → 答案`；选 "Something else" 时只发用户写的话，不许带上被拒绝的选项标签）——模型读到的是同一条 harness note，两个前端给出不同格式就是给同一个契约两个定义。

9c. **窗口使用 app 自绘标题栏**（`decorations: false`）。`.topbar` 与其 `--chrome` 背景是应用主题的一部分，不能继承系统 caption 颜色；最小化、最大化、关闭由 `WindowControls` 调用 Tauri window API，`WindowDragRegion` 将 `data-tauri-drag-region` 只放在不含交互控件的 `.topbar-gap`，并保留双击最大化。浏览器是原生子 webview，会在 HTML 之上接收命中，因此它只能由 pane body 的 DOM rect 定位在标题栏下方，绝不能覆盖 drag region 或窗口控制。

    **topbar 里只放 app 级别的东西**（返回启动台、折叠会话栏、显示偏好），不放会话名、路径，也不放作用于某个会话的动作。判据是"这东西属于整个 app 还是属于某个会话"：会话栏属于 app，所以它的折叠开关在这儿；文件索引属于某个会话，所以它下沉到了那个窗格自己的 header。**文件夹选择也不在这儿**——分屏之后两个窗格是两个文件夹，"当前文件夹"在这一层是猜的，所以它是每个窗格 header 上的 `FolderMenu`（同时兼任窗格身份，会话名本来就等于文件夹名，画两遍是同一个事实占两个元素）。

9e. **转录里的每一步只有一种形状**（`Transcript.tsx` 的 `TraceGroup` + `.trace-*`）。单个调用、连续读、连续编辑、连续命令、并发批次、sub-agent run 全走同一个组件与同一套 class。**不许再为某一类步骤新写一个容器**——曾经有五个近乎相同的组件配五套 class，后果是同一个工具时而是圆角卡片、时而只有一条线，取决于它前后有没有同类邻居。步骤之间不画线也不画框，靠 `.transcript-inner` 的节奏分隔（连续步骤 `--s-1`，与散文之间 `--s-4`）。理由不是审美：这些东西**会嵌套**（组里装调用、run 里装整份转录），而嵌套卡片是明令禁止的，行靠缩进可以无限嵌。

    **thinking 不在这套形状里，而且这条是反过来学到的。** 它曾经也是一条 `TraceGroup`，于是与"并发批次""连续读"长得一模一样，读的人必须逐行展开才知道哪几行是真发生过的事——一个折叠行看起来像一步，但它不是一步。现在它是散文（`.thinking`）：勾选了就直接铺出来，没勾就整块不存在（默认不存在，见规则 18）。连带一条：`groupTranscriptBlocks` **不许再把 thinking 扫进 exploration 组**，折叠的组会把它吞掉，"给我看推理"最后靠藏起来回答。

    **成功且"body 就是结果"的那一步不画 output。** `edit` 返回的 `edited <path> (1 replacement). Result:` 加编号片段是**写给模型看的**（省掉它再读一遍文件），画在 diff 底下就是同一处改动换个更差的记法再来一遍，而读的人刚刚已经在红绿里看过了。后端早就把这条判断发出来了（`ToolViewMeta::hide_success_result`，`BODY_IS_THE_RESULT` 那四个工具），TUI 一直在遵守（`view.rs::result_render`），**只有这边从来没读过它**——`tests/bridge.rs` 里有断言钉住这个字段，却没人消费。别再把它当成"前端可以自己判断的表现细节"重写一份名字列表。

    连带一条：**转录里没有任何一行会自己展开。** `editDetails` 开关的语义是"把 changes 组打开，好让 diff 在屏幕上"，不是"再顺手把每条底下的文字也铺开"——它曾经悄悄兼任了后者，于是每个 edit 底下都挂着一段没人要看的模型自愈文本。

    **失败的那一步不画它的 body，也不画第二遍错误。** 调用失败等于什么都没发生，所以被拒的 edit **不许再画 diff**：红绿两色在这个 app 里的意思就是"文件被这样改了"，而这里文件根本没动，那是转录里最有说服力的一句假话。行上的 `failed` 加一条 `.tool-error`（`--danger-text`，只为失败而生，不是伪装成 output 预览的 muted 小字）就是全部。同时 `preview` 是 result 的第一行、而工具错误通常只有一行，于是老写法在 flow 里印一遍、在下面的 disclosure 里再印一遍**一模一样**的字——所以失败时 disclosure 不带 output（`shownOutput`），expand 也就自然消失。想看完整输出走行尾的 PopOut：`inspectFor(name, failed)` 让失败一律回落到 `FALLBACK`（output），不去开一个不存在的 diff。preview fixture 里必须留一条 `isError: true` 的调用，这个 bug 长期没人看见就是因为 fixture 里没有任何东西会失败。

    **每一行的展开控件都在行尾，都是 hover 才显形**——组、单个调用、sub-agent run 一视同仁。之前组和 run 的 chevron 在行首、run 的还常驻，结果一列步骤有两条左边缘：组的标签比单个调用的工具名右一个字形，而这正是"只有一种形状"要消掉的那种参差。配套的两条几何：**tinted 行盒左右各外扩 `--s-2`**（`margin-inline: calc(var(--s-2) * -1)`，三种行同写法），`width: 100%` 配负左 margin 是陷阱——盒子右边会短 `--s-2`，组的尾部 chevron 因此比调用的左 8px；`.trace` 因此是 flex column，让那个 `<button>` 不必自己声明宽度。组的 chevron 是 `<span>` 不是 `<button>`：整行本来就是开关，button 套 button 是非法结构。

    配套的排版规则：**行的标签用 UI 字体，等宽只留给真正的机器文本**（工具名、命令、路径）。"Run 2 commands" 是我们写的说明，不是机器输出；全用等宽就退化成 PRODUCT.md 点名反对的终端外观。**工具名一律用 core 的 `Tool::display_name()`**（经 `ToolViewMeta.display_name` 过桥，`useToolName()` 读），前端不许自己再写一套大小写规则——两套的结果是同一列里 `Read 15 files`（core 的 `batch_label`）挨着 `read 3 files`（前端自己拼的组标签）。前端自己拼的组标签因此也首字母大写。

    **正在跑的那一步带 `--brand-wash`，靠 `.is-running`**，结果到达时用 `--dur-slow` 褪掉——chroma 表示状态，所以运行中的行可以有，闲着的行不许有，工具名永远不因为"它是工具名"上色。旁边那颗呼吸的点已经是全 app 唯一一个常驻动画，这里不许再加第二个。

9g. **"正在跑"这一行说的是**在哪一步**，不是"有事在跑"**（`activity.ts::phaseOf` + `Transcript.tsx::Working`）。它曾经每一秒都只印 `working` 一个词——而"有没有在跑"这件事状态点、会话栏、窗格 header 已经各答一遍了，"跑到哪儿了"没有任何地方答。词表照抄 TUI 的 `state_label`（`app/turn.rs`）：`sending` / `responding` / `thinking` / `writing` / `calling a tool` / `Run · <目标>` / `retrying (2/5)` / `sub-agent working` / `compacting history`。**它不是 wire 契约**，两边漂了也不炸；但同一个人两个前端都在用，同一个状态取两个名字是白让人多学一遍。

    三条容易破的：**返回 `null` 表示"这个事件与阶段无关"**，保留上一个答案——记账类事件（`Usage`/`Note`/`ToolEnd`）远多于有信息的事件，给它们一个兜底词只会让这行在两个有意义的状态之间不停闪回泛词。**`TaskRunEvent` 一律 `null`**：那是子 agent 的阶段、不是这个 turn 的，而且成百上千地来，放进来就是让屏幕上这一行替一个没人在看的会话闪。**工具名走 `display_name()`**，不许用 wire 名——它和 `.rail-activity` 是同一个字符串，一处写 `read` 一处写 `Read` 就是 9e 那条"一列里两种大小写"换个地方重演。

    工具名的解析器在 `App.tsx` 的监听里，用的是那个 effect **自己的局部 `names` map**，不是 `toolMeta` state：这个 effect 不能重跑（重订阅会把每个 delta 收两遍），所以闭包会永远读到注册时那张空表。别"顺手"改成读 state。

    **动效只有一个，就是这一行**（`.working` + `@keyframes working-sweep`，几何照搬 `theme.rs::shimmer_color`：一条软光带、匀速、走完在右边缘外停一拍）。**光带扫的是底，绝不扫字**——`background-clip: text` 是明令禁止项，而且它必须覆写文字自己的颜色才成立，恰好和"活着的行是被**提亮**而不是被换掉"这条相反。**转录里的行不许各自扫**：五行各扫各的时钟就是纯噪音。还有一条踩出来的：**`@keyframes` 是全局扁平命名空间，没有作用域也没有告警**——本来叫 `sweep`，被文件后面 artifact loader 的同名 `sweep` 静默顶掉，行还在动（动的是别人的 opacity 脉冲），看起来就像"扫光写坏了"。新加动画一律带宿主前缀命名。

9f. **一次委派只画一行。** `agent` 调用与它开出的 run 是同一步的两份记录：run 带 kind/model/调用数/状态与自己整份转录，调用带回来的 report。两行都画，这一步就占两行，第一行还是毫无信息的 `agent · agent(explore)`。`blocks.ts::runPairs` 按 **`parent_call`** 配对（`RunMeta.parentCall`，wire 上本来就有），把调用行让给 run 行，report 挪进 run 里；**不许按工具名判**——转录不需要知道那个工具叫什么。老日志没有 `parent_call` 时配不上，两行照画：多一行比少一行诚实。

    **sub-agent 的状态串是 `TaskRunStatus`**（`done` / `failed` / `cancelled` / `interrupted`，snake_case），不是 `ok`。这里踩过一次：代码比的是 `"ok"`，于是每个正常跑完的 sub-agent 都戴着红叉，而 preview fixture 里写的也是 `"ok"`，所以设计预览永远看不出来。**fixture 写的必须是 wire 上真有的值**——一个替代实现陪着一个错误实现，等于把验收手段一起废掉。`cancelled`/`interrupted` 既不是成功也不是失败，状态点没有对应字形，所以用词说，不许借失败的颜色。

9d. **窗格树是纯数据**（`layout.ts`）：`Tiling = {root, focus}`，`Layout = Leaf | Split{dir, ratio, a, b}`。分裂/关闭/聚焦/改比例全是纯函数，由 `layout.test.ts` 钉住；`Panes.tsx` 只负责把这棵树画出来，`ratio` 变成 `flex-grow` 交给浏览器算。**布局判断不许写进组件**——发现自己在 `Panes.tsx`/`Workspace.tsx` 里算"这个窗格该被谁顶掉"时，那条规则属于 `layout.ts`（`show` 的"落在 inspect 窗格上就分裂而不是覆盖"就是这么回来的）。

    **键盘布局操作一律带 `Mod` 修饰键**（Ctrl/Cmd）。手基本一直在 composer 里，裸键就是文字。数字键从 `event.code`（`Digit1`）读，不从 `event.key`——按下 Shift 后 key 在美式布局上是 `!`，换个布局又是别的。方向移焦点靠 DOM 的 box 算（`focus.ts`，纯函数、有测试），不靠树：`row(a, col(b, c))` 里 `a` 往右该去 b 还是 c，树答不了，那是像素的事。

    **窗格必须画成平的一层**（`frames()` 出 rect，`Panes.tsx` 绝对定位，按 leaf id 作 key）。**不许改回嵌套容器**：嵌套时一个窗格的 DOM 深度会随周围的树变化——分裂一次就把原叶子包进新节点——React 会把移动了的子树卸载重建，于是转录的滚动位置、展开的工具输出、正在跑的 artifact iframe 全部丢失。"开个侧栏结果左边窗口跳回底部"就是这么来的，那不是滚动逻辑的 bug。

    `Pane` 的两个变体都带 `session`，这是承重的：关掉一个会话必须连它开出去的 diff、run、artifact 一起收走，`closeSession` 能写成几行全靠它。inspect 窗格持有的是 `Nav`（整条前进后退历史）而不是一个 `Inspect` 值——历史属于窗格，不属于组件，否则窗格一挪位置历史就没了。

9h. **浏览器窗格是原生子 webview，而且是窗口级单例**（`src/browser.rs` + `ui/src/WebPane.tsx`，`Pane` 的第三个变体 `{kind:"web"}`）。

    **capability 恒为空，这条是全仓库最容易静默破掉的一条。** Tauri 的 capability 按 label 匹配，而 `windows: ["main"]` 的语义是**授予该窗口下的每一个 webview**（tauri-utils 原话：regardless of the value of `webviews`）。浏览器是 `main` 的子 webview，所以那一行等于把 `core:default` 发给任意网页，也就是把 `window.__TAURI__`、也就是本机任意命令，发给任意网页。**破掉时什么都不会坏**：app 正常、浏览器正常、只是每个站点都被信任。`capabilities/default.json` 因此写 `webviews: ["main"]`，`browser.rs` 里有一条测试读那个文件钉住它（已验证改回 `windows` 会红）。这也是回读页面只能走 `eval_with_callback` 的原因——它在 runtime 层（WebView2 的 `ExecuteScript`）而不是 Tauri IPC，所以页面不需要任何权限我们也读得到。永远不要为了"让 agent 能操作页面"去加 `dangerousRemoteDomainIpcAccess`。

    **它不带 `session`，这是承重的不是省事**：`closeSession` 按 `paneSession(pane)` 过滤，浏览器答 `null` 于是永远不会被会话带走——你正在读的文档不该因为关掉一个对话而消失。连带三条：入口放在每个会话窗格的文件/工作区工具组中，方便在找文件时就近打开，但它仍只会聚焦同一个窗口级浏览器；`sessionsInView` 要跳过它；**`show` 落在它上面必须分裂而不是覆盖**——那是窗口里唯一一个覆盖掉就找不回来的窗格。`layout.test.ts` 有一组测试钉这几条。

    **原生 webview 合成在 HTML 之上**，不在任何 CSS 能触达的层叠上下文里，所以**每个 popover 打开时浏览器必须让位**（`seat.ts::yieldToPopover`，计数而非布尔——popover 会嵌套，最后一个关掉才恢复）。挂在 `seat.ts` 而不是各调用点，就是规则 17 那条"只有一份实现"现在多了一个更难查的失败：菜单开在页面**后面**，看起来是按钮没反应。

    **rect 由前端连续上报**（`WebPane` 的 `useLayoutEffect` **不带依赖数组**）：原生 webview 不参与布局，它只待在最后被告知的位置，而窗格会**不改变尺寸地移动**（邻居关闭、远处分隔条被拖），那种情况 `ResizeObserver` 一次都不响。两者都要。

    **地址栏是视图不是真相**：webview 自己拥有"现在在哪"，输入只是请求它去某处，显示的 URL 一律来自 `BROWSER_NAVIGATED` 回报——于是重定向、点链接、`history.back()` 三种情况走同一条路径，没有任何地方需要猜一次导航是否真的发生了。**前进后退按钮不能置灰**：有没有可去之处存在页面自己的历史里，跨源读不到；`history.go` 无处可去时什么都不做，那是无害的方向，而猜一份栈出来只会在 SPA 上错得更难查。

    **`to_url` 里 loopback 走 `http` 不是 `https`**（纯函数，有测试）：这个窗格被要求做出来的头号用途就是看 dev server，而 `localhost:5173` 是明文 HTTP，默认给它 `https` 等于在最常输入的那一行前面立一个 TLS 错误页。**裸词报错而不是搜索**：这个 app 没有搜索提供商，把用户打的字悄悄发给一个他没点名的服务不在选项里。

    **tab 只给 `web` 窗格，别的 inspect 值一个都不给。** `inspect.ts` 那条"单值槽不是 tab 容器"仍然成立，它服务的是**对照**（想同时看两样就分屏）；浏览网页要的是**切换**，同屏摊开五个网页没有意义,而每开一页裂一个窗格会把窗口撑爆。两种窗格要的是不同的东西,所以这是一次有理由的例外,不是先例——发现自己想给 diff 或 run 加 tab 时，回来读这一段。

10. **模型输出永远不许变成 markup**。`rich.tsx` 只用 marked 取 token，再**构造**白名单内的 React 元素；认不出的 token、原始 HTML token、不在协议白名单里的链接，一律按字面文本渲染。理由不是洁癖：这个 webview 里跑起来的脚本能拿到 `window.__TAURI__`，等于本机任意命令，而模型输出里天然混着文件内容、抓取的网页和 MCP 结果——按信任边界那条，它们是**观察到的数据**，不是指令。

    只有两处豁免，各自在文件里写明理由：`math.tsx`（KaTeX 的产物本身就是 markup 字符串，边界改画在 KaTeX 的 options 上，`trust: false` 是承重的那一条，且 options 被 freeze 且不接受参数）和 `src/sandbox/`（见规则 11）。`src/boundary.test.ts` 机械地扫描 `src/`，除这两处外出现 `dangerouslySetInnerHTML` / `innerHTML =` / `eval` / `new Function` 即失败；`src/rich.test.tsx` 用敌意 markdown 断言输出是文本。**新增第三处豁免之前，先确认它不能改成构造节点。**

11. **artifact 沙箱的三条不变量**（`src/sandbox/`，图表/图示/**模型在回复里手写的** HTML 都在里面渲染；磁盘上的 `.html` 文件走 11b，别混）：

    - **iframe 永远只有 `sandbox="allow-scripts"`，绝不加 `allow-same-origin`**。没有它，frame 是不透明源：`parent.document`、`parent.__TAURI__`、`localStorage` 全部抛 DOMException（实测），于是里面爱怎么 `innerHTML` 都只能毁掉它自己。加上它，这一整套设计当场归零。`rich.test.tsx` 钉住了这个属性。
    - **`tauri.conf.json` 的 `script-src` 永远不许出现 `unsafe-inline` / `unsafe-eval`**。它是规则 10 的第二道防线。（`img-src` 里的 `data:` 是给粘贴图片的缩略图开的，与规则 10 不冲突：`rich.tsx` 把模型输出里的 image token 渲染成文本 chip，模型根本没有产出 `<img>` 的路径。）
    - **沙箱里的脚本必须是经典脚本，不能是 module**。实测：不透明源下 `<script type="module">` 走 CORS，请求带 `Origin: null`，除非服务端显式放行否则**根本不执行**；经典 `<script src>` 是 no-cors，无条件可用。所以 `src/sandbox/*` 由 `vite.sandbox.config.ts` 单独构建成 IIFE 进 `public/`，不走主 module graph。改成 module 会在开发机上"看起来能跑"（若 dev server 恰好发了 ACAO）而在装出来的 app 里静默失效。

11b. **这个 app 有两个 frame，靠两种不同的机制隔离，而各自需要的属性恰好会拆掉对方。** 加第三个之前先读完这条（`boundary.test.ts` 机械地钉住"只有两个"）。

    | | `Sandbox.tsx` | `Framed.tsx` |
    |---|---|---|
    | 装的是 | 模型手写的 markup **字符串** | 磁盘上的一个**文件** |
    | 从哪加载 | app 自己的源（`sandbox.html`） | 本机 origin `http://127.0.0.1:<随机端口>` |
    | 靠什么隔离 | **没有源**（`allow-scripts` 且**绝不**加 `allow-same-origin`） | **有自己的源**，与 app 不同源 |
    | 因此 | 加 `allow-same-origin` 当场归零 | 必须给 `allow-same-origin`，报告要 fetch 自己的数据 |

    **两边的 `sandbox` 属性长得相反，而抄错一次没有任何编译或测试之外的迹象。** 把 `Framed` 那串属性贴到一个从 `'self'` 加载的 frame 上，边界当场消失，diff 看起来还完全正常——这就是 `boundary.test.ts` 里那两条按属性值（不是按注释文本）比对的检查存在的理由。

    **为什么 `.html` 文件搬出了规则 11**：它在那里从来就没工作过，而不是工作得不够好。`innerHTML` 按规范**不执行** `<script>`，加上 `sandbox.html` 继承 app 的 `default-src 'self'`，于是 plotly/bokeh/altair 的自包含产物和 CDN 产物**两种都是死的**——一个 python 报告在那条路上永远只是一个空 div。这三件事全是"字节从哪加载"的属性，不是"解析得够不够小心"的属性，所以修法只能是给它一个源（`src/serve.rs`）。

    **它确实弄脏了规则 13 的"一张表两个入口"，这条要睁眼看着**：` ```html ` 围栏仍走 sandbox，`.html` 文件走 framed，这是那张表里**唯一**一处内联与文件不同的条目。理由是这两个位置装的东西对 mermaid/svg 是同一个（同一段源换个地方），对 html 不是：围栏里的是模型手写的 markup，文件是脚本产出的文档。**但这个不对称有个真实的缺口**——模型可以 `write` 一段自己手写的 HTML 到文件再 `show` 它，那段 markup 就换了跑道。缓解写在 `serve.rs` 的 `POLICY` 上（`connect-src 'self'` + `form-action 'none'`，堵外传不堵渲染），**它是纵深不是边界**，理由同样写在那儿：能写 HTML 的前提是能写文件+能跑脚本，而那两件事都过审批，且都是更直接的出口。改这条之前先读那段注释。

12. **沙箱拿到的主题值必须是 sRGB**。主题用 OKLCH 写，而沙箱里的第三方库自己解析颜色（mermaid 的 khroma 直接拒绝 `oklch(...)`）。`Sandbox.tsx` 的 `readTheme()` 负责光栅化转换后再发过去——注意只读 `fillStyle` 是不够的，Chrome 会把 `oklch()` 原样序列化回来，必须真画一个像素读回。

13. **`show` 出来的文件与模型自己写的正文同级不可信**（`ui/src/Shown.tsx` + `src/commands.rs::shown_file`）。文件是脚本产出的、不是模型手写的，但那不抬高它的信任等级——它同样是观察到的数据。所以 `.svg`/`.mmd`/echarts option **必须走规则 11 那同一个 `sandbox="allow-scripts"` 无 same-origin 的 iframe**；`.html` 走 11b 的本机 origin（**同样不许**改用 asset 协议、给它 app 的源、或直接 `location = file://`——它拿到的是一个**第三方**源，不是放行）。图片走后端返回的 `data:` URL（CSP `img-src` 已有 `data:`），因此也不需要任何新协议。

    **路径来自 webview，所以是数据**（规则 3）：`shown_file` 不因为"模型调 show 时已经查过了"就信它，重新过一遍 `tcode_tools::is_viewable_path`——工具与命令共用**同一个** boundary 定义，两处各写一份必然漂移。`VIEWER_TEXT_BUDGET` 同理：工具说"会被截断"和 viewer 真截断，用的是同一个常量。

    **`Inspect` 的 `shown` 是唯一读磁盘的检视值**，与 `Inspector.tsx` 顶上"一切来自 blocks 而非磁盘"刻意相反：其余每一种都在回答"agent 做了什么"，重读文件会把问题换成"现在是什么"；而 `show` 的文件正是为了**不进对话**才写到磁盘的，transcript 里根本没有它的字节。因此陈旧只靠 reload 按钮说清楚，**不要上文件监听**——为一个按钮换一套常驻机制不划算。

    **未做的第二阶段**：让 option 里能写 `{"$file": "pnl.csv"}` 由前端解引用，见 `DATA-BINDING.md`（含承重约束：解引用只能在父窗口做，沙箱读不到文件）。**这条约束对 framed 的 `.html` 不再成立**——它与自己的数据同源，直接 `fetch('./pnl.csv')` 就有，不需要任何解引用协议。那份文档说的仍是沙箱里的 echarts option。

    **`ui/src/show.ts` 是 `fences.tsx` 的第二个入口，不是第二张表**：围栏语言与文件扩展名问的是同一个问题，答案重合（mermaid/html/svg/markdown）。分成两套的后果是同一张图内联写和写成文件长得不一样，而 `show` 的全部意义就是"改的只有成本，不是结果"。`.json` 是唯一按内容判定的条目（它是容器不是一种东西），其余在加载任何字节之前就定了。**第三个入口是文件树点开的文件**（`WorkspaceFile.tsx`），同表同渲染（`FileBody.tsx`），理由同上。

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

16. **用量的两笔账不许合并**（`ui/src/usage.ts` + `UsagePanel.tsx`）。**context** 是这段对话在模型窗口里占了多少（含缓存前缀——缓存过的输入照样占位置），是**每个会话**的，compact 一次就清掉一大半；**订阅额度**（5h / 周）是**整个账号**的，只有时钟能回填。两者作用域不同，所以 `Meter` 挂在 `SessionState` 上，`Limits` 挂在 App 顶层的 `LimitsContext` 上——把额度也塞进会话，结果就是新开的文件夹说"没有额度信息"，而旁边那个窗格正显示 42%。**它们也永远不许平均成一个"usage"数字**：那正好在你没时间了的那一刻告诉你一切正常。

    **`Usage` 事件是替换不是累加**：一次请求的整个 prompt 就是 `total_input()`，跨步累加会把缓存前缀按请求数重复计。回合账单（`turn`）才是累加的那个，`DelegatedUsage` 进账单不进窗口——sub-agent 花钱，但占的是它自己的窗口。这条有单测钉住。

    **算不准就说算不准**：resume 的日志里没有 token 计数，compact 之后没人知道摘要多大，`@path` 展开是在回合前就进了 prompt——这三种情况打 `estimated`，界面上是 `≈`。宁可显示 `≈68k` 也不要把猜的数字画成事实；下一次真实 usage 事件会把它换掉。

    **阈值与 TUI 同源**（context 85/95，额度 75/90）。两个前端在不同时刻变黄，等于同一个契约有两份定义。

17. **portal popover 的定位与消解只有一份**（`ui/src/seat.ts` + CSS 的 `.seated`）。窗口里每个弹出层都必须 portal + `position: fixed`：窗格会裁掉它，而 composer 那个 `<form>` 里的输入框按 Enter 会把消息发出去。于是"量触发器的盒子 / resize 时重量 / Esc 与点外面消解"这三件事在每个调用点都一样——`useSeat` 收着，各家只留自己开在哪个角、多宽。**别再抄第四份**：三份里漏掉 resize 监听的那份，只在改窗口大小时才看得出来。右键菜单是同一个弹出层的另一种锚（`at` 传视口坐标当零尺寸矩形），不是第四份实现；子菜单画在父菜单的 DOM 里而不是另开 portal，否则点子菜单会被父菜单判成"点了外面"。**浏览器让位的计数器不在这里，在 `browserYield.ts`**——分隔条拖动要用同一个计数（硬规则 21），两份计数会让一个关掉的子菜单把还开着的父菜单底下的页面放出来。

    **`mousedown` 监听必须是捕获阶段。** Tauri 自己的 `drag.js` 会在 `document` 上处理命中 `data-tauri-drag-region` 的 `mousedown` 并调用 `stopImmediatePropagation()`；所以任何复用 `DRAG` 的区域旁若有 popover 触发器，冒泡阶段的监听都会漏掉关闭事件。

    **Esc 的优先级写在 `Workspace.tsx` 里，不靠挂载顺序。** 它和 `seat.ts` 都在 window 捕获阶段，`stopPropagation` 分不开同一元素上的两个监听，先跑的是先挂的（就是窗格那个）。所以窗格的 Esc 显式让位给两样东西：开着的 `.seated`，以及正在被输入的 input/textarea（树里的重命名、编辑器里没存的改动）。**Esc 永远不许是那个把你正在写的东西扔掉的键。**

18. **"窗口怎么画"的偏好留在 webview，不进 config**（`ui/src/display.ts` + `DisplayMenu.tsx` + `rail.ts` 的项目顺序，都在 `localStorage`）。判据是这个开关能不能改变 agent 做的事：不能，就不是配置。`[tcode_state]` / `config.toml` 里每加一个字段都要同步 `tcode-config` skill（根 CLAUDE.md 的硬要求），而"显示不显示推理"错了只值一次点击，不值一条配置契约。反过来也成立：**任何影响一次 turn 的行为、发送内容或落盘内容的开关，绝不许悄悄住进 `localStorage`。**

    偏好按作用域归属，和用量那两笔账同理（规则 16）：`Display` 从 App 顶层的 `DisplayContext` 下发，所以 sub-agent 的转录跟着同一个开关走——转录递归时读的是同一个 context，不需要一处一份。什么该粘住也要想清楚：**项目顺序粘住，折叠状态不粘**——顺序是设一次就依赖的排列，折叠是为了眼下几分钟把长列表收起来，上周那个决定不该让今天的会话栏一半是收着的。

    读存下来的值时它是**数据**：每个字段逐个校验，认不出的落回默认，`localStorage` 抛异常（webview 关了存储）也必须能开窗——`loadDisplay` / `loadOrder` 都这么写。

19. **"这个会话忙不忙"只有开 turn 的那把锁答得了**（`SessionHandle::send_or_queue`）。跑着就入队、闲着就把消息**交还**给调用方去开 turn，两件事在同一把锁下完成——先问 `is_busy()` 再动作的写法，只会在"turn 刚好结束的那一瞬间打字"时丢消息，那正是最难复现也最气人的丢法。前端一律不自己判：`send_message` 返回当前队列，空数组=已开始发送，非空=这就是要画在 composer 上面的东西。

    **队列必须由一个 turn 排空，永远。** core 在安全边界 drain 覆盖常规情况；`run_turn` 结尾那次 `take_for_next_turn` 覆盖另一种——turn 没走到边界就结束了。少了它，消息会停在一个再没人 drain 的队列里：收下了、画出来了、然后永远不发。"停下并立刻发送"也走这条路（`interrupt_and_flush` 先 defer 再 cancel，顺序承重：反过来的话正在拆的 turn 会在退出途中把留给后继者的消息吞掉）。

    **撤回按位置 + 文本双验证**。队列只会整体 drain，所以陈旧下标通常落在空队列上；但"入队 → 看着它被发出 → 再入队"之后，一次迟到的撤回就会删掉用户想留的那条。文本比对是免费的，`PendingInput::withdraw` 因此收两个参数。

20. **rewind 的截断点只能是后端给的 ledger 下标**（`Session::rewind_targets` / `rewind_to`，两个前端共用）。前端**不许自己数**：转录是从 `history()`（archived ++ entries）重放的，而 core 明说 archived 段**没有任何合法的 truncate 索引**——压缩掉的那段正是 rewind 不进去的地方，所以"转录里的第 N 条 user 块"根本不是一个 ledger 下标。webview 拿后端的目标列表，按**文本顺序配对**到屏幕上的 prompt（`ui/src/rewind.ts::rewindPoints`，按 block 对象作 key，因为分组与过滤会挪动位置）。配不上就不画按钮，后端收到下标还要再验一遍——**失败模式必须是"少一个按钮"，绝不能是"截在没人指的地方"**。

    **truncate + freshness 清空 + 可选文件回滚是一个操作**（`Session::rewind_to`）。freshness 是最容易漏的那个：它记着"模型已经看过这个文件"，而那些 read 就在刚被删掉的历史里——不清空，下一次 read 会对着模型再也看不见的调用回答"你已经有了"。**文件回滚是另一个决定**，由调用方传参：忘掉说过的话重打一遍就有了，把文件推回去可能丢掉之后手改的东西。所以 UI 上那个勾**默认不勾**，而且那段历史没动过文件时**整条不出现**。

    **rewind 之后重放，不要就地截断**。webview 有四份从事件流派生的东西（转录、文件索引、用量、rewind 点），手工截四次就是四次漏掉一处的机会——所以 `rewind` 命令返回整个 `OpenedSession`，前端走已经存在且已经正确的 `replayLedger`。

21. **窗口里最贵的两样东西是转录和检视窗格，它们靠 `memo` 跳过重绘——所有上游必须为此保持引用稳定。** 这条是踩出来的：整个 app 一份 `states`，全树零 memo，于是**每一次按键、每一个流式 token、拖动分隔条的每一帧**都会把所有会话的全部转录重新渲染一遍（含每条 assistant 消息重跑一次 markdown lex）。实测 120 轮对话下一次无关的父级 state 变化要 49ms，且随对话长度线性增长——中文输入法面板疯狂闪烁的根因也是它：受控 `<textarea>` 的 `value` 在 preedit 中被重新赋值会打断合成态。落实它的四道防线，删任何一道都会静默退化回去：

    - **`App` 的回调一律 `useCallback` 且不依赖 `states`**（读 `held.current` 那个 ref），`Workspace` 的 `PaneContext` 走 `useMemo`，`Panes` 的 `SessionPane`/`InspectPane` 在组件里绑好交给子组件的闭包。**JSX 里现写一个箭头函数 = 这个 memo 不存在了**，而且不报错、只是慢回去。
    - **草稿归 composer 自己所有**（`Composer.tsx`）：按键只动本地 state，200ms 空闲、失焦、提交时才上抛；合成态期间什么都不上抛，高度只涨不缩（把高度清成 `auto` 再读 `scrollHeight` 会让光标矩形一次按键跳两次，候选窗跟着跳，那就是闪烁）。因此 **`onSubmit` 必须把文本带上**——回头去读 `states[id].draft` 会读到 200ms 前的内容。
    - **`rich()` 与 `highlight()` 各有一份按源文本的缓存**（`rich.tsx` / `syntax.ts`）。`Diff.tsx` 逐行调 `highlight`，200 行的 diff 在屏幕上就是每次渲染 20ms 的 TextMate 分词。**别把缓存挪进某个组件的 `useMemo`**：调用点有五个，以后还会更多。
    - **`blocks.ts` 的 reducer 必须复用没变的块对象**（`extend` 只换最后一个、`updateCall`/`updateRun` 的 `map` 对不匹配的原样返回）。`BlockView`/`RunCall` 的 memo 全靠这一点：一旦哪次改动开始顺手复制块，流式输出会静默退回"每个 token 重绘整场对话"。

    **转录的 key 按块在对话里的起始位置算，不按 item 下标**（`Transcript.tsx::keyOf`）。分组边界会移动——第二个 edit 一到，一个 item 变成一个两块的组，后面所有 item 往上挪一格，React 于是卸载重建改动点以下的每一步，连带丢掉所有展开状态。

    **原生浏览器窗格是这套开销的放大器**（`WebPane.tsx`）。量它的矩形要用 `requestAnimationFrame`，不能在 layout effect 里直接 `getBoundingClientRect()`：那是在一次刚提交、布局还脏的时候读，会强制整篇文档同步布局——开着这个窗格时，窗口里**每一次**渲染都要付这笔钱。**让浏览器暂时让位只有一份实现**（`browserYield.ts` 的计数器，popover 与分隔条拖动共用，见硬规则 17）；拖动分隔条时必须让位，否则每个指针采样都在让平台把另一个进程里的整页重新布局一次。

22. **读文件系统的 command 一律 `async` + `tauri::async_runtime::spawn_blocking`。** 同步 command 跑在**主线程**上（`send_message` 的注释里已经写了这条的另一半），而主线程就是画界面那条线程：`project_sessions` 曾经是同步的，于是启动台上展开一个项目（replay 那个文件夹下每一条 session log 取 preview）会把整个窗口冻住——按钮、拖动区、另外几个窗格里正在跑的会话，一起停到读完为止。debug 构建下九十条对话实测 ~250ms，release ~35ms，冷盘更久，而它**看起来完全像 UI 卡顿而不像后端慢**，因为卡住的确实是 UI。

    **是 `spawn_blocking` 不是 `spawn`**：这是文件 IO，而 `spawn` 落到的那个 runtime 正是每个 turn 跑在上面的那个，占住一个 worker 几百毫秒等于让正在对话的会话陪着一起等。

    连带一条：**picker 的 preview 不许走 `resume_path`**。`SessionStore::list` 现在用 `store.rs::preview` 逐行浅解析（`LogEvent` 是 internally tagged，反序列化一次要先把整行缓冲成一棵泛型树，等于把每条工具结果、每张贴进来的图解析两遍）。它照样是**重放**而不是扫描——`append`/`truncate_tail`/`compact` 三个操作全都实现，少一个 `/clear` 掉的对话就会复活；而"只认这三个"是安全的，因为 `Ledger` 本来就只有这三种变更（根 CLAUDE.md 的设计约束 2）。

## 现有结构

- `src/bridge.rs`：出向事件（`SessionEvent`/`TurnFinished`/`ApprovalRequest`）、入向审批（`ApprovalAnswer`/`Pending`）、`WebviewApprover`、`pump_events`。`Emit` trait 在这里。
- `src/state.rs`：`Supervisor`（agent + `SessionFactory` + 会话表 + 顺序）、`SessionHandle`（会话私有的 session/cancel/pending）、`run_turn`。
- `src/commands.rs`：Tauri command，薄封装，只做参数校验后转 `state`/`projects`。
- `src/boot.rs`：app 的 composition root，外加 `SessionFactory`（开第二个文件夹时**按该文件夹重新加载 config**，因为 `.tcode/config.toml` 是项目级的）。
- `src/projects.rs`：启动台的数据源。`~/.tcode/projects/<id>/` 的目录名是路径的**有损**变换（`store::project_id` 把非字母数字全折成 `-`），反推不回文件夹，所以真实路径只从每条 session log 首行的 `Meta{cwd}` 读——每个项目一行，够便宜；带 preview 的完整重放留给用户真打开的那个项目（`project_sessions`）。
- `tests/bridge.rs`：scripted provider 驱动真实 agent loop，断言事件流、审批往返、fail-closed、双会话隔离、忙会话拒绝第二个 turn。**测试不打真 API。**
  - `ui/src/`：`types.ts`（wire 契约）、`blocks.ts` 与 `files.ts`（事件→块树 / 事件→文件清单，都是纯函数 reducer）、`session.ts`（每个会话的 UI 状态，窗格按 id 查）、`layout.ts`（窗格树，纯函数）、`Launchpad.tsx`（第一屏）、`Workspace.tsx`（标题栏 + 可折叠的会话栏 + 窗格场）、`Panes.tsx`（窗格树的渲染与窗格外框；浏览器入口在会话窗格的文件工具组）、`FolderMenu.tsx`（窗格 header 上的身份兼文件夹选择器）、`seat.ts`（portal popover 的定位与消解，见硬规则 17）、`browserYield.ts`（让原生浏览器窗格暂时让位的唯一计数器）、`theme/`（token 契约与主题包）、`preview/`（只在 `--mode preview` 下加载的 fixture，`split` 场景是三窗格嵌套，专门用来看递归对不对；`plan` 场景是带评论与改动的审批面板；`model` 场景会自己把模型面板点开，因为静态看一遍看不到它；场景名进 URL 的 `?scene=`，所以一个状态可以直接链接、刷新也还在）。
  - **模型这一摊**：`picker.ts`（wire 类型 + 纯函数：按 profile 分组、pin 的措辞、effort 槽位）、`ModelPanel.tsx`（面板本体，见硬规则 15）、`Chips.tsx`（composer 下面那条：模式菜单 + 用量环 + 面板的触发 chip）。`mock-core.ts` 里的 picker fixture 是**可变的**，与其他静态 fixture 不同：这个面板的验收标准就是"选完能回读"，一个永远回答 `Opus 5 · high` 的 fixture 演示不出来。
  - **用量这一摊**：`usage.ts`（两笔账的类型 + 纯 reducer + 措辞：token 缩写、窗口名从 `window_minutes` 推、reset 倒计时）、`UsagePanel.tsx`（strip 上那个环 + 展开的面板）、`session.ts` 的 `LimitsContext`。见硬规则 16。窗口大小走 `picker_state.context_window`（读活着的 `ModelCell`，不是配置默认值——理由同 effort）。
  - **计划这一摊**：`plan.ts`（类型 + 全部纯操作：改/增删/换序/状态循环、计划条的行、`planChanges` 结构化比对）、`selection.ts`（选区→引用，纯函数）、`PlanEditor.tsx`（审批面板与计划窗格共用同一个编辑器、同一份 draft）、`ProgressStrip.tsx`（composer 上方那一行）、`components/SelectionBubble.tsx`。**编辑靠 id 认行**（`DraftPhase.id`，只在一次编辑里有效、不落盘），所以"改名"是改名而不是"删一个加一个"；按标题匹配那条路恰好在用户最认真的时候错得最狠。
  - **`blocks.ts` 不叫 `transcript.ts`**：那个名字与 `Transcript.tsx` 只差大小写，在不区分大小写的文件系统上 tsc 会把两个 import 解析成同一个文件，Windows 上直接构建失败。同理，新增模块别取只和某个组件差大小写的名字。
  - **块是树**：`run`（sub-agent）与 `batch` 持有子块。`TaskRunEvent` 裹的是一个完整的 `AgentEvent`，所以 sub-agent 的内容就是同一个 reducer 递归一层——嵌套 run 不需要任何额外代码。`runPairs` / `reportOf` 是这棵树上的两个纯查询，把委派调用和它的 run 认成一步（见硬规则 9f）。
  - **会话栏这一摊**：`rail.ts`（纯函数：按 cwd 分组、项目换序、从第一条 user 块取会话标题 + 顺序的 `localStorage` 存取）、`Workspace.tsx` 的 `RailProject`。**会话名不是文件夹名**：会话名等于文件夹名，所以同一个文件夹开两个会话就是两条一模一样的行，会话栏能把它们都列出来却说不清哪个是哪个。文件夹上升成分组标题，行上写"这个会话是被要求做什么的"（第一条 prompt，不是最后一条——对话是为它开的那件事而存在，跟着最新消息改名等于每打一行字就改一次名）。换序用 Alt+方向 + hover 出来的两个按钮，与 `PlanEditor` 同一套词汇；**存下来的顺序只记被移动过的文件夹**，没记过的按到达顺序排在后面，这样新开一个文件夹不会把已经排好的打乱。
  - **文件树是"往哪儿去"的那个窗格，不是"看什么"的那个**（`layout.ts::browsing` / `browserPane`）。`openInspect` 复用 inspect 窗格时**跳过正在浏览的那个**，所以点一个文件是开在树旁边而不是把树顶掉，第二次点复用第一次开出来的那个窗格——"点击换文件、树不动"就是这一条，没有 tab 条也没有 preview 槽。`openAside`（右键 open in → 一个新窗格 / Mod+点击 / Mod+Enter）永远新分裂一个，这是"再开一个"这个明确的动作，和会话的 `show` / `showBeside` 是同一组区分。从树里分裂出来的窗格默认给树 `BROWSER_SHARE`（0.34）而不是一半：一列文件名和文件本身不是同量级的东西。
  - **外部打开的两条边界**（`src/openers.rs`）：webview 送的是**表里的 id**（`vscode`/`cursor`/`zed`/`reveal`），永远不是命令行——认不出就报错，不猜（规则 3）；路径先过 `Workspace::host_path`（与读写同一套 link/canonical 检查）再交出去。Windows 上 `code` 是 `.cmd`，`CreateProcess` 跑不了批处理，只能过 `cmd.exe`，所以**路径里含 cmd 元字符时整条拒绝**而不是加引号赌一把——仓库里允许有叫 `a&b.txt` 的文件，这个 app 不允许去跑它。检测是文件系统探测（PATH + 各平台固定安装位置），没装的编辑器**不进菜单**，而不是进了菜单点了才失败。
  - **一个 inspect 窗格是单值槽，不是 tab 容器**（`inspect.ts`）。它只持有一个 `Inspect` 值，栈底是文件索引；转录里的路径、文件行、run、artifact 全都只调 `open(...)`。前进/后退因此是白拿的，新增一种可检视的东西 = 加一个 `kind`，不是加一个 tab。**分屏没有削弱这条，反而更纯粹**：想同时看两样东西就分两个窗格，不是在一个窗格里长出 tab 条。每个会话只复用一个 inspect 窗格（`openInspect`），所以连开五样东西是五条历史记录而不是五个窗格。
  - **两张前端注册表**，同构于 core 的 `Tool`/`SlashCommand`/`ToolRenderer`：`fences.tsx`（围栏语言→富渲染，未命中落到高亮代码块）与 `toolViews.tsx`（工具→怎么画 + 点开看什么）。**路由不在前端定**，`tool_views()` 从后端拿，`route`、`quiet_output` 与 `display_name` 都由活着的工具派生（`Tool::route` / `Tool::batch_policy` / `Tool::display_name`），名字表已经删了。**一个工具的行只说得清"哪个工具、干什么"时，缺的那一半属于这张注册表**：core 的 `summarize_call` 只认一组固定的参数键，`skill` 的参数叫 `name` 不在里面，于是每次加载 skill 在转录里都只是光秃秃一个 `skill`——补法是给它一个 `ToolView.summary`，不是去改 core 那组键的语义。（TUI 的**工具调用**路径至今是同一个洞、同一个原因；它的 `/名字` **斜杠**路径不受影响，那条走 `parse_skill_echo` + `skill_echo_lines`，名字本来就在。两条路径不是一回事，别拿其中一条的表现推另一条。）

    **`summary` 回答"哪一个"，参数属于 `detail`。** skill 的 `name` 与 `arguments` 拼成一个字符串时读者看不出名字在哪儿结束（`impeccable audit the trace column`），而行要回答的问题是"加载了哪个 skill"。参数走 shell 命令用的那同一个折叠位。唯一的例外是 `progress`：它的路由**随 input 变**（阶段翻转归计划条，提交的计划是对话要留住的文档），后端送的是工具的默认答案，那一个例外由 `plan.ts::isPlanSubmission` 在调用点认出来——一个字段，紧挨着它读的类型。
  - **批次里的调用没有 summary**：core 的三条并发路径（parallel read / mutation lanes）只发 `ToolBatchStart`，不为每个调用发 `ToolStart`，所以 `(call_id, name, input)` 之外什么都没有。`toolViews.tsx` 的 `describe` 从 input 里取目标补上——展开一个批次看到五行 `read` 而不知道读了什么，等于批次白折叠。真正的修法是 core 把它已经算好的 `summarize_call` 一并放进 `ToolBatchStart`。
  - **`show` 的三个文件**：`show.ts`（扩展名→怎么画的注册表 + CSV 解析，纯函数、有测试）、`Shown.tsx`（唯一读磁盘的检视视图，见硬规则 13）、后端的 `commands.rs::shown_file`（哑字节服务：**要 text 还是 `data:` URL 由前端那张表说了算**，后端不再判一次，否则就是两张要同步的表）。
  - **`show.ts` 的 `Load` 有三个值不是两个**：`text` / `bytes` / `served`。第三个的意思是**根本不读**——frame 自己去 origin 取，所以 `Shown.tsx` 对它跳过 `shown_file`。这不是省一次 IO：走读的那条路会在 `VIEWER_TEXT_BUDGET` 上把一份 10MB 的自包含报告截断，然后把残片交给一个跑不了它脚本的渲染器。**新加一种"前端不该读"的文件时改这个字段，不要在 `Shown.tsx` 里加 `if`。**
  - **`Framed.tsx` 的 reload 是 React `key`，不是 URL 上的 cache-buster**。跨源没法叫文档自己刷新，所以把手只能是元素本身：换 key → 卸载重建 → 重新请求 → `no-store` 让它是一次真读。写成 `?v=N` 对真 server 也work，但对任何不吃 query 的 URL 就碎了——设计预览里那个 `blob:` 正是这种，这条就是这么发现的。渲染本体在 `FileBody.tsx`（表格只把有限行放进 DOM，`ROW_STEP` 不是分页而是上限——20 万行 DOM 就是一个冻住的窗口）。
  - **文件树点开的文件是那张表的第三个入口，不是第四种画法**（`WorkspaceFile.tsx`）。它和 `Shown.tsx` 共用 `FileBody`，所以一个 `.svg` 是模型 show 出来的还是有人从树里点开的，画出来一模一样。它曾经是 `WorkspaceEditor`：所有文件一律开在灰 textarea 里，旁边挂一个 `source / preview` 开关——而"preview"这个区分只有 Markdown 有，`.rs` 点它没有意义，`.png` 后端根本读不出来（`Workspace::read` 只认 UTF-8）。

    **两态是"看"和"改"，不是"源码"和"预览"。** 渲染是静止态（打开一个文件的窗格就是被要求把它显示出来），改是一次点击（改是一个决定）。**读的时候不许有输入框的外观**——沉底的灰井是"这里可以打字"的承诺，静止态给这个承诺就是承诺错了东西；井属于 textarea，跟它一起出现。哪些文件连这个开关都没有也由同一张表说了算：`isBinary` 认下的以 `data:` URL 到达，那是一张图片，图片没有源码可以放进 textarea。

    **二进制走 `Workspace::read_binary`，不许改道去 `shown_file`。** 两者的"在不在工作区里"是两套定义（前者按 root 解析且每一段都不许是 link，后者按 `is_viewable_path`），图片能读了却换一条边界进来，就是给同一句话两个意思。`.ico` 之类新扩展名要同时进 `show.ts` 的表和 `commands.rs::media_type`。
  - **粘贴/拖入的图片**走 `paste.ts`（长边 1568 以上重采样，模型本来也只看这个分辨率）→ `send_message` 的 `images` → `commands.rs::compose`。模型不支持 vision 时图片**存进 scratch 并告诉模型路径**，不静默丢——用户贴了个东西，丢掉它等于让人对着一张谁也没有的图提问。
  - **语法高亮：语法来自 Shiki，配色一律不来自它**（`syntax.ts`）。这里曾经是一个手写扫描器，理由是任何库都自带调色板、是字面值、主题包改不动它，等于在"chroma 只表示状态"的界面里塞第二套配色——那条理由至今成立，变的是划线的位置。现在交给 Shiki 的**主题根本不是调色板**：`mark()` 把八种 kind 各涂一个哨兵色（`#000001`…），`kindOf()` 再把它读回来，跨过这趟旅程的只有一个下标。到 DOM 的仍然是 `tok-*` class，值仍然在 `base.css` 的 `--syn-*` 契约里。

    三条别改坏：**只调 `codeToTokens`，永远不调 `codeToHtml`**——前者返回数据，后者返回 markup，规则 10 在这里是靠类型成立的，不是靠 review。**scope→kind 的表是我们自己的**，不是 Shiki 的 `css-variables` 主题（它把对象属性和数字归一堆、把标签名和关键字归一堆，跟这个 app 画它们的方式不一样）。**语法是异步按语言懒加载的**，所以 `highlight()` 在拿到之前返回 `null`，调用方拿 `useGrammar` 订阅、这期间画纯文本——一段没上色的代码是完整可读的，为它画个 loading 比等它更差。
- `src/serve.rs`：`.html` 报告的本机 origin（见 11b）。绑 `127.0.0.1:0`，每个 root 一个不可猜的 token 前缀，边界**复用 `tcode_tools::viewable_within`**——那是 `is_viewable_path` 的同一条规则换个返回值（落在哪个 root 下 + 相对路径），**不许在这里写第三套"在不在工作区里"**。相对路径不能用 `strip_prefix` 自己算：containment 是逐 component 且 Windows 上大小写不敏感的，`c:\proj` 与 `C:\Proj\x.html` 正是它接受而 strip 返回 `None` 的那一对。
  - **文件服务本体是 `tower_http::ServeDir`，不是手写的**。Range（视频拖动、PDF viewer）、条件请求、HEAD、目录 index、按扩展名的 MIME 全在它那儿，而这些正好是一份生成报告会逐个踩到的东西。**MIME 错一个就是一次"就这个报告显示不出来"**：`.js` 不是 `text/javascript` 时 module script 直接被拒，`.wasm` 不是 `application/wasm` 时 `instantiateStreaming` 失败——`a_report_and_everything_it_pulls_in` 按值钉住了这几个。
  - 三个自己加的响应头，各有理由：`no-store`（否则 `Shown.tsx` 那个 reload 按钮是个不做事的按钮）、`text/*` 补 `charset=utf-8`（`mime_guess` 只给裸 `text/html`，中文报告靠浏览器猜编码就是乱码）、`POLICY`（见 11b，堵外传）。
  - `Serve` 起不来**不是致命错误**（`ServeHandle` 带着失败原因走）：Windows 上安全软件拦本机监听是真实存在的，为一类文件显示不了而让整个 app 开不了是拿工具换功能。失败时报告窗格说清原因，其余一切照常。
- `src/openers.rs`：把工作区里的一个路径交给外面的程序（编辑器 / 文件管理器）。表在这里，边界见上面那条。
- `src/paths.rs`：`canonical_dir`——app 里唯一一处把用户选的文件夹变成键的地方（见硬规则 9）。
- `capabilities/default.json`：webview 的权限授予（见硬规则 6）。现有 `core:default` + `dialog:allow-open`；窗口控制走系统标题栏，不向 app webview 授予不再使用的 window 能力。
- `icons/`：由 `icons/mark.svg` 用 `rsvg-convert` 生成，改标记要重新导出全部尺寸。

## 已知限制

**多文件夹会话共用一条 `ShellFilters` 链。** 它是 boot 时建的、被 agent 的工具集持有，`open_folder` 只能把同一个 `Arc` 再注册一次。后果：A 项目 `.tcode/filters.toml` 里的 shell 输出过滤规则会作用到 B 项目的 shell 输出。影响面是输出裁剪，不涉及权限或安全边界，所以没为它改 core；但这条**不是**"每会话隔离"，别在它上面叠新假设。真要修，得让 shell 工具按 `ToolCtx` 取 filter 链而不是在构造时捕获。

## 排查手册

界面没反应时，**先看 stderr**，它把"没跑起来 / 跑完了但前端没收到"分得很清楚：

- 无 `turn started` → command 里的 spawn 没起来。
- 有 `turn started` 无 `turn finished/failed` → 卡在 provider 请求。
- 两行都有但界面空 → 前端监听侧。九成是 capabilities 或某个没接 catch 的 promise。
- `could not emit '…'` → 事件名非法（Tauri 只收 `[a-zA-Z0-9-/:_]`）或窗口已关。

**报告是空白的**，先看启动那行 `viewer origin on http://127.0.0.1:<port>`：

- 没有这行 → server 没起来（`ServeHandle` 里有原因，也进了 warnings）。窗格会自己说出来。
- 有这行 → origin 是好的，问题在 frame。直接 `curl` 那个 URL 就把"服务端发的对不对"和"浏览器画得对不对"分开了。
- 报告显示但图是空的 → 十有八九是它自己的脚本报错，不是这条链。webview 的 devtools 里那个 frame 有自己的 console。
- 报告在浏览器里打开正常、在这里空 → 看它是不是在 `fetch` 外部地址：`POLICY` 的 `connect-src 'self'` 会挡，而这是**故意**的（11b）。
