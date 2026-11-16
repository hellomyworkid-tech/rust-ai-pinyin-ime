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

    func testTapSyllableThenTypeReplacesThatSyllable() {
        type("zhongguoneiyoueuv")
        guard case .update(let tapped) = engine.tapSyllable(at: 2) else { return XCTFail() }
        XCTAssertEqual(tapped.activeSyllableIndex, 2)
        XCTAssertEqual(tapped.syllableSpans[2].codeKey, "nei")

        let edited = type("mei")
        XCTAssertNil(edited.activeSyllableIndex)
        XCTAssertEqual(edited.editedByteRange, IMEByteRange(start: 8, end: 11))
        XCTAssertEqual(edited.syllableSpans.prefix(4).map(\.codeKey), ["zhong", "guo", "mei", "you"])
        XCTAssertTrue(edited.visibleCandidates.first?.text.hasPrefix("中国没有") == true,
                      "首选应以『中国没有』开头，实际：\(edited.visibleCandidates.first?.text ?? "")")

        // 单音节改写为双音节（wo -> tamen）：左右两侧均同步高亮 ta men 与「他们」
        engine.reset()
        type("nihaowozaizheli")
        guard case .update(let tappedWo) = engine.tapSyllable(at: 2) else { return XCTFail() }
        let tappedModel = CompositionModel(view: tappedWo)
        XCTAssertEqual(tappedModel.syllables.map(\.isActive), [false, false, true, false, false, false])
        XCTAssertEqual(tappedModel.sentence, [
            IMESentencePart(text: "你好", pinned: false),
            IMESentencePart(text: "我", pinned: true),
            IMESentencePart(text: "在这里", pinned: false),
        ])

        let multi = type("tamen")
        XCTAssertEqual(multi.editedByteRange, IMEByteRange(start: 5, end: 10))
        let multiModel = CompositionModel(view: multi)
        XCTAssertEqual(multiModel.syllables.map(\.code), ["ni", "hao", "ta", "men", "zai", "zhe", "li"])
        XCTAssertEqual(multiModel.syllables.map(\.isActive), [false, false, true, true, false, false, false])
        XCTAssertFalse(multiModel.syllables.contains { $0.isPinned })
        XCTAssertEqual(multiModel.sentence, [
            IMESentencePart(text: "你好", pinned: false),
            IMESentencePart(text: "他们", pinned: true),
            IMESentencePart(text: "在这里", pinned: false),
        ])
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
        XCTAssertEqual(controller.pager.fetchedCount, min(90, engine.currentView().totalCandidates))
        // 系统字体显示不出来的生僻字（方框）不进列表
        XCTAssertTrue(controller.pager.candidates.allSatisfy { CandidateRenderability.isRenderable($0.text) })
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

    func testTouchBackspaceOnTappedSyllableAndSnapshotRestore() throws {
        XCTAssertNil(engine.compositionSnapshot(), "空闲时不应有快照")

        // 输入 nihaotamenzaizheli，点选 men（下标 3）按退格：从 men 末尾删起，不误删 ta 的 a
        let initial = type("nihaotamenzaizheli")
        XCTAssertEqual(initial.syllableSpans[3].codeKey, "men")
        guard case .update = engine.tapSyllable(at: 3) else { return XCTFail("应当更新") }
        guard case .update(let bs1) = engine.handle(.backspace) else { return XCTFail("应当更新") }
        XCTAssertEqual(bs1.editedByteRange, IMEByteRange(start: 7, end: 9))
        guard case .update(let bs2) = engine.handle(.backspace) else { return XCTFail("应当更新") }
        XCTAssertEqual(bs2.preeditText, "ni'hao'ta'm'zai'zhe'li")
        XCTAssertEqual(bs2.editedByteRange, IMEByteRange(start: 7, end: 8))

        // 导出快照并在全新 RustEngine 实例上恢复（模拟切换应用或截图后回到输入框）
        let snapshot = try XCTUnwrap(engine.compositionSnapshot())
        let fresh = try Self.makeEngine()
        guard case .update(let restored) = fresh.restoreComposition(snapshot) else {
            return XCTFail("快照应当恢复成功")
        }
        XCTAssertTrue(fresh.isComposing)
        XCTAssertEqual(restored.preeditText, "ni'hao'ta'm'zai'zhe'li")
        XCTAssertEqual(restored.editedByteRange, IMEByteRange(start: 7, end: 8))

        // 恢复后继续补打 ei 得到 mei，改写区间依然保持蓝色高亮
        _ = fresh.handle(.character("e"))
        guard case .update(let afterMei) = fresh.handle(.character("i")) else { return XCTFail("应当更新") }
        XCTAssertEqual(afterMei.preeditText, "ni'hao'ta'mei'zai'zhe'li")
        XCTAssertEqual(afterMei.editedByteRange, IMEByteRange(start: 7, end: 10))
    }

    func testPrefixSelectionKeepsTrailingJieAndYueIntact() throws {
        // 1) sijie：首选保持「四届」，展开网格内包含跨音节联想「司机」「司机饿」；
        //    选前缀「司」后音节保持 si + jie，前排为 jie 单字 +「饥饿」，后部完整补齐全部 ji 单字（含「饥」）
        _ = type("sijie")
        let allSijie = engine.candidates(from: 0, count: 60)
        XCTAssertEqual(allSijie.first?.text, "四届")
        let sijieTop25 = allSijie.prefix(25).map(\.text)
        XCTAssertTrue(sijieTop25.contains("司机") && sijieTop25.contains("司机饿"), "\(sijieTop25)")
        let siIndex = try XCTUnwrap(allSijie.firstIndex { $0.text == "司" })
        guard case .update(let afterSi) = engine.selectCandidate(at: siIndex) else {
            return XCTFail("选前缀『司』应当留在组合态")
        }
        XCTAssertEqual(afterSi.syllableSpans.map(\.codeKey), ["si", "jie"])
        XCTAssertEqual(afterSi.activeSyllableIndex, 1)
        XCTAssertFalse(afterSi.composedFullSentence.contains("饥饿"))
        XCTAssertGreaterThan(afterSi.totalCandidates, 180)
        let jieTexts = engine.candidates(from: 0, count: 20).map(\.text)
        XCTAssertEqual(jieTexts.first?.count, 1)
        XCTAssertTrue(jieTexts.contains("借") && jieTexts.contains("饥饿"), "\(jieTexts)")
        XCTAssertFalse(jieTexts.contains("级") || jieTexts.contains("几"), "\(jieTexts)")
        let fullAfterSi = engine.candidates(from: 0, count: 200).map(\.text)
            + engine.candidates(from: 200, count: 100).map(\.text)
        XCTAssertTrue(fullAfterSi.contains("饥"), "后部 ji 备选字应完整包含『饥』")
        engine.reset()

        // 2) dongyue 选前缀「东」：音节保持 dong + yue（不拆成 dong + yu + e），「余额」保留在前 10 个候选中
        _ = type("dongyue")
        let allDongyue = engine.candidates(from: 0, count: 60)
        let dongIndex = try XCTUnwrap(allDongyue.firstIndex { $0.text == "东" })
        guard case .update(let afterDong) = engine.selectCandidate(at: dongIndex) else {
            return XCTFail("选前缀『东』应当留在组合态")
        }
        XCTAssertEqual(afterDong.syllableSpans.map(\.codeKey), ["dong", "yue"])
        XCTAssertEqual(afterDong.activeSyllableIndex, 1)
        XCTAssertFalse(afterDong.composedFullSentence.contains("余额"))
        let yueTexts = engine.candidates(from: 0, count: 10).map(\.text)
        XCTAssertEqual(yueTexts.first?.count, 1)
        XCTAssertTrue(yueTexts.contains("月") && yueTexts.contains("越") && yueTexts.contains("余额"), "\(yueTexts)")
        XCTAssertFalse(yueTexts.contains("于") || yueTexts.contains("与"), "\(yueTexts)")
        engine.reset()

        // 3) 单独输入 jie / yue，以及多段整句 chaxunyue / nayue：单音节优先且保留「饥饿」「余额」联想
        _ = type("jie")
        let standaloneJie = engine.candidates(from: 0, count: 20).map(\.text)
        XCTAssertEqual(standaloneJie.first, "接")
        XCTAssertTrue(standaloneJie.contains("饥饿"), "\(standaloneJie)")
        engine.reset()

        _ = type("yue")
        let standaloneYue = engine.candidates(from: 0, count: 10).map(\.text)
        XCTAssertEqual(standaloneYue.first, "越")
        XCTAssertTrue(standaloneYue.contains("余额"), "\(standaloneYue)")
        engine.reset()

        XCTAssertEqual(type("chaxunyue").composedFullSentence, "查询余额")
        engine.reset()

        _ = type("nayue")
        let nayueAll = engine.candidates(from: 0, count: 40).map(\.text)
        XCTAssertEqual(nayueAll.first, "那月")
        XCTAssertTrue(nayueAll.contains("那余额"), "\(nayueAll)")
    }

    func testModernVocabularyAndMisreadPronunciationHints() throws {
        // 1) 高频现代词汇与短剧搭配
        for (py, expected) in [
            ("shouzhongrenqun", "受众人群"),
            ("renshe", "人设"),
            ("nvpinduanju", "女频短剧"),
            ("fuhenvpinduanju", "符合女频短剧"),
        ] {
            let view = type(py)
            XCTAssertEqual(view.visibleCandidates.first?.text, expected, py)
            engine.reset()
        }

        // 2) hundun：#0 混沌（无提示），#1 馄饨（带正确拼音提示）；点选「馄饨」仅上屏「馄饨」
        let hundun = type("hundun")
        XCTAssertGreaterThanOrEqual(hundun.visibleCandidates.count, 2)
        XCTAssertEqual(hundun.visibleCandidates[0].text, "混沌")
        XCTAssertNil(hundun.visibleCandidates[0].pronunciationHint)
        XCTAssertEqual(hundun.visibleCandidates[1].text, "馄饨")
        XCTAssertEqual(hundun.visibleCandidates[1].pronunciationHint, "馄 (hun) 饨 (tun)")

        guard case .commit(let committed, _) = engine.selectCandidate(at: 1) else {
            return XCTFail("点选『馄饨』应当直接上屏")
        }
        XCTAssertEqual(committed, "馄饨")

        // 3) 输入正确拼音 huntun 时不展示错音提示
        let huntun = type("huntun")
        XCTAssertEqual(huntun.visibleCandidates.first?.text, "馄饨")
        XCTAssertNil(huntun.visibleCandidates.first?.pronunciationHint)
        engine.reset()

        // 4) 个人习惯词半音节联想（敲过 xionghan -> 凶悍 后，打 xiongh 首选即「凶悍」）与日常构词/热梗长句
        XCTAssertEqual(type("xionghan").visibleCandidates.first?.text, "凶悍")
        guard case .commit(let xhCommitted, _) = engine.handle(.space) else {
            return XCTFail("空格应上屏凶悍")
        }
        XCTAssertEqual(xhCommitted, "凶悍")
        XCTAssertEqual(type("xiongh").visibleCandidates.first?.text, "凶悍")
        engine.reset()

        for (py, expected) in [
            ("furenqu", "富人区"),
            ("zaya", "咋呀"),
            ("chigongjiafande", "吃公家饭的"),
            ("chenxianzaiai", "趁现在 AI"),
            ("chuntangping", "纯躺平"),
            ("gonghu", "公户"),
            ("sihangkehu", "私行客户"),
            ("jiewa", "接娃"),
            ("baixindeqian", "百姓的钱"),
            ("baixindeqiansanqifenzhang", "百姓的钱三七分账"),
            ("haoshenchuleqianbaixingcaihuichu", "豪绅出了钱百姓才会出"),
        ] {
            XCTAssertEqual(type(py).visibleCandidates.first?.text, expected, py)
            engine.reset()
        }
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
