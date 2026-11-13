import XCTest

/// 宿主 App 自己的界面：逐屏走一遍并截图，下载走调试用的假通道（启动参数 -simulateDownloads，不联网）。
/// 截图写到 SCREENSHOT_DIR（xcodebuild 用 TEST_RUNNER_SCREENSHOT_DIR 传入）下的 app/ 目录。
final class AppWalkthroughTests: XCTestCase {
    private var app: XCUIApplication!

    override func setUp() {
        super.setUp()
        continueAfterFailure = false
        XCUIDevice.shared.appearance = .light
        XCUIDevice.shared.orientation = .portrait
    }

    private func launch(_ environment: [String: String] = [:]) {
        app = XCUIApplication()
        app.launchArguments = ["-simulateDownloads"]
        app.launchEnvironment = environment
        app.launch()
    }

    private func shot(_ name: String) {
        let shot = XCUIScreen.main.screenshot()
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let dir = ProcessInfo.processInfo.environment["SCREENSHOT_DIR"], !dir.isEmpty {
            let folder = URL(fileURLWithPath: dir).appendingPathComponent("app")
            try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            try? shot.pngRepresentation.write(to: folder.appendingPathComponent("\(name).png"))
        }
    }

    private func tab(_ name: String) {
        let button = app.tabBars.buttons[name]
        XCTAssertTrue(button.waitForExistence(timeout: 5), "找不到标签 \(name)")
        button.tap()
    }

    private func element(_ id: String) -> XCUIElement {
        app.descendants(matching: .any)[id].firstMatch
    }

    private func button(_ id: String) -> XCUIElement { app.buttons[id].firstMatch }

    private func scrollTo(_ element: XCUIElement) {
        var tries = 0
        while !element.isHittable && tries < 8 {
            app.swipeUp()
            tries += 1
        }
    }

    /// 点开关右端的滑块（点中心是点在文字上，不一定翻转）
    private func flip(_ toggle: XCUIElement) {
        toggle.coordinate(withNormalizedOffset: CGVector(dx: 0.92, dy: 0.5)).tap()
    }

    private func tapMode(_ mode: String) {
        let row = button("mode-\(mode)")
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.tap()
    }

    /// 把两个模型清回「未下载」，让每条测试从同样的状态开始
    private func cleanModels() {
        for id in ["qwen2.5-0.5b-q4km", "qwen3-0.6b-q4km"] {
            for action in ["delete", "cancel"] {
                let b = button("\(action)-\(id)")
                if b.exists, b.isHittable {
                    b.tap()
                    if action == "delete" {
                        let confirm = app.buttons["删除"].firstMatch
                        if confirm.waitForExistence(timeout: 2) { confirm.tap() }
                    }
                }
            }
        }
    }

    // MARK: - 测试

    func testWalkThroughEveryScreen() {
        launch()
        // 首页
        XCTAssertTrue(element("keyboardStatus").waitForExistence(timeout: 5))
        shot("01-home")
        app.swipeUp()
        shot("01-home-scrolled")

        // AI 选词：三种方式
        tab("AI 选词")
        tapMode("off")
        shot("02-ai-off")
        tapMode("apple")
        XCTAssertTrue(element("appleStatus").waitForExistence(timeout: 5))
        shot("03-ai-apple")
        tapMode("local")
        XCTAssertTrue(button("mode-local").isSelected)
        shot("04-ai-local")

        // 输入设置
        tab("输入设置")
        let zh = app.switches["toggle-z / zh"].firstMatch
        XCTAssertTrue(zh.waitForExistence(timeout: 5))
        flip(zh)
        flip(app.switches["toggle-in / ing"].firstMatch)
        shot("05-input-settings")
        app.swipeUp()
        let haptics = app.switches["toggle-haptics"].firstMatch
        if haptics.waitForExistence(timeout: 2) { flip(haptics) }
        shot("06-input-settings-bottom")

        // 关于
        tab("关于")
        shot("07-about")
        app.swipeUp()
        shot("07-about-bottom")
    }

    func testDownloadPauseResumeFinishDelete() {
        launch(["SIM_DOWNLOAD_SECONDS": "5"])
        tab("AI 选词")
        tapMode("local")
        cleanModels()
        let id = "qwen2.5-0.5b-q4km"
        scrollTo(button("download-\(id)"))
        button("download-\(id)").tap()
        XCTAssertTrue(button("pause-\(id)").waitForExistence(timeout: 5))
        shot("10-downloading")
        button("pause-\(id)").tap()
        XCTAssertTrue(button("resume-\(id)").waitForExistence(timeout: 5))
        shot("11-paused")
        button("resume-\(id)").tap()
        XCTAssertTrue(button("delete-\(id)").waitForExistence(timeout: 20), "下载没有完成")
        shot("12-downloaded")
        button("delete-\(id)").tap()
        app.buttons["删除"].firstMatch.tap()
        XCTAssertTrue(button("download-\(id)").waitForExistence(timeout: 5))
    }

    func testFailureThenAbandon() {
        launch(["SIM_DOWNLOAD_SECONDS": "5", "SIM_DOWNLOAD_FAIL": "qwen3-0.6b-q4km"])
        tab("AI 选词")
        tapMode("local")
        cleanModels()
        let id = "qwen3-0.6b-q4km"
        scrollTo(button("download-\(id)"))
        button("download-\(id)").tap()
        XCTAssertTrue(button("retry-\(id)").waitForExistence(timeout: 15))
        shot("13-failed")
        button("cancel-\(id)").tap()
        XCTAssertTrue(button("download-\(id)").waitForExistence(timeout: 5))
    }

    func testSizeMismatchIsRejected() {
        launch(["SIM_DOWNLOAD_SECONDS": "3", "SIM_DOWNLOAD_SHORT": "qwen3-0.6b-q4km"])
        tab("AI 选词")
        tapMode("local")
        cleanModels()
        let id = "qwen3-0.6b-q4km"
        scrollTo(button("download-\(id)"))
        button("download-\(id)").tap()
        XCTAssertTrue(element("failure-\(id)").waitForExistence(timeout: 15))
        shot("14-size-mismatch")
        XCTAssertFalse(button("use-\(id)").exists)
        button("cancel-\(id)").tap()
    }

    func testCellularWarning() {
        launch(["SIM_EXPENSIVE": "1"])
        tab("AI 选词")
        tapMode("local")
        cleanModels()
        let id = "qwen2.5-0.5b-q4km"
        scrollTo(button("download-\(id)"))
        shot("15-cellular-banner")
        button("download-\(id)").tap()
        // iPhone 上这个对话框是气泡样式，没有单独的「取消」键，点气泡外面就是取消
        let proceed = app.buttons.matching(NSPredicate(format: "label BEGINSWITH '继续下载'")).firstMatch
        XCTAssertTrue(proceed.waitForExistence(timeout: 5))
        shot("16-cellular-confirm")
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.05, dy: 0.08)).tap()
        XCTAssertTrue(button("download-\(id)").waitForExistence(timeout: 5))
        XCTAssertTrue(button("download-\(id)").exists, "取消后不该开始下载")
    }

    func testNotEnoughSpace() {
        launch(["SIM_FREE_BYTES": "150000000"])
        tab("AI 选词")
        tapMode("local")
        cleanModels()
        let id = "qwen2.5-0.5b-q4km"
        scrollTo(button("download-\(id)"))
        button("download-\(id)").tap()
        XCTAssertTrue(app.alerts["存储空间不够"].waitForExistence(timeout: 5))
        shot("17-not-enough-space")
        app.alerts.buttons["好"].tap()
        XCTAssertTrue(button("download-\(id)").exists)
    }

    /// 首页的试打字框里，AI 拼音键盘照常工作（需要先运行 scripts/enable-keyboard.sh）
    func testKeyboardWorksInTryItField() {
        launch()
        app.swipeUp()
        let field = app.textViews["tryTextField"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap()
        XCTAssertTrue(app.keys["q"].firstMatch.waitForExistence(timeout: 10), "AI 拼音键盘没有出现")
        for letter in "nihao" { app.keys[String(letter)].firstMatch.tap() }
        let candidate = app.buttons["你好"].firstMatch
        XCTAssertTrue(candidate.waitForExistence(timeout: 5))
        shot("08-try-it-composing")
        candidate.tap()
        let done = NSPredicate { _, _ in (field.value as? String) == "你好" }
        let wait = expectation(for: done, evaluatedWith: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [wait], timeout: 3), .completed, "输入框内容是 \(field.value ?? "nil")")
        shot("08-try-it-committed")
    }
}
