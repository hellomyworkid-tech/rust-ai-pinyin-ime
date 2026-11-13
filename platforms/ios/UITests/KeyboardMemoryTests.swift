import XCTest

/// 真机上键盘扩展本身的内存：调试构建的键盘工具栏状态行末尾是「已用/上限MB」（phys_footprint 与
/// phys_footprint + os_proc_available_memory()），这里通过无障碍读出来。
///
/// 只在设置了 KEYBOARD_MEMORY_MODEL（xcodebuild 用 TEST_RUNNER_KEYBOARD_MEMORY_MODEL 传入，取 ModelCatalog 的 id）时运行：
/// 模型必须已经在 App Group 的模型目录里（真机评测 AIPinyinDeviceEval 会把拷进来的模型挪进去）。
/// 前提：手机上已经添加「AI 拼音」键盘，并且在某个输入框里用地球键切换到过它（系统会记住上次用的键盘）。
final class KeyboardMemoryTests: XCTestCase {
    func testKeyboardMemoryWithLocalModel() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let modelID = environment["KEYBOARD_MEMORY_MODEL"], !modelID.isEmpty else {
            throw XCTSkip("没有设置 KEYBOARD_MEMORY_MODEL")
        }
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchArguments = ["-hostTest"]
        app.launchEnvironment["HOST_TEST_TEXT"] = "我对天发誓这是我的"
        // 模型已经在 App Group 里：HostTestSetup 只需选中它（路径不会被用到）
        app.launchEnvironment["HOST_TEST_MODEL"] = environment["HOST_TEST_MODEL"] ?? "already-in-app-group"
        app.launchEnvironment["HOST_TEST_MODEL_ID"] = modelID
        app.launch()

        let field = app.textViews["hostTextField"].exists ? app.textViews["hostTextField"] : app.textFields["hostTextField"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.coordinate(withNormalizedOffset: CGVector(dx: 0.92, dy: 0.5)).tap()
        XCTAssertTrue(app.keys["q"].firstMatch.waitForExistence(timeout: 15),
                      "AI 拼音键盘没有出现：先在 设置 → 通用 → 键盘 里添加它，并在任意输入框里用地球键切换过去一次")

        let status = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "AI 选词")).firstMatch
        XCTAssertTrue(status.waitForExistence(timeout: 10), "找不到键盘的状态行")
        let loaded = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "本地模型")).firstMatch
        let start = Date()
        XCTAssertTrue(loaded.waitForExistence(timeout: 60), "本地模型没有加载：\(status.label)")
        // 状态行在加载完成时刷新，读数就是加载后的占用
        print("[keyboard-memory] \(modelID) 加载后（等待 \(String(format: "%.1f", Date().timeIntervalSince(start))) 秒）：\(loaded.label)")

        for letter in "shiyan" {
            let key = app.keys[String(letter)].firstMatch
            XCTAssertTrue(key.waitForExistence(timeout: 5))
            key.tap()
        }
        let boosted = app.buttons["誓言，AI 推荐"].firstMatch
        let picked = boosted.waitForExistence(timeout: 5)
        print("[keyboard-memory] \(modelID) 上文「我对天发誓这是我的」+ shiyan：首选\(picked ? "是" : "不是")带 AI 标记的「誓言」")
        if picked {
            boosted.tap()
        } else {
            app.keys["空格"].firstMatch.tap()
        }
        // 上屏之后状态行重新出现，读数包括打过一次分之后的占用
        XCTAssertTrue(loaded.waitForExistence(timeout: 10))
        print("[keyboard-memory] \(modelID) 打分之后：\(loaded.label)")
        XCTAssertTrue(picked, "模型没有把「誓言」提到首位")
    }
}
