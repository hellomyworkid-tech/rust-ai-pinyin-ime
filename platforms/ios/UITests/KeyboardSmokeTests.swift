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

        // 数字页
        tap("数字")
        XCTAssertTrue(key("1").waitForExistence(timeout: 2))
        screenshot("light-numbers")
        tap("1")
        tap("。")
        waitForText("你好你ok1。")
        tap("符号")
        screenshot("light-symbols")
        tap("字母")
        XCTAssertTrue(key("q").waitForExistence(timeout: 2))
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
        tap("数字")
        screenshot("dark-numbers")
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

    func testURLFieldStartsInEnglish() {
        let url = app.textFields["urlTextField"]
        XCTAssertTrue(url.waitForExistence(timeout: 5))
        url.tap()
        XCTAssertTrue(key("英文，点按切换到中文").waitForExistence(timeout: 10))
        type("abc")
        XCTAssertEqual(url.value as? String, "abc")
    }
}
