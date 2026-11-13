import XCTest

/// 真引擎（Rust，经 C 接口）的测试。词库直接从源码目录读（模拟器看得到 Mac 的文件系统），
/// 所以要先跑过 scripts/build-rust.sh；找不到词库时这些测试跳过。
final class RustEngineTests: XCTestCase {
    private var engine: RustEngine!

    /// platforms/ios/Resources/merged_lexicon.bin
    static let lexiconURL: URL = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("Resources/merged_lexicon.bin")

    static func makeEngine(userDictionary: URL? = nil) throws -> RustEngine {
        guard FileManager.default.fileExists(atPath: lexiconURL.path) else {
            throw XCTSkip("没有词库 \(lexiconURL.path)，先运行 scripts/build-rust.sh")
        }
        return try RustEngine(lexiconURL: lexiconURL, userDictionaryURL: userDictionary)
    }

    override func setUpWithError() throws {
        try super.setUpWithError()
        engine = try Self.makeEngine()
    }

    override func tearDown() {
        engine = nil
        super.tearDown()
    }

    @discardableResult
    private func type(_ letters: String, file: StaticString = #filePath, line: UInt = #line) -> IMEView {
        var last = IMEView.idle
        for letter in letters {
            guard case .update(let view) = engine.handle(.character(letter)) else {
                XCTFail("打 \(letter) 应当得到 update", file: file, line: line)
                return last
            }
            last = view
        }
        return last
    }

    // MARK: - 与 Rust 状态机原始输出一致

    func testNihaoViewEqualsFixture() throws {
        let view = type("nihao")
        // fixture 是同一份词库、默认设置、无上文时状态机的完整输出；精简后的视图解码出来应当一模一样
        XCTAssertEqual(view, try Fixtures.view("update_nihao"))
        XCTAssertEqual(engine.currentView(), view)
    }

    func testLongSentenceViewEqualsFixture() throws {
        let expected = try Fixtures.view("update_long_sentence")
        let letters = expected.syllableSpans.map(\.codeKey).joined()
        XCTAssertEqual(type(letters), expected)
    }

    // MARK: - 按键

    func testTypeAndCommitWithSpace() {
        XCTAssertFalse(engine.isComposing)
        let view = type("nihao")
        XCTAssertTrue(engine.isComposing)
        XCTAssertEqual(view.visibleCandidates.first?.text, "你好")
        XCTAssertEqual(view.preeditText, "ni'hao")
        XCTAssertEqual(view.syllableSpans.map(\.codeKey), ["ni", "hao"])
        XCTAssertEqual(view.pageSize, 30)

        guard case .commit(let text, let after) = engine.handle(.space) else {
            return XCTFail("空格应当上屏首选")
        }
        XCTAssertEqual(text, "你好")
        XCTAssertEqual(after, .idle)
        XCTAssertFalse(engine.isComposing)
        XCTAssertEqual(engine.currentView(), .idle)
    }

    func testIdleKeysPassThrough() {
        for key in [IMEKey.space, .enter, .backspace, .escape, .pageUp, .pageDown, .cursorLeft, .cursorRight,
                    .character("A"), .character(","), .character("1"), .character("你")] {
            guard case .passThrough = engine.handle(key) else {
                return XCTFail("空闲时 \(key) 应当放行")
            }
        }
        guard case .passThrough = engine.selectCandidate(at: 0) else { return XCTFail("空闲时点候选应当放行") }
        guard case .passThrough = engine.commitFullSentence() else { return XCTFail("空闲时整句上屏应当放行") }
    }

    func testEnterCommitsRawLettersAndBackspaceEdits() {
        type("nihao")
        guard case .commit(let raw, _) = engine.handle(.enter) else { return XCTFail("回车应当上屏原样字母") }
        XCTAssertEqual(raw, "nihao")

        type("ni")
        guard case .update(let shorter) = engine.handle(.backspace) else { return XCTFail() }
        XCTAssertEqual(shorter.preeditText, "n")
        guard case .update(let empty) = engine.handle(.backspace) else { return XCTFail() }
        XCTAssertEqual(empty, .idle)
        XCTAssertFalse(engine.isComposing)

        type("ni")
        engine.reset()
        XCTAssertFalse(engine.isComposing)
    }

    func testToggleEnglish() {
        XCTAssertFalse(engine.isEnglishMode)
        guard case .update(let view) = engine.handle(.toggleChineseEnglish) else { return XCTFail("切换应当返回 update") }
        XCTAssertEqual(view, .idle)
        XCTAssertTrue(engine.isEnglishMode)
        guard case .passThrough = engine.handle(.character("n")) else { return XCTFail("英文模式字母应当放行") }
        _ = engine.handle(.toggleChineseEnglish)
        XCTAssertFalse(engine.isEnglishMode)

        type("nihao")
        guard case .commit(let raw, _) = engine.handle(.toggleChineseEnglish) else {
            return XCTFail("输入中途切换应当上屏原样字母")
        }
        XCTAssertEqual(raw, "nihao")
        XCTAssertTrue(engine.isEnglishMode)
    }

    func testSelectingPrefixCandidatePinsIt() throws {
        let view = type("jintiantianqihenhao")
        XCTAssertEqual(view.composedFullSentence, "今天天气很好")
        let index = try XCTUnwrap(view.visibleCandidates.firstIndex { $0.text == "今天" })
        guard case .update(let pinned) = engine.selectCandidate(at: index) else {
            return XCTFail("选前缀词应当锁定并继续")
        }
        XCTAssertEqual(pinned.fullSentenceParts.first, IMESentencePart(text: "今天", pinned: true))
        XCTAssertNotNil(pinned.activeSyllableIndex)
        guard case .commit(let text, _) = engine.commitFullSentence() else { return XCTFail() }
        XCTAssertEqual(text, "今天天气很好")
    }

    func testTapSyllableTogglesSingleSyllableMode() {
        type("nihao")
        guard case .update(let tapped) = engine.tapSyllable(at: 1) else { return XCTFail() }
        XCTAssertEqual(tapped.activeSyllableIndex, 1)
        XCTAssertEqual(tapped.visibleCandidates.first?.text, "好")
        guard case .update(let back) = engine.tapSyllable(at: 1) else { return XCTFail() }
        XCTAssertNil(back.activeSyllableIndex)
        XCTAssertEqual(back.visibleCandidates.first?.text, "你好")
    }

    func testPagingThroughCandidates() {
        let first = type("shi")
        XCTAssertEqual(first.pageIndex, 0)
        XCTAssertEqual(first.visibleCandidates.count, 30)
        XCTAssertGreaterThan(first.totalCandidates, 30)
        guard case .update(let second) = engine.handle(.pageDown) else { return XCTFail() }
        XCTAssertEqual(second.pageIndex, 1)
        XCTAssertNotEqual(second.visibleCandidates.first, first.visibleCandidates.first)
        // 跨页下标
        guard case .commit(let text, _) = engine.selectCandidate(at: 30) else { return XCTFail() }
        XCTAssertEqual(text, second.visibleCandidates.first?.text)
    }

    func testCandidateRangeDoesNotMoveTheEngine() {
        let first = type("shi")
        let more = engine.candidates(from: 30, count: 30)
        XCTAssertEqual(more.count, min(30, first.totalCandidates - 30))
        XCTAssertNotEqual(more.first, first.visibleCandidates.first)
        XCTAssertEqual(engine.currentView().pageIndex, 0, "取后面的候选不翻页")
        XCTAssertEqual(engine.candidates(from: 0, count: 30), first.visibleCandidates)
        XCTAssertTrue(engine.candidates(from: first.totalCandidates, count: 30).isEmpty)
        // 与翻页看到的是同一批候选，下标就是 selectCandidate 的全局下标
        guard case .update(let second) = engine.handle(.pageDown) else { return XCTFail() }
        XCTAssertEqual(more.prefix(second.visibleCandidates.count), second.visibleCandidates[...])
        engine.reset()
        XCTAssertTrue(engine.candidates(from: 0, count: 30).isEmpty, "空闲时没有候选")
    }

    /// SW-1：候选栏往后加载了更多候选，空格上屏的仍是用户看到的第一个，引擎也不会把别的词当成用户的选择记住
    func testLoadMoreThenSpaceCommitsTheTopCandidate() {
        let output = RecordingTextOutput()
        let engine = self.engine!
        let controller = InputController(makeEngine: { engine })
        controller.output = output

        "shi".forEach { controller.typeLetter($0) }
        let top = controller.pager.candidates.first?.text
        XCTAssertEqual(top, "是")
        XCTAssertTrue(controller.loadMoreCandidates())
        XCTAssertTrue(controller.loadMoreCandidates())
        XCTAssertEqual(controller.pager.candidates.count, min(90, engine.currentView().totalCandidates))
        XCTAssertEqual(controller.pager.candidates.first?.text, top, "加载更多只往后接")
        XCTAssertEqual(engine.currentView().pageIndex, 0, "引擎没有翻页")

        controller.space()
        XCTAssertEqual(output.hostText, top)
        "shi".forEach { controller.typeLetter($0) }
        XCTAssertEqual(controller.pager.candidates.first?.text, top, "没有学到别的词")
        controller.resetComposition()
    }

    // MARK: - 设置与上文

    func testFuzzySettingsReachTheEngine() {
        func candidates(_ letters: String) -> [String] {
            defer { engine.reset() }
            return type(letters).visibleCandidates.map(\.text)
        }
        var settings = IMESettings()
        settings.fuzzy.sSh = false
        engine.apply(settings)
        XCTAssertFalse(candidates("si").contains("是"))
        settings.fuzzy.sSh = true
        engine.apply(settings)
        XCTAssertTrue(candidates("si").contains("是"))
    }

    func testDefaultSettingsMatchTheEngineDefaults() throws {
        // 应用默认设置不应改变引擎的行为：fixture 是在引擎自己的默认值下生成的
        engine.apply(IMESettings())
        XCTAssertEqual(type("nihao"), try Fixtures.view("update_nihao"))
    }

    func testContextChangesTheTopCandidate() {
        engine.setContext("坚决防止国家机关滥用")
        XCTAssertEqual(type("quanli").visibleCandidates.first?.text, "权力")
        engine.reset()
        engine.setContext("宪法保障公民的合法")
        XCTAssertEqual(type("quanli").visibleCandidates.first?.text, "权利")
        engine.reset()
        engine.setContext(nil)
    }

    // MARK: - 接到按键规则上

    func testInputControllerDrivesTheRealEngine() {
        let output = RecordingTextOutput()
        let engine = self.engine!
        let controller = InputController(makeEngine: { engine })
        controller.output = output

        "nihao".forEach { controller.typeLetter($0) }
        XCTAssertTrue(controller.isComposing)
        XCTAssertEqual(controller.pager.candidates.first?.text, "你好")
        controller.space()
        XCTAssertEqual(output.hostText, "你好")

        // 输入中途按逗号：先整句上屏，再插中文逗号
        "shijie".forEach { controller.typeLetter($0) }
        controller.typeSymbol(",")
        XCTAssertEqual(output.hostText, "你好世界，")
        XCTAssertFalse(controller.isComposing)

        // 点选候选、退格
        "ni".forEach { controller.typeLetter($0) }
        controller.selectCandidate(at: 0)
        XCTAssertEqual(output.hostText, "你好世界，你")
        controller.backspace()
        XCTAssertEqual(output.hostText, "你好世界，")

        // 往后翻页，候选接在后面
        "shi".forEach { controller.typeLetter($0) }
        let firstPage = controller.pager.candidates.count
        XCTAssertTrue(controller.loadMoreCandidates())
        XCTAssertGreaterThan(controller.pager.candidates.count, firstPage)
        controller.resetComposition()
        XCTAssertFalse(engine.isComposing)

        // 切英文：字母直接上屏
        controller.toggleLanguage()
        XCTAssertFalse(controller.isChineseMode)
        "ok".forEach { controller.typeLetter($0) }
        XCTAssertEqual(output.hostText, "你好世界，ok")
    }

    // MARK: - 个人词频、错误、性能

    func testUserDictionaryIsSavedAndReloaded() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("RustEngineTests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = directory.appendingPathComponent("nested/user_dict.json")

        var learner: RustEngine? = try Self.makeEngine(userDictionary: file)
        engine = learner
        // 首选之后的第一个词（表情候选选中后不进个人词库，跳过）
        let second = try XCTUnwrap(type("shiyan").visibleCandidates.dropFirst().first { $0.tag != "[表情]" }?.text)
        // 连选两次这个词，引擎会记住
        for _ in 0..<2 {
            if !engine.isComposing { type("shiyan") }
            let index = try XCTUnwrap(engine.currentView().visibleCandidates.firstIndex { $0.text == second })
            guard case .commit = engine.selectCandidate(at: index) else { return XCTFail("应当上屏") }
        }
        // 写盘在引擎的后台线程
        let saved = expectation(for: NSPredicate { _, _ in FileManager.default.fileExists(atPath: file.path) },
                                evaluatedWith: nil)
        wait(for: [saved], timeout: 5)
        learner = nil

        engine = try Self.makeEngine(userDictionary: file)
        XCTAssertEqual(type("shiyan").visibleCandidates.first?.text, second, "重新打开后应当记得上次的选择")
    }

    /// SW-5：另一个引擎（另一个键盘进程）写过个人词库后，reloadUserDictionaryIfChanged 读入它学到的词，
    /// 之后本引擎写盘就不会把那些词盖掉
    func testReloadPicksUpAnotherEnginesLearning() throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("RustEngineTests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let file = directory.appendingPathComponent("user_dict.json")

        let other = try Self.makeEngine(userDictionary: file)
        XCTAssertFalse(other.reloadUserDictionaryIfChanged(), "文件没变")

        var learner: RustEngine? = try Self.makeEngine(userDictionary: file)
        engine = learner
        let second = try XCTUnwrap(type("shiyan").visibleCandidates.dropFirst().first { $0.tag != "[表情]" }?.text)
        for _ in 0..<2 {
            if !engine.isComposing { type("shiyan") }
            let index = try XCTUnwrap(engine.currentView().visibleCandidates.firstIndex { $0.text == second })
            guard case .commit = engine.selectCandidate(at: index) else { return XCTFail("应当上屏") }
        }
        // 释放时等写盘完成
        engine = nil
        learner = nil
        XCTAssertTrue(FileManager.default.fileExists(atPath: file.path))

        engine = other
        XCTAssertTrue(other.reloadUserDictionaryIfChanged(), "别的引擎写过，应当重新读入")
        XCTAssertFalse(other.reloadUserDictionaryIfChanged(), "读过之后没有再变")
        XCTAssertEqual(type("shiyan").visibleCandidates.first?.text, second)
        engine.reset()

        let memoryOnly = try Self.makeEngine()
        XCTAssertFalse(memoryOnly.reloadUserDictionaryIfChanged(), "没有启用持久化")
    }

    func testMissingLexiconThrows() {
        let missing = URL(fileURLWithPath: "/no/such/lexicon.bin")
        XCTAssertThrowsError(try RustEngine(lexiconURL: missing, userDictionaryURL: nil)) { error in
            XCTAssertTrue(error.localizedDescription.contains("lexicon.bin"), "\(error)")
        }
    }

    func testFootprintIsReported() {
        XCTAssertGreaterThan(RustEngine.memoryFootprint, 1_000_000)
    }

    /// 一次按键（解码 + JSON + Swift 解码）的耗时。打一句 19 个字母的话再取消，算每键平均
    func testKeystrokeLatency() {
        let letters = Array("jintiantianqihenhao")
        type("nihao")
        engine.reset()
        var worst = 0.0
        var total = 0.0
        var count = 0
        for _ in 0..<20 {
            for letter in letters {
                let start = CFAbsoluteTimeGetCurrent()
                _ = engine.handle(.character(letter))
                let elapsed = CFAbsoluteTimeGetCurrent() - start
                worst = max(worst, elapsed)
                total += elapsed
                count += 1
            }
            engine.reset()
        }
        let mean = total / Double(count)
        print(String(format: "RustEngine keystroke: mean %.2f ms, worst %.2f ms over %d keys", mean * 1000, worst * 1000, count))
        // 模拟器上通常 1–3 毫秒；这里只防明显的退化
        XCTAssertLessThan(mean, 0.02)
    }
}
