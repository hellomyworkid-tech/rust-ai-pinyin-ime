# AI 拼音 · iOS

iOS 版由四个 target 组成（`project.yml`，XcodeGen 生成工程）：

| target | 内容 |
| --- | --- |
| `AIPinyin` | 宿主 App（SwiftUI，`App/`）：首页、AI 选词（下载本地模型）、输入设置、关于 |
| `AIPinyinKeyboard` | 键盘扩展（`Keyboard/` + `Shared/`），链接 Rust 引擎 `Frameworks/ImeFFI.xcframework`；完整词库只打进调试构建（见下文「词库」） |
| `AIPinyinTests` | 单元测试，不挂宿主；键盘的纯逻辑和真引擎（Rust）直接编进测试包 |
| `AIPinyinUITests` | 冒烟测试：在宿主 App 里用 AI 拼音键盘打字并截图，逐屏走一遍宿主 App |

键盘里跑的是真正的输入法引擎：仓库里的 Rust 状态机（`crates/ime-session`），经 C 接口
`crates/ime-ffi`（头文件 `crates/ime-ffi/include/ime_ffi.h` 就是接口约定）编成静态库，由
`Keyboard/Engine/RustEngine.swift` 包成 `IMEEngine` 协议。解码、规则重排、个人词频、上文记忆、
本地模型打分都在 Rust 里，和 Windows 版是同一套代码。

## 键盘的结构

```
Keyboard/
  KeyboardViewController.swift   入口：根视图是普通 UIView，首帧之后才建引擎、接 AI
  ProxyTextOutput.swift          把 textDocumentProxy 包成 TextOutput
  PrivacyInfo.xcprivacy          键盘扩展的隐私清单
  Engine/
    IMEEngine.swift              界面与引擎之间唯一的边界（协议）
    RustEngine.swift             真引擎：C 接口 + JSON；回调一律转回主线程
    IMEEngineJSON.swift          引擎 JSON 的解码（以 Tests/Fixtures 为准）
    EngineFactory.swift          唯一创建引擎的地方：按「App 下载的完整词库 → 调试构建自带的完整词库 →
                                 包里的基础词库」找词库，建 RustEngine；都没有或建不起来时 Release 用
                                 PassthroughEngine（只能打英文，不显示假候选），调试构建用 MockEngine
    MockEngine.swift / MockLexicon.swift / MockSegmenter.swift   假引擎，只给测试和没有词库时的界面调试用
  Logic/                         不依赖界面的规则，全部有单元测试
    InputController.swift        按键规则：字母进引擎、标点先上屏整句、上文维护、中英文、
                                 候选「加载更多」、AI 结果什么时候刷新到界面
    CandidatePager.swift         候选栏和候选网格共用的连续候选列表
    PunctuationMap.swift         中文标点映射与引号配对
    ContextTracker.swift         本地维护的光标前上文（最多 64 字）
    KeyboardLayout.swift / KeyboardGeometry.swift / GridPacking.swift   键位、排布、候选网格排法
    ShiftState.swift / FieldTraits.swift / CompositionModel.swift
  AI/
    ChoiceModel.swift            选词模型协议（测试里换假模型）
    AppleChoiceModel.swift       FoundationModels 引导生成选词（只用端侧模型）
    AppleModelChooser.swift      一次一个请求、新请求取消旧请求、引擎作废的请求立即停下、1.5 秒超时
    LocalModelLoader.swift       本地 GGUF 模型：后台加载、换模型先卸旧的、加载前一刻查内存（110MB 门槛）、
                                 内存警告卸载
    AIController.swift           按 IMESettings.aiMode 接线
  UI/                            UIKit 视图，手动排布，无 SwiftUI
```

几条和引擎打交道的规则（细节见 `ime_ffi.h` 和各文件的注释）：

- **候选分页**：引擎始终停在第 0 页。候选栏、候选网格往后滚动时用 `candidates(from:count:)`
  （`ime_engine_candidates`，不改引擎状态）取更多候选接在后面，所以空格上屏的总是看到的第一个候选。
- **AI 结果到达的时机**：手指正按着候选、已经往后加载过候选、或者展开了候选网格时，结果先压着，
  等这些结束（或下一次视图变化）再刷新，不把正在看的列表抽走。
- **上文**：一段输入开始时问一次宿主（`documentContextBeforeInput`），之后本地维护。宿主给 nil
  （空输入框）就传 `""`；传 nil 的意思是「沿用引擎记下的、之前输入框里的上屏历史」。
- **个人词库**：在键盘自己的容器里（`Application Support/AIPinyin/user_dict.json`），每段输入结束后
  后台写回。键盘每次出现时调用 `reloadUserDictionaryIfChanged()`：别的宿主里的键盘进程写过就重新读入，
  免得这里下次写盘把那边学到的词盖掉。
- **设置**：App 写 App Group 里的 `settings.json`，键盘每次出现时读。逐个字段解码：缺字段、
  读不懂的值只让那一个字段回到默认值（加字段的新版本照样读得懂旧文件）。

## 宿主 App 的结构

```
App/
  AIPinyinApp.swift / AppDelegate.swift / AppModel.swift   入口、后台下载唤起、组件接线
  Assets.xcassets                AppIcon（目前是生成的占位图，见「已知情况」）
  PrivacyInfo.xcprivacy          App 的隐私清单
  Core/                          不依赖界面的逻辑，编进单元测试
    SettingsStore.swift          设置的唯一写入口：每次改动立刻 IMESettings.save() 写进 App Group
    ModelDownloadManager.swift   下载状态机（开始 / 进度 / 暂停 / 继续 / 失败 / 校验 / 取消 / 删除）
    BackgroundDownloadTransport.swift   后台 URLSession 通道（续传数据、退到后台或被杀后继续）
    DownloadTypes.swift          状态、事件、传输层协议、空间检查、选中模型的规则
    SimulatedTransport.swift     调试用假通道（仅 DEBUG）
    AppleModelStatus.swift / KeyboardInstallMonitor.swift / NetworkMonitor.swift
  Views/                         四个页面，加上 UI 测试用的宿主 HostTestView（仅 DEBUG）
```

- **模型文件落地**：后台会话下完先放进 App 自己的暂存目录（`<id>.download`）→ 校验字节数与 `ModelCatalog` 一致 →
  改名为 `<id>.verify`，在后台线程上核对 SHA-256（`ModelCatalog` 里记着 Hugging Face 公布的哈希；这段时间界面显示
  「正在校验文件」，并用 `UIApplication` 的后台任务向系统要一段运行时间，免得 App 被后台下载唤醒后没核对完就被挂起）→
  挪到 `Models/<文件名>.part` → 改名为 `Models/<文件名>`。键盘只认最后这个名字，所以读不到半成品。
  传输层迟到或重复的完成事件、下一次下载完成都只碰 `<id>.download`，碰不到正在核对的文件；
  核对期间取消或删除时文件随即删掉，那次核对的结果回来后只丢掉结果，不再碰这个路径（那里可能已是重新下载的文件）。
  `Models` 目录和暂存目录都排除备份。启动时清掉 `Models` 里遗留的 `.part`；暂存目录里留下的 `<id>.download` /
  `<id>.verify`（核对到一半进程被杀，或者完成事件还没交到管理器手里 App 就退出了）：模型已经装好的直接删掉，
  否则重新核对大小和 SHA-256，通过就装上，不通过就删掉。已经在 `Models` 里的文件启动时只看大小，不再算哈希。
  调试用的假下载通道写的是空数据，用它时只核对大小。
- **键盘是否已添加**：读系统的 `AppleKeyboards` 列表（iOS 只提供「已添加」，看不出「正在使用」，
  也看不出「允许完全访问」是否开启，首页的文案按这个事实写）。
- **调试开关**（仅 DEBUG）：启动参数 `-simulateDownloads` 用假通道下载（不联网）。这时模型目录和暂存目录都换成
  App 自己临时目录里的 `tmp/SimulatedDownloads/{Models,Staging}`，不碰 App Group：假模型键盘、真机评测和以后的正常启动
  都看不到，App Group 里真正下载好的模型在这个模式下也显示为没下载。设置用的是真的那份：
  还没选本地模型时，假模型下完照样会被选中；以后正常启动时，如果 App Group 里没有这个模型，本地模型那条路
  只按词频选词，要在「AI 选词」页里点一次「使用」（App Group 里有同名的真模型时则照常可用）。
  在这个模式下删掉选中的假模型，也会改掉真设置里的选择；
  环境变量 `SIM_DOWNLOAD_SECONDS`、`SIM_DOWNLOAD_FAIL=<模型 id>`、`SIM_DOWNLOAD_SHORT=<模型 id>`、
  `SIM_EXPENSIVE=1`（按蜂窝网络处理）、`SIM_FREE_BYTES=<字节>`（假装剩余空间）。
  启动参数 `-hostTest` 显示键盘冒烟测试用的宿主（几个类型不同的输入框），由 `HostTestSetup` 准备设置，见「测试」。

## AI 选词的三种方式

`IMESettings.aiMode`，在 App 的「AI 选词」页里选：

| 模式 | 谁来选 | 说明 |
| --- | --- | --- |
| `apple`（默认） | Apple 端侧模型（FoundationModels） | 引擎把上文、拼音、候选交给宿主（`onChoiceRequest`），`AppleModelChooser` 去问模型、交回答案；不占键盘内存，只在支持 Apple Intelligence 的设备上可用。只用 `SystemLanguageModel.default`，不走 Private Cloud Compute |
| `local` | 本地 GGUF 模型（llama.cpp，在键盘进程内） | App 里下载模型，键盘首帧之后在后台队列加载（mmap，权重不算进键盘内存），引擎自己打分 |
| `off` | 只用规则重排 | |

规则重排始终开着、同步完成；模型的结果异步到达。引擎里两种后端都装上时只用本地模型，所以 `AIController`
在 Apple 和关闭模式下会把引擎里的本地模型卸掉（哪怕加载器记的状态是加载失败）。

本地模型的内存：键盘扩展超过约 177MB 会被系统杀掉。加载前（在加载队列上、已有模型先卸掉之后）
`os_proc_available_memory()` 低于 110MB 就不加载，工具栏显示「内存不足」；收到内存警告立刻卸载，
本次显示期间不再重试。模拟器里键盘扩展没有内存上限（`os_proc_available_memory()` 为 0），
`LocalModelLoader.systemAvailableMemory` 在模拟器上当作不受限，所以模拟器里也能加载、能测。

**调试构建的内存读数**：DEBUG 构建在键盘工具栏状态行末尾显示「已用 / 上限 MB」。已用是
`phys_footprint`（`ime_phys_footprint()`，系统判定是否超限的那个数），上限 = 已用 + `os_proc_available_memory()`；
模拟器没有上限，只显示已用。

## 构建

需要 Xcode 27.2、XcodeGen 和 Rust（`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`）。

### 1. Rust 引擎和词库：`scripts/build-rust.sh`

```sh
cd platforms/ios
scripts/build-rust.sh            # 或 scripts/build-rust.sh --no-llama：不含本地模型
```

脚本做三件事：

1. 校验词库 `data/compiled/merged_lexicon.bin`（AIMELX03；可用环境变量 `IME_LEXICON` 换一个），
   拷到 `Resources/merged_lexicon.bin`。没有编译好的词库时先在仓库根目录运行
   `cargo run --release -p ime-lexicon --bin lexicon-builder`。
2. 为 `aarch64-apple-ios` 和 `aarch64-apple-ios-sim` 构建 `ime-ffi`（默认带 `--features llama`）。
   带 llama 时需要 `third_party/llama/{ios-arm64,ios-arm64-simulator}/lib/libime_llama.a`：
   先在仓库根目录运行 `scripts/build_llama.sh ios ios-sim`。脚本把 `CARGO_TARGET_DIR` 固定为仓库的 `target/`，
   外面设了别的目录也不会打包到旧库。
3. 打包 `Frameworks/ImeFFI.xcframework`（静态库 + `ime_ffi.h`，模块名 `ImeFFI`）。

两个产物都不进版本库。改了 Rust 代码、llama 库或词库之后重跑脚本，再构建 Xcode 工程。
头文件变了之后，Xcode 有时报 `ime_ffi.h has been modified since the module file … was built`：
删掉 `build/DerivedData/Build/Intermediates.noindex/SwiftExplicitPrecompiledModules`（和 `build/DerivedData/ModuleCache.noindex`）再构建。
`RustEngine` 创建时核对 `ime_ffi_abi_version()` 与头文件里的 `IME_FFI_ABI_VERSION`。
键盘 target 有一个构建前检查：缺 xcframework（调试构建还有词库）时构建直接失败并说明原因；Release 构建结束前
查一次产物：打进了完整词库就报错，没有基础词库就警告。

### 词库：首次启动时下载

完整词库（`merged_lexicon.bin`，58 MB）由雾凇拼音编译而来。雾凇拼音是 GPL-3.0-only，所以 Release 构建
不把它打进安装包（`project.yml` 里键盘 target 的 `EXCLUDED_SOURCE_FILE_NAMES`），由 App 第一次打开时下载
（背景见 `ime-ios-research/词库协议调研.md`）：

- **发布包**：`scripts/package-lexicon.sh <版本>` 先校验词库，再用系统的 `compression_tool` 压成 xz（约 12 MB），
  解压回来逐字节比对，写出 `build/lexicon-release/<版本>/manifest.json`，并打印要抄进
  `Shared/LexiconCatalog.swift` 的大小和 SHA-256；同时打一个自成一体的源码包（编译用的全部词表、编译工具
  `crates/ime-lexicon` 的源码、GPL-3.0 全文），并把它解到临时目录离线重编一遍，核对编出的文件逐字节相同。
  两个文件上传到 GitHub Release（标签 `lexicon-<版本>`，不要设成 Latest，免得盖掉 Windows 版的发布）。
  版本号形如 `2026.11.12`，同一天第二版写 `2026.11.12.2`。当前是 `lexicon-2026.11.12.2`（2026-11-12 发布，
  编译结果可复现）；`lexicon-2026.11.12` 是它之前的一版，留着没删。
  注意：发布之前访问过的下载地址，GitHub 会把 404 缓存约一分钟，所以先发布、再装要下载它的构建。
- **App**：`AppModel.ensureLexicon()` 在 App 打开、回到前台时检查，没装好就下载（另一条后台 URLSession，
  `BackgroundDownloadTransport.lexicon`）。下载管理器和本地模型共用（`DownloadManager`）：核对压缩包的大小和 SHA-256，
  后台解压，再核对解压后的大小和 SHA-256，改名成 `App Group/…/Lexicon/merged_lexicon-<版本>.bin`。
  首页的「词库」一栏显示进度、失败原因和重试。新版本装好后删掉旧版本。
- **键盘**：`EngineFactory.lexiconSource()` 按「下载的完整词库 → 调试构建自带的完整词库 → 基础词库」找。
  键盘每次出现时再找一次，词库变了（比如刚下完）就丢掉旧引擎、按新词库重建（`InputController.keyboardWillAppear`），
  本地模型跟着重新加载。没有完整词库时，空闲时的状态栏提示去 App 里下载。
- **基础词库**：AOSP 拼音输入法的词库（Apache-2.0，6.5 万词，2009 年的数据，新词较少）由
  `tools/lexicon/aosp_to_rime.py` 转成 `data/fallback/aosp_pinyin.dict.yaml`（来源和校验值见 `data/fallback/README.md`），
  `scripts/build-fallback-lexicon.sh`（`build-rust.sh` 也会调用）编译成 `Resources/fallback_lexicon.bin`（4.4 MB），
  打进所有构建。完整词库下完之前键盘用它，状态栏提示去 App 里下载完整词库。
- **协议全文**：打进安装包的第三方内容（AOSP 词库、Unicode CLDR、llama.cpp）的协议文本在 `App/Licenses/`，
  「关于」页的「协议全文」里能看。
- **调试**：调试构建的键盘自带完整词库，App 不自动下载；但以前下载过（App Group 里有任何版本）就照样自动更新到
  当前版本。启动参数 `-forceLexiconDownload` 强制下载，`-lexiconURL <地址>` 换下载地址（比如本机
  `python3 -m http.server` 起的服务）。跑 UI 测试前清掉模拟器 App Group 里的 Lexicon 目录，免得测试中途联网下载。
- **换词库**：`data/compiled/merged_lexicon.bin` 换了之后，要重新生成 golden 基线（`verify-all.sh` 里的 golden 一步），
  `Tests/Fixtures/update_nihao.json` 这类「状态机完整输出」的 fixture 也可能要重生成（同分的生僻字换了位置）。

### 2. Xcode 工程

模拟器构建不需要签名团队：`CODE_SIGN_IDENTITY=-` 会把 App Group 权限写进模拟器二进制
（`CODE_SIGNING_ALLOWED=NO` 则不会，App Group 就拿不到）。xcframework 只有 arm64，模拟器构建排除了 x86_64。

```sh
xcodegen generate
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyin \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' \
  -derivedDataPath build/DerivedData CODE_SIGN_IDENTITY=- build
```

装到模拟器：

```sh
xcrun simctl boot 'iPhone 18 Pro'
xcrun simctl install 'iPhone 18 Pro' build/DerivedData/Build/Products/Debug-iphonesimulator/AIPinyin.app
```

## 在模拟器里启用键盘（脚本）

```sh
scripts/enable-keyboard.sh 'iPhone 18 Pro'
```

脚本直接写模拟器的偏好，省掉 设置 → 通用 → 键盘 → 添加新键盘 的手动步骤：

- `defaults write -g AppleKeyboards`：已启用键盘列表，第三方键盘写扩展的 bundle id `com.zengji.aipinyin.keyboard`；
- `com.apple.keyboard.preferences KeyboardsCurrentAndNext`：把 AI 拼音设为当前键盘；
- `AutomaticMinimizationEnabled = false`：模拟器认为接着硬件键盘时会把软键盘收成小条（画在屏幕外），关掉它；
- 最后重启模拟器。

在 iPhone 18 Pro 和一台新开的 iPhone 17e 上验证过：装好 App、跑脚本，键盘就是当前键盘，不用点任何东西。
「允许完全访问」不用开，键盘的一切功能在没有完全访问时都能用（只是没有按键音和触感）。

## 测试

先跑过 `scripts/build-rust.sh`：单元测试里的真引擎测试从源码目录读 `Resources/merged_lexicon.bin`
（模拟器看得到 Mac 的文件系统），没有词库就跳过。

```sh
# 单元测试（模拟器上跑，不需要启用键盘）
TEST_RUNNER_IME_LLM_TEST_MODEL=/path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyin \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' \
  -derivedDataPath build/DerivedData CODE_SIGN_IDENTITY=- \
  -only-testing:AIPinyinTests test

# 冒烟测试（先跑 enable-keyboard.sh），截图写到 build/screenshots/
TEST_RUNNER_SCREENSHOT_DIR=$PWD/build/screenshots \
TEST_RUNNER_HOST_TEST_MODEL=/path/to/qwen2.5-0.5b-instruct-q4_k_m.gguf \
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyin \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' \
  -derivedDataPath build/DerivedData CODE_SIGN_IDENTITY=- \
  -only-testing:AIPinyinUITests test
```

`TEST_RUNNER_` 前缀的环境变量由 xcodebuild 去掉前缀后交给测试进程：

| 变量 | 用在哪 | 没有设置时 |
| --- | --- | --- |
| `TEST_RUNNER_IME_LLM_TEST_MODEL` | `RustEngineAITests` 的本地模型测试：经 `LocalModelLoader` 加载 GGUF，在「我对天发誓这是我的」后打 shiyan，模型把「誓言」排到首位；再模拟内存警告卸载 | 这条测试跳过 |
| `TEST_RUNNER_HOST_TEST_MODEL`（可选 `TEST_RUNNER_HOST_TEST_MODEL_ID`，默认 `qwen2.5-0.5b-q4km`） | `KeyboardSmokeTests.testLocalModelPicksByContext`：`HostTestSetup` 把模型拷进 App Group 的模型目录、选中它，键盘真的在扩展进程里加载 | 这条测试跳过 |
| `TEST_RUNNER_SCREENSHOT_DIR` | 两组 UI 测试的截图目录 | 不写截图 |

单元测试里和引擎有关的部分：

- `RustEngineTests`：真引擎的按键、选词、音节、分页（`candidates(from:count:)` 不翻页、加载更多后空格上屏首选）、
  设置、上文、个人词库存取和跨引擎重新读入、按键耗时；
- `RustEngineAITests`：真引擎上的 AI。宿主选词那条路把 Apple 模型换成 `FakeChoiceModel`
  （请求、作答、重排、作废通知、上屏后选词器立即停下、浏览候选时压着的结果），本地模型那条路用上面的 GGUF；
- `InputControllerTests` / `MockEngineTests` / `AIWiringTests` / `AppleModelChooserTests`：按键规则、假引擎、
  AI 接线（换模型、加载失败、模式切换都不留模型）、选词调度，用记录型假引擎和假模型，不依赖词库。

UI 测试里，宿主 App 以 `-hostTest` 启动时 `HostTestSetup` 先把设置写成已知状态：`HOST_TEST_AI_MODE=off|apple|local`
选 AI 方式，`HOST_TEST_MODEL` / `HOST_TEST_MODEL_ID` 放入并选中模型，`HOST_TEST_TEXT` 给输入框预填上文。

- `KeyboardSmokeTests`（11 条）：聚焦输入框后键盘出现、键高不被压矮；打 nihao 点首选得到「你好」；输入中按逗号先上屏整句再插「，」；
  退格；切英文直接上屏字母；数字页、符号页；点音节进入单音节选词；在点的音节上选字后回到整句、再点这个音节按退格撤销选字；
  展开候选网格；打 ok 有 👌 🆗；收起键盘再点输入框；上文「宪法保障公民的合法」下
  quanli 的首选是带 AI 标记的「权利」（规则重排）；本地模型在键盘进程里加载并把「誓言」提到首位；深色模式；横屏；
  网址输入框从英文开始。
- **触屏的音节选字**（`KeySession::touch_syllable_editing`，iOS 打开、Windows 关闭）：在点的音节上选完字回到整句，
  不自动跳到别的音节，首选是按这次选字重新排过的整句（有 AI 时带 ✦）；选到句尾且整句都已选定才直接上屏。
  点着已选定的音节按退格，撤销那里的选字。在候选栏里选前缀词（「他的名字」）仍是逐段选词：接着选剩下的部分。
- `AppWalkthroughTests`（7 条）：宿主 App 逐屏截图（`build/screenshots/app/`）、下载暂停 / 继续 / 完成 / 删除、
  失败、大小不符、蜂窝网络提醒、空间不足，以及首页试打字框里键盘照常工作。

XCUITest 能直接看到键盘扩展里的键（`app.keys["q"]`、`app.buttons["你好"]`），靠的是键和候选的无障碍标签。
截图（浅色 / 深色，空闲 / 输入中，数字页、符号页、候选网格、音节选词、AI 标记、本地模型、横屏）在 `build/screenshots/`。

## 真机评测（`AIPinyinDeviceEval`）

单独的 scheme，平时的测试不跑它。测试包挂在宿主 App 里运行：能读 App Group 里下载好的模型，
词库取自（调试构建的）App 包里的键盘扩展，用例是仓库的 `data/eval/*.tsv`（构建时拷进测试包）。
用真引擎、真模型逐个字母输入，量首选准确率、最后一键之后等 AI 的时间、逐键耗时和进程内存。

```sh
# 手机连上、解锁；第一次要在 App 里下载一个模型，或者加 TEST_RUNNER_DEVICE_EVAL_DOWNLOAD=1 让评测自己下（每个 400–500MB）
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyinDeviceEval \
  -destination 'platform=iOS,id=<UDID>' -allowProvisioningUpdates test

# 模拟器上也能跑（Apple 端侧模型那两项会跳过），模型直接从 Mac 上拷进 App Group
TEST_RUNNER_DEVICE_EVAL_MODEL=/path/a.gguf:/path/b.gguf \
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyinDeviceEval \
  -destination 'platform=iOS Simulator,name=iPhone 18 Pro' \
  -derivedDataPath build/DerivedData CODE_SIGN_IDENTITY=- test
```

| 测试 | 内容 |
| --- | --- |
| `testA_RulesOnly` | 只有规则重排，各组用例的基线和逐键耗时 |
| `testB_LocalModels` | 每个已下载的目录模型各跑一遍（第一次见到每条用例，没有个人记忆）；记录加载时间和进程内存（几个模型在同一个进程里先后加载，第一个之后的内存增量不准，见下文） |
| `testC_AppleModel` | Apple 端侧模型经 `AppleModelChooser`（和键盘里一样带 120ms 防抖）。选词器一个答案也没交回时判失败，某一组用例每一条都等到了引擎的宿主选词时限（4 秒）时也判失败，见下文 |
| `testD_AppleMemoryPolicies` | 会话模拟：`chooser_memory` 取 blocked / contested / always / taught 时，个人记忆形成之后的准确率、换一批上文时的准确率、习惯词压力测试、教会的词测试、反向检验。每个引擎的 `AIController` 留到这种取值测完，每个引擎各数一份选词往来；和 `testC` 一样判失败。用来核对 Apple 那条路的默认策略（现在是 taught：用户主动改选过的词不让模型换掉，空格接受的习惯照样问模型；模拟和 2026-11-12 真机结果见 `crates/ime-ai/src/lib.rs` 的 `ChooserMemoryPolicy`）。`DEVICE_EVAL_POLICIES=taught,contested` 只量其中几种：Apple 模型每次要 0.4 秒多，四种一起跑约 26 分钟 |
| `testE_LocalModelSession` | 本地模型的会话模拟（个人记忆作先验）：同上五项，与 Mac 上 `ffi_session` 的 `--rounds`、`--skew 8`、`--teach 5`、`--teach 3`（反向检验）一一对应 |
| `testF_ChooserGuardCatchesASilentChooser`、`testG_TeachResultKeepsReversalRowsApart` | 评测本身的自检，不需要模型，模拟器上也跑：用假模型确认上面的护栏认得出一个答案也不交回的选词器（包括 `AIController` 提前释放、请求没人接），以及反向行的分组 |

`testC`、`testD` 的护栏：在 `AIController` 接好的两头（`engine.onChoiceRequest` 和 `AppleModelChooser.deliver`）
数选词请求、选词器交回的次数和其中真正给了答案的次数（写进报告的 notes）。一次运行（一个引擎配一个 `AIController`）
一个答案也没拿到就判失败：这时的数字只是规则和个人记忆的成绩。只看等待时间认不全：请求没人接时，
引擎在宿主选词时限（4 秒）一到就让 `aiPending` 转为假，每条只等 4 秒，碰不到评测自己的 6 秒时限；
模型一直出错时选词器当场交回空答案，根本不用等。另一项按引擎的 4 秒时限查：某一组结果每一条都等到了它，
说明那一组里选词器一次也没在时限内交回。

教会的词测试（`habit_neutral_cases.tsv`：44 组，每组同一个目标词、5 个中性上文）按每组第 1 次的结果分开报告。
第 1 次首选不对、用户点选了目标的组才算教过，报其中换上文第 1 次仍在首位的组数，和其余 4 次全在首位
（中途没再纠正，只教了一次）的组数；第 1 次本来就对的组用户只按了空格，没有教过，单列它们其余各次的结果；
所有组其余各次的合计（以前报的那个数）标为旧口径，只作对照。其余各次首选不对时评测照样点选目标，
会话和真实使用一样，所以旧口径里混着教了两三次之后的结果和没教过的组。

反向检验（`habit_reversal_cases.tsv`：同样 44 个拼音，每组 3 次）看的是反方向：记忆太强也不行。
前 2 次和教会的词一样，在两个中性上文里教一个词、换上文再看一次；第 3 次换成同音的另一个词，
上文取这个词在 `alternating_cases.tsv` 里第一次出现的那一句，明确指向它。目标和第 1 次不同的这一条是反向行，
不算进任何「仍在首位」的数，按所在组教过没教过分开报它是不是首选（与 `ffi_session --teach 3` 那一行里的「反向行」相同）。
教会的词只量一面，记忆越强分数越高；教过的组里教的词保持得住、上文明确要另一个词时又选得回来，分工才算对。
两个用例文件都由 `tools/eval/gen_habit_cases.py` 生成。

结果打印在测试日志里，也写进 App 容器的 `Library/Caches/DeviceEval/*.json`（同时作为测试附件保留），真机上用
`xcrun devicectl device copy from --device <UDID> --domain-type appDataContainer --domain-identifier com.zengji.aipinyin --source Library/Caches/DeviceEval --destination <目录>` 取回。
可选变量（加 `TEST_RUNNER_` 前缀）：`DEVICE_EVAL_DOWNLOAD=1`、`DEVICE_EVAL_MODEL=<路径[:路径…]>`、`DEVICE_EVAL_ROUNDS=<n>`（默认 3）、
`DEVICE_EVAL_SESSION_MODEL=<ModelCatalog 的 id>`（`testE` 用哪个模型，默认 Qwen2.5）。

评测进程（宿主 App）里的内存读数要看清是怎么量的。`testB` 在同一个进程里按 `ModelCatalog` 的顺序先后加载
两个模型（先 Qwen2.5、后 Qwen3），第二个模型的「加载前」紧接在第一个引擎卸载模型之后读，它的增量是失真的：
2026-11-11 的 `testB` 里 Qwen2.5 +47 MiB、Qwen3 只 +49 MiB，这两个增量不能互相比较。各自在单独的进程里量就看得出差别：
`testE` 用 `-only-testing` 单独跑、读数时同时留着 3 个装着模型的引擎（反向检验的第 4 个引擎在读数之后才建），
Qwen2.5 177.3 MiB、Qwen3 249.3 MiB，每个实例相差约 24 MiB，与键盘里（加载后 59 / 80 MB，打过分之后 67 / 92 MB）
和 Mac 上普通进程里（加载后 55.3 / 80.4 MiB）的差距一致，也和两者 KV 缓存的差相当（上下文 256 个 token，每个 token 12 KiB 对 112 KiB）。
键盘扩展是另一个进程，有自己的 177 MB 上限，它的实际占用用 UI 测试 `KeyboardMemoryTests` 量，
它从调试构建的键盘工具栏状态行读「已用/上限MB」。前提：模型已经在 App Group 里（跑过一次上面的评测即可），
手机上已添加「AI 拼音」键盘，并在任意输入框里用地球键切换到过它：

```sh
TEST_RUNNER_KEYBOARD_MEMORY_MODEL=qwen3-0.6b-q4km \
xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyin -destination 'platform=iOS,id=<UDID>' \
  -allowProvisioningUpdates test "-only-testing:AIPinyinUITests/KeyboardMemoryTests"
```

2026-11-11 在 iPhone 16 Pro Max（iOS 27.2）上的结果，键盘上限 177 MB：

| 模型 | 加载后 | 打过分之后 | 第一次见到的中文用例（389 条） | 习惯词测试（63） | 教会的词：换个上文仍在首位 | 反向检验：另一个词仍能选对 |
| --- | --- | --- | --- | --- | --- | --- |
| Qwen2.5-0.5B（推荐） | 55 MB | 67 MB | 373 | 58 | 29/37 | 33/37 |
| Qwen3-0.6B | 80 MB | 93 MB | 377 | 54 | 25/34 | 30/34 |

表里都是 2026-11-12 用最终版本在手机上重测的数（11-11 的内存读数是 59→67、80→92 MB）。后两列只算第 1 次确实经过改选的组，
这两列和会话、习惯词的结果都与 Mac 逐项相同。
UI 测试报「Timed out while enabling automation mode」时，在手机的 设置 → 开发者 里打开「启用 UI 自动化」，解锁后再跑。

## 上架前

- **App 图标**：`App/Assets.xcassets/AppIcon.appiconset/AppIcon.png` 是脚本生成的占位图（蓝绿底、白圆），
  只为让归档能通过校验，**正式提交前换成真正的图标**（1024×1024，不带透明通道）。
- **隐私清单**：`App/PrivacyInfo.xcprivacy`、`Keyboard/PrivacyInfo.xcprivacy`。不追踪、不收集数据。
  App 声明了 UserDefaults（读 `AppleKeyboards`，CA92.1）和磁盘空间（下载前检查并告诉用户，E174.1、85F4.1）；
  键盘声明了文件时间戳（Rust 引擎对词库、个人词库、模型用 stat / fstat，按修改时间判断个人词库是否被改过，C617.1）。
  依据是链接后可执行文件引用的符号（`nm -u`）；换了 Rust、llama 或系统 API 的用法要重新检查。
- 真机归档、签名和上传都还没有做过。

## 已知情况

- **键盘高度**：iOS 27 不理会挂在键盘根视图自己身上的高度约束（2026-11-12 试过 9 种做法，高度都停在系统默认的
  216pt，真机 224pt，比系统键盘矮约 45pt），但会按子视图的约束算出键盘该多高（仓输入法也是让内容撑出高度）。
  所以根视图里放了一个隐形视图，高度约束挂在它身上（`KeyboardRootView.updateHeight`）：顶部 50pt（横屏 40pt），
  加四行键，行高和系统键盘一样，竖屏 54pt（宽 414pt 以上的大屏手机 56pt），横屏 40pt。竖屏总高 266pt（大屏 274pt），
  比系统键盘高 8pt 左右，多出来的是拼音条。`KeyboardSmokeTests` 的 testTypingFlow、testLandscape 会检查键高，
  iPhone 18 Pro 和 18 Pro Max 模拟器上量过。
- **Apple 端侧模型**：这台 Mac 的模拟器报告不可用（deviceNotEligible），模拟器里走不到真实调用；
  工具栏会显示「Apple 端侧模型不可用，按词频选词」。调度逻辑（取消、超时、无效答案、引擎作废）都用假模型测过，
  宿主选词的整条链路用真引擎测过。
- **个人词库是整份文件覆盖**：两个键盘进程不会合并各自学到的词，靠的是键盘出现时重新读入。
  已经在显示中的键盘若在别的进程写盘之后才上屏，那边的新词仍会被盖掉；输入进行到一半时重新读入会丢掉这段输入里
  还没写盘的学习。
- **引擎在少数情况下给不出候选**：在已锁定的多音节片段里再次改选之后，偶尔没有任何候选（Rust 侧的已知问题）。
  这时空格、回车、标点都照常上屏「已选定的词 + 剩余字母」，不会卡住。
- **本地模型卸载后内存不回到加载前**：Mac 上实测卸载后 phys_footprint 仍比基线高十几到四十多 MiB（见 `ime_ffi.h`），
  真机上换模型时 110MB 门槛可能因此不过，工具栏会如实显示「内存不足」，下次键盘出现再试。
- **按键音和触感**要「允许完全访问」；没有完全访问时其余功能都正常。
- **宿主 App** 暂时允许横屏，只是为了测键盘的横屏布局；正式 App 可以改回只竖屏
  （那样横屏测试就得换别的宿主）。
- Safari 的地址栏是搜索框不是网址框，键盘在那里保持中文，这是对的。
