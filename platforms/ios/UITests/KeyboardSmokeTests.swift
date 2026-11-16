import XCTest

/// 冒烟测试：在宿主 App 的输入框里用 AI 拼音键盘打字。
/// 前提：模拟器里已经启用 AI 拼音键盘并设为当前键盘（见 README 里的 enable-keyboard.sh）。
/// 截图写到环境变量 SCREENSHOT_DIR 指定的目录（xcodebuild 用 TEST_RUNNER_SCREENSHOT_DIR 传入）。
final class KeyboardSmokeTests: XCTestCase {
    private var app: XCUIApplication!

    override func setUp() {
        super.setUp()
        continueAfterFailure = false
        XCUIDevice.shared.appearance = .light
        XCUIDevice.shared.orientation = .portrait
        app = XCUIApplication()
        // 宿主界面里有试键盘用的各种输入框，只在这个启动参数下出现
        app.launchArguments = ["-hostTest"]
        app.launch()
    }

    override func tearDown() {
        XCUIDevice.shared.appearance = .light
        XCUIDevice.shared.orientation = .portrait
        super.tearDown()
    }

    // MARK: - 工具

    private var field: XCUIElement { app.textViews["hostTextField"].exists ? app.textViews["hostTextField"] : app.textFields["hostTextField"] }

    private func key(_ label: String) -> XCUIElement {
        app.keys[label].firstMatch
    }

    private func tap(_ label: String, file: StaticString = #filePath, line: UInt = #line) {
        let element = key(label)
        XCTAssertTrue(element.waitForExistence(timeout: 5), "找不到键 \(label)", file: file, line: line)
        element.tap()
    }

    private func swipeUpKey(_ label: String, file: StaticString = #filePath, line: UInt = #line) {
        let element = key(label)
        XCTAssertTrue(element.waitForExistence(timeout: 5), "找不到键 \(label)", file: file, line: line)
        let start = element.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        let end = element.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: -0.3))
        start.press(forDuration: 0.02, thenDragTo: end)
    }

    private func type(_ letters: String) {
        for c in letters { tap(String(c)) }
    }

    private var fieldText: String {
        (field.value as? String) ?? ""
    }

    private func waitForText(_ expected: String, file: StaticString = #filePath, line: UInt = #line) {
        let predicate = NSPredicate { [weak self] _, _ in self?.fieldText == expected }
        let done = expectation(for: predicate, evaluatedWith: nil)
        if XCTWaiter.wait(for: [done], timeout: 3) != .completed {
            XCTFail("输入框内容是「\(fieldText)」，应为「\(expected)」", file: file, line: line)
        }
    }

    private func screenshot(_ name: String) {
        let shot = XCUIScreen.main.screenshot()
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let dir = ProcessInfo.processInfo.environment["SCREENSHOT_DIR"], !dir.isEmpty {
            let url = URL(fileURLWithPath: dir).appendingPathComponent("\(name).png")
            try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
            try? shot.pngRepresentation.write(to: url)
        }
    }

    /// 键要和系统键盘的差不多高（系统键盘竖屏 42、横屏 32）。键盘的高度约束没生效时，iOS 27 会把键盘压到
    /// 系统默认的 216 / 160，键高只剩约 33 / 24
    private func assertKeysAreFullHeight(minimum: CGFloat, file: StaticString = #filePath, line: UInt = #line) {
        let height = key("q").frame.height
        XCTAssertGreaterThanOrEqual(height, minimum, "键高 \(height)：键盘高度没生效", file: file, line: line)
    }

    /// 聚焦输入框，等 AI 拼音键盘出现
    private func focusAndWaitForKeyboard() {
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        let ourKey = key("q")
        if !ourKey.waitForExistence(timeout: 10) {
            print(app.debugDescription)
            XCTFail("AI 拼音键盘没有出现。先运行 scripts/enable-keyboard.sh")
        }
        // 等 viewDidAppear 之后的引擎创建
        Thread.sleep(forTimeInterval: 0.5)
    }

    // MARK: - 测试

    func testTypingFlow() {
        focusAndWaitForKeyboard()
        assertKeysAreFullHeight(minimum: 40)
        screenshot("light-idle")

        // nihao → 选第一个候选
        type("nihao")
        let first = app.buttons["你好"].firstMatch
        XCTAssertTrue(first.waitForExistence(timeout: 3), "候选栏里没有「你好」")
        screenshot("light-composing")
        first.tap()
        waitForText("你好")

        // 输入中途按逗号：先上屏整句，再插中文逗号
        type("ni")
        tap("，")
        waitForText("你好你，")

        // 退格
        tap("删除")
        waitForText("你好你")

        // 切到英文：字母直接上屏
        tap("中文，点按切换到英文")
        XCTAssertTrue(key("英文，点按切换到中文").waitForExistence(timeout: 2))
        type("ok")
        waitForText("你好你ok")
        tap("英文，点按切换到中文")

        // 数字页 → 分类符号面板（测试分类切换、锁定连输、退格与返回）
        tap("数字")
        XCTAssertTrue(key("1").waitForExistence(timeout: 2))
        screenshot("light-numbers")
        tap("1")
        tap("。")
        waitForText("你好你ok1。")
        tap("符号")
        let backFromSymbols = app.buttons["返回"].firstMatch
        let lockSymbols = app.buttons["锁定符号面板"].firstMatch
        let mathTab = app.buttons["数学符号"].firstMatch
        XCTAssertTrue(backFromSymbols.waitForExistence(timeout: 2), "应打开分类符号面板")
        XCTAssertTrue(lockSymbols.exists)
        XCTAssertTrue(mathTab.exists)
        screenshot("light-symbols")

        // 切到「网络」分类查看跨列长符号排布
        app.buttons["网络符号"].firstMatch.tap()
        XCTAssertTrue(key("https://").waitForExistence(timeout: 2))
        screenshot("light-symbols-network")

        // 直接按在分类按钮（「数学」）上往左拖动：验证手指落在按钮上也能顺畅横滑出后续分类（「希腊」）
        let startDrag = mathTab.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        let endDrag = backFromSymbols.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        startDrag.press(forDuration: 0.02, thenDragTo: endDrag)
        let greekTab = app.buttons["希腊符号"].firstMatch
        XCTAssertTrue(greekTab.waitForExistence(timeout: 2) && greekTab.isHittable, "按在分类按钮上横滑应顺畅滚出后续分类")
        greekTab.tap()
        XCTAssertTrue(key("α").waitForExistence(timeout: 2))

        // 再直接按在分类按钮上往右拖回开头，验证双向拖动均顺畅
        for _ in 0..<2 {
            let dragRightStart = backFromSymbols.coordinate(withNormalizedOffset: CGVector(dx: 1.4, dy: 0.5))
            let dragRightEnd = lockSymbols.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
            dragRightStart.press(forDuration: 0.02, thenDragTo: dragRightEnd)
        }
        Thread.sleep(forTimeInterval: 0.35)

        // 锁定面板：连续输入两个数学符号再退格删掉第二个，面板保持不关闭
        lockSymbols.tap()
        XCTAssertEqual(lockSymbols.value as? String, "已锁定")
        mathTab.tap()
        tap("≠")
        tap("∞")
        waitForText("你好你ok1。≠∞")
        app.buttons["删除"].firstMatch.tap()
        waitForText("你好你ok1。≠")
        XCTAssertTrue(backFromSymbols.exists, "锁定状态下点选符号不应退出面板")

        // 解锁后点选一个英文半角符号：中文模式下也原样上屏（不转全角），并自动返回字母主键盘
        lockSymbols.tap()
        XCTAssertEqual(lockSymbols.value as? String, "未锁定")
        app.buttons["常用符号"].firstMatch.tap()
        tap("_")
        waitForText("你好你ok1。≠_")
        XCTAssertTrue(key("q").waitForExistence(timeout: 2), "未锁定时点选符号后应自动回到主键盘")

        // 字母页上划输入数字与标点：q→1、x→.、t→5、b→？、，→。
        swipeUpKey("q")
        swipeUpKey("x")
        swipeUpKey("t")
        swipeUpKey("b")
        swipeUpKey("，")
        waitForText("你好你ok1。≠_1.5？。")
    }

    private func syllableChip(_ label: String) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label == %@", label)).firstMatch
    }

    /// 点音节选字之后回到整句（不自动选中别的音节）；再点这个已选定的音节按退格，撤销那里的选字
    func testTapSyllablePickAndUndo() {
        focusAndWaitForKeyboard()
        type("tademingzijiaoyangjiaye")
        let jiao = syllableChip("音节 jiao")
        XCTAssertTrue(jiao.waitForExistence(timeout: 3))
        jiao.tap()
        XCTAssertTrue(jiao.isSelected)
        let jiaoChar = app.buttons["叫"].firstMatch
        XCTAssertTrue(jiaoChar.waitForExistence(timeout: 3))
        jiaoChar.tap()

        let pinned = syllableChip("音节 jiao，已选定")
        XCTAssertTrue(pinned.waitForExistence(timeout: 3))
        XCTAssertFalse(pinned.isSelected, "选完字回到整句")
        XCTAssertFalse(syllableChip("音节 ta").isSelected, "不跳到句首的音节")
        let sentence = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label BEGINSWITH %@", "整句预览：")).firstMatch
        XCTAssertTrue(sentence.label.contains("叫"), sentence.label)
        screenshot("light-syllable-picked")

        pinned.tap()
        XCTAssertTrue(pinned.isSelected)
        tap("删除")
        let undone = syllableChip("音节 jiao")
        XCTAssertTrue(undone.waitForExistence(timeout: 3), "退格撤销了 jiao 上的选字")
        XCTAssertTrue(undone.isSelected, "光标留在 jiao 上")
        screenshot("light-syllable-undone")
    }

    /// 点选输错的音节后直接按键输入，删除原音节并在原处改写为新拼音；改写出的单音节或多音节及右侧对应文字均保持蓝色高亮
    func testTapSyllableAndRetypeReplacesMistypedPinyin() {
        focusAndWaitForKeyboard()
        type("zhongguoneiyoueuv")
        let nei = syllableChip("音节 nei")
        XCTAssertTrue(nei.waitForExistence(timeout: 3))
        nei.tap()
        XCTAssertTrue(nei.isSelected)

        type("mei")
        let mei = syllableChip("音节 mei")
        XCTAssertTrue(mei.waitForExistence(timeout: 3), "nei 应被改写为 mei")
        XCTAssertFalse(nei.exists, "旧音节 nei 应已被删除")
        XCTAssertTrue(mei.isSelected, "改写出的 mei 应保持蓝色高亮")
        let sentence = app.descendants(matching: .any)
            .matching(NSPredicate(format: "label BEGINSWITH %@", "整句预览：")).firstMatch
        XCTAssertTrue(sentence.label.contains("中国没有"), sentence.label)

        tap("空格")
        XCTAssertTrue(fieldText.hasPrefix("中国没有"), "上屏内容应以『中国没有』开头，实际：\(fieldText)")

        // 单音节改写为双音节：nihaowozaizheli 点选 wo 改写成 tamen，ta 与 men 均蓝色高亮，整句预览识别为「他们」
        type("nihaowozaizheli")
        let wo = syllableChip("音节 wo")
        XCTAssertTrue(wo.waitForExistence(timeout: 3))
        wo.tap()
        XCTAssertTrue(wo.isSelected)
        screenshot("light-syllable-tapped-wo")

        type("tamen")
        let ta = syllableChip("音节 ta")
        let men = syllableChip("音节 men")
        XCTAssertTrue(ta.waitForExistence(timeout: 3), "wo 应被改写为 ta men")
        XCTAssertTrue(men.exists)
        XCTAssertFalse(wo.exists)
        XCTAssertTrue(ta.isSelected, "ta 应保持蓝色高亮")
        XCTAssertTrue(men.isSelected, "men 应保持蓝色高亮")
        XCTAssertTrue(sentence.label.contains("你好他们在这里"), sentence.label)
        screenshot("light-syllable-overwritten")

        tap("空格")
        XCTAssertTrue(fieldText.hasSuffix("你好他们在这里"), "实际：\(fieldText)")

        // 点选音节后按退格：从被选音节 men 自身末尾字母删起（不误删 ta 的 a），继续补打仍保持蓝色高亮
        type("nihaotamenzaizheli")
        let men2 = syllableChip("音节 men")
        XCTAssertTrue(men2.waitForExistence(timeout: 3))
        men2.tap()
        XCTAssertTrue(men2.isSelected)
        tap("删除")
        tap("删除")
        let taRemain = syllableChip("音节 ta")
        let mRemain = syllableChip("音节 m")
        XCTAssertTrue(taRemain.waitForExistence(timeout: 3), "前一个音节 ta 不应被误删")
        XCTAssertTrue(mRemain.exists, "men 连按两次退格后应剩 m")
        XCTAssertFalse(taRemain.isSelected, "ta 不应被误标蓝")
        XCTAssertTrue(mRemain.isSelected, "剩余的 m 应保持蓝色高亮")
        type("ei")
        let mei2 = syllableChip("音节 mei")
        XCTAssertTrue(mei2.waitForExistence(timeout: 3))
        XCTAssertTrue(mei2.isSelected)
        tap("空格")
        XCTAssertTrue(fieldText.hasSuffix("你好他没在这里"), "实际：\(fieldText)")
    }

    func testSyllableStripAndGrid() {
        focusAndWaitForKeyboard()
        type("jintiantianqihenhao")
        XCTAssertTrue(app.buttons["今天天气很好"].firstMatch.waitForExistence(timeout: 3))
        // 点第 3 个音节（tian）进入单音节选词
        let syllable = app.buttons.matching(NSPredicate(format: "label == %@", "音节 tian")).element(boundBy: 1)
        XCTAssertTrue(syllable.waitForExistence(timeout: 2))
        syllable.tap()
        screenshot("light-syllable-cursor")
        syllable.tap()
        // 展开候选网格
        app.buttons["展开候选"].tap()
        XCTAssertTrue(app.buttons["收起候选"].waitForExistence(timeout: 2))
        screenshot("light-grid")
        app.buttons["收起候选"].tap()
        tap("空格")
        waitForText("今天天气很好")

        // 易读错词召回与正音提示：打 hundun，候选栏与展开网格均展示「馄饨 馄 (hun) 饨 (tun)」，点选仅上屏「馄饨」
        type("hundun")
        XCTAssertTrue(app.buttons["混沌"].firstMatch.waitForExistence(timeout: 3))
        let wonton = app.buttons["馄饨，读音 馄 (hun) 饨 (tun)"].firstMatch
        XCTAssertTrue(wonton.waitForExistence(timeout: 3), "hundun 应召回带正音提示的「馄饨」")
        screenshot("light-hundun-bar")

        app.buttons["展开候选"].tap()
        XCTAssertTrue(app.buttons["收起候选"].waitForExistence(timeout: 2))
        let gridWonton = app.cells.matching(NSPredicate(format: "label == %@", "馄饨，读音 馄 (hun) 饨 (tun)")).firstMatch
        XCTAssertTrue(gridWonton.waitForExistence(timeout: 2), "展开网格中也应完整显示带正音提示的「馄饨」")
        screenshot("light-hundun-grid")
        gridWonton.tap()
        waitForText("今天天气很好馄饨")

        // 高频现代词组与短剧搭配首屏核验
        type("shouzhongrenqun")
        XCTAssertTrue(app.buttons["受众人群"].firstMatch.waitForExistence(timeout: 3))
        screenshot("light-shouzhongrenqun")
        tap("空格")
        waitForText("今天天气很好馄饨受众人群")

        type("renshe")
        XCTAssertTrue(app.buttons["人设"].firstMatch.waitForExistence(timeout: 3))
        screenshot("light-renshe")
        tap("空格")
        waitForText("今天天气很好馄饨受众人群人设")

        type("fuhenvpinduanju")
        XCTAssertTrue(app.buttons["符合女频短剧"].firstMatch.waitForExistence(timeout: 3))
        screenshot("light-fuhenvpinduanju")
        tap("空格")
        waitForText("今天天气很好馄饨受众人群人设符合女频短剧")
    }

    /// 上文决定同音词：输入框里已有「宪法保障公民的合法」，打 quanli，首选应是带 AI 标记的「权利」
    /// （不带上文时首选是「权力」或「全力」）。这一条走的是引擎的规则重排，不需要模型。
    func testContextDecidesHomophone() {
        app.terminate()
        app.launchEnvironment["HOST_TEST_TEXT"] = "宪法保障公民的合法"
        app.launch()
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        // 点在输入框靠右的空白处，光标落在已有文字的末尾
        field.coordinate(withNormalizedOffset: CGVector(dx: 0.92, dy: 0.5)).tap()
        XCTAssertTrue(key("q").waitForExistence(timeout: 10), "AI 拼音键盘没有出现。先运行 scripts/enable-keyboard.sh")
        Thread.sleep(forTimeInterval: 0.5)

        type("quanli")
        let boosted = app.buttons["权利，AI 推荐"].firstMatch
        XCTAssertTrue(boosted.waitForExistence(timeout: 3), "首选应是带 AI 标记的「权利」")
        screenshot("light-ai-boosted")
        boosted.tap()
        waitForText("宪法保障公民的合法权利")
    }

    /// 本地模型端到端：模型由 App 放进 App Group，键盘在首帧之后加载它；上文是「我对天发誓这是我的」时打 shiyan，
    /// 规则给出的首选是「实验」，模型打完分后「誓言」被提到首位。
    /// 需要一个 GGUF 文件：xcodebuild 用 TEST_RUNNER_HOST_TEST_MODEL=<路径>（可选 TEST_RUNNER_HOST_TEST_MODEL_ID）传入，没有就跳过。
    func testLocalModelPicksByContext() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let modelPath = environment["HOST_TEST_MODEL"], !modelPath.isEmpty else {
            throw XCTSkip("没有设置 HOST_TEST_MODEL")
        }
        app.terminate()
        app.launchEnvironment["HOST_TEST_TEXT"] = "我对天发誓这是我的"
        app.launchEnvironment["HOST_TEST_MODEL"] = modelPath
        app.launchEnvironment["HOST_TEST_MODEL_ID"] = environment["HOST_TEST_MODEL_ID"] ?? "qwen2.5-0.5b-q4km"
        app.launch()
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.coordinate(withNormalizedOffset: CGVector(dx: 0.92, dy: 0.5)).tap()
        XCTAssertTrue(key("q").waitForExistence(timeout: 10), "AI 拼音键盘没有出现。先运行 scripts/enable-keyboard.sh")

        // 工具栏显示加载状态
        let loaded = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "AI 选词 · 本地模型")).firstMatch
        XCTAssertTrue(loaded.waitForExistence(timeout: 20), "本地模型没有加载")
        screenshot("light-local-model-idle")

        type("shiyan")
        let boosted = app.buttons["誓言，AI 推荐"].firstMatch
        XCTAssertTrue(boosted.waitForExistence(timeout: 5), "模型没有把「誓言」提到首位")
        screenshot("light-local-model-boosted")
        boosted.tap()
        waitForText("我对天发誓这是我的誓言")
    }

    func testDarkMode() {
        XCUIDevice.shared.appearance = .dark
        focusAndWaitForKeyboard()
        screenshot("dark-idle")
        type("nihao")
        XCTAssertTrue(app.buttons["你好"].firstMatch.waitForExistence(timeout: 3))
        screenshot("dark-composing")
        app.buttons["你好"].firstMatch.tap()
        tap("数字")
        screenshot("dark-numbers")
        tap("符号")
        XCTAssertTrue(app.buttons["中文符号"].firstMatch.waitForExistence(timeout: 2))
        app.buttons["中文符号"].firstMatch.tap()
        screenshot("dark-symbols")
    }

    /// 英文词后面也有表情：打 ok，「ok」后面是 👌 🆗（和系统键盘一样），点表情上屏的是表情
    func testEnglishWordGetsEmoji() {
        focusAndWaitForKeyboard()
        type("ok")
        let hand = app.buttons["👌"].firstMatch
        XCTAssertTrue(hand.waitForExistence(timeout: 3), "ok 后面应当有 👌")
        XCTAssertTrue(app.buttons["🆗"].firstMatch.exists)
        screenshot("light-english-emoji")
        hand.tap()
        waitForText("👌")
    }

    /// 点「收起键盘」再点输入框，键盘照样能打字。2026-11-12 这里崩过：输入框正在消失时
    /// textDocumentProxy.documentIdentifier 是 nil，按非可选的 UUID 读就崩溃
    func testDismissAndRefocus() {
        focusAndWaitForKeyboard()
        let dismiss = app.buttons["收起键盘"].firstMatch
        XCTAssertTrue(dismiss.waitForExistence(timeout: 3))
        dismiss.tap()
        Thread.sleep(forTimeInterval: 1.0)
        field.tap()
        XCTAssertTrue(key("q").waitForExistence(timeout: 10), "键盘没有再出现")
        type("nihao")
        XCTAssertTrue(app.buttons["你好"].firstMatch.waitForExistence(timeout: 3))
        app.buttons["你好"].firstMatch.tap()
        waitForText("你好")
    }

    func testLandscape() {
        XCUIDevice.shared.orientation = .landscapeLeft
        focusAndWaitForKeyboard()
        assertKeysAreFullHeight(minimum: 30)
        screenshot("landscape-idle")
        type("nihao")
        XCTAssertTrue(app.buttons["你好"].firstMatch.waitForExistence(timeout: 3))
        screenshot("landscape-composing")
        app.buttons["你好"].firstMatch.tap()
        waitForText("你好")
    }

    /// 输完拼音还没选完字时切输入框、收起键盘或切应用，回到原输入框时保留未完成的拼音和选字进度
    func testUnfinishedCompositionSurvivesSwitchAndDismiss() {
        focusAndWaitForKeyboard()
        type("nihaotamenzaizheli")
        let hao = syllableChip("音节 hao")
        XCTAssertTrue(hao.waitForExistence(timeout: 3))
        hao.tap()
        app.buttons["好"].firstMatch.tap()
        let pinnedHao = syllableChip("音节 hao，已选定")
        XCTAssertTrue(pinnedHao.waitForExistence(timeout: 3))

        // 1. 切到网址输入框：网址框干净且处于英文模式，不带入刚才未完成的拼音
        let url = app.textFields["urlTextField"]
        XCTAssertTrue(url.waitForExistence(timeout: 3))
        url.tap()
        XCTAssertTrue(key("英文，点按切换到中文").waitForExistence(timeout: 5))
        XCTAssertFalse(pinnedHao.exists)

        // 2. 切回原输入框：未完成的拼音与已锁定的「好」完整恢复
        field.tap()
        XCTAssertTrue(pinnedHao.waitForExistence(timeout: 5), "切回原输入框后应恢复未完成的组合与已选定的字")

        // 3. 切到桌面（模拟切换程序 / 截图跳出）再切回宿主 App，同样保留并可继续选字上屏
        XCUIDevice.shared.press(.home)
        Thread.sleep(forTimeInterval: 0.8)
        app.activate()
        if !key("q").waitForExistence(timeout: 3) {
            field.tap()
            XCTAssertTrue(key("q").waitForExistence(timeout: 10))
        }
        XCTAssertTrue(pinnedHao.waitForExistence(timeout: 5), "切应用回来后应保留未完成的组合")
        tap("空格")
        waitForText("你好他们在这里")
    }

    func testURLFieldStartsInEnglish() {
        let url = app.textFields["urlTextField"]
        XCTAssertTrue(url.waitForExistence(timeout: 5))
        url.tap()
        XCTAssertTrue(key("英文，点按切换到中文").waitForExistence(timeout: 10))
        type("abc")
        XCTAssertEqual(url.value as? String, "abc")
    }

    /// 工具栏左上角快捷设置：输入时自动隐藏防误触；空闲点击展开二级面板，开关即时生效，左 `<` / 右 `⊗` 均可收起
    func testQuickSettingsPanelTogglesAndCloses() {
        focusAndWaitForKeyboard()
        let gear = app.buttons["快捷设置"].firstMatch
        XCTAssertTrue(gear.waitForExistence(timeout: 3), "空闲工具栏左上角应有快捷设置入口")

        // 输入中隐藏工具栏与设置入口，避免连打时误触
        type("n")
        XCTAssertFalse(gear.exists, "输入拼音期间不应显示快捷设置按钮")
        tap("删除")
        XCTAssertTrue(gear.waitForExistence(timeout: 2))

        // 展开二级面板
        gear.tap()
        let back = app.buttons["返回键盘"].firstMatch
        let close = app.buttons["关闭设置"].firstMatch
        let openApp = app.buttons["打开 AI 拼音设置"].firstMatch
        let emojiToggle = app.buttons["候选表情"].firstMatch
        XCTAssertTrue(back.waitForExistence(timeout: 2))
        XCTAssertTrue(close.exists)
        XCTAssertTrue(openApp.exists)
        XCTAssertTrue(emojiToggle.exists)
        XCTAssertFalse(key("q").exists, "快捷设置面板展开时应遮住按键区")
        screenshot("light-quick-settings")

        // 关闭「候选表情」并点右上角「关闭设置」回到键盘，验证立即生效
        emojiToggle.tap()
        XCTAssertEqual(emojiToggle.value as? String, "已关闭")
        close.tap()
        XCTAssertTrue(key("q").waitForExistence(timeout: 2))

        type("ok")
        XCTAssertTrue(app.buttons["ok"].firstMatch.waitForExistence(timeout: 3))
        XCTAssertFalse(app.buttons["👌"].firstMatch.exists, "关闭候选表情后不应再出现表情候选")
        tap("删除")
        tap("删除")

        // 重新打开面板（顺便切深色外观截一张图），开启「候选表情」并点左上角「返回键盘」收起
        XCUIDevice.shared.appearance = .dark
        XCTAssertTrue(gear.waitForExistence(timeout: 2))
        gear.tap()
        XCTAssertTrue(back.waitForExistence(timeout: 2))
        emojiToggle.tap()
        XCTAssertEqual(emojiToggle.value as? String, "已开启")
        screenshot("dark-quick-settings")
        back.tap()
        XCTAssertTrue(key("q").waitForExistence(timeout: 2))

        type("ok")
        XCTAssertTrue(app.buttons["👌"].firstMatch.waitForExistence(timeout: 3), "重新开启候选表情后应立即恢复")
    }
}
