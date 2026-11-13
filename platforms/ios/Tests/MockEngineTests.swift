import XCTest

/// MockEngine 要像真引擎：视图形状对得上 fixture，词来自 MockLexicon
final class MockEngineTests: XCTestCase {
    private var scheduler: ManualScheduler!
    private var engine: MockEngine!

    override func setUp() {
        super.setUp()
        scheduler = ManualScheduler()
        engine = MockEngine(schedule: scheduler.schedule)
    }

    @discardableResult
    private func type(_ letters: String) -> IMEView {
        var last = IMEView.idle
        for c in letters {
            if case .update(let view) = engine.handle(.character(c)) { last = view }
        }
        return last
    }

    private func texts(_ view: IMEView) -> [String] { view.visibleCandidates.map(\.text) }

    // MARK: - 与 fixture 对照

    func testNihaoViewMatchesRustFixture() throws {
        let fixture = try Fixtures.view("update_nihao")
        let view = type("nihao")
        XCTAssertEqual(view.visible, fixture.visible)
        XCTAssertEqual(view.preeditText, fixture.preeditText)
        XCTAssertEqual(view.pageIndex, fixture.pageIndex)
        XCTAssertEqual(view.pageSize, fixture.pageSize)
        XCTAssertEqual(view.selectedCandidateIndex, fixture.selectedCandidateIndex)
        XCTAssertEqual(view.composedFullSentence, fixture.composedFullSentence)
        XCTAssertEqual(view.fullSentenceParts, fixture.fullSentenceParts)
        XCTAssertEqual(view.syllableSpans, fixture.syllableSpans)
        XCTAssertEqual(view.cursorBytePos, fixture.cursorBytePos)
        XCTAssertEqual(view.activeSyllableIndex, fixture.activeSyllableIndex)
        XCTAssertEqual(view.ghostTextPreview, fixture.ghostTextPreview)
        let first = try XCTUnwrap(view.visibleCandidates.first)
        let expected = try XCTUnwrap(fixture.visibleCandidates.first)
        XCTAssertEqual(first, expected)
        // 前缀单字：consumedLen 是覆盖到的字节数
        let ni = try XCTUnwrap(view.visibleCandidates.first { $0.text == "你" })
        XCTAssertEqual(ni.consumedLen, 2)
        XCTAssertEqual(ni.preedit, "ni")
        XCTAssertEqual(ni.tag, "[前缀选词]")
        XCTAssertEqual(view.totalCandidates, view.visibleCandidates.count)
    }

    func testSyllableCursorMatchesFixtureShape() throws {
        let fixture = try Fixtures.view("update_syllable_cursor")
        type("nihao")
        guard case .update(let view) = engine.tapSyllable(at: 1) else { return XCTFail() }
        XCTAssertEqual(view.activeSyllableIndex, 1)
        XCTAssertEqual(view.cursorBytePos, 2, "光标在当前音节的起点")
        let single = try XCTUnwrap(MockLexicon.entry(for: ["hao"])?.candidates)
        XCTAssertEqual(Array(texts(view).prefix(single.count)), single)
        let first = try XCTUnwrap(view.visibleCandidates.first)
        XCTAssertEqual(first.tag, "(hao)")
        XCTAssertEqual(first.consumedLen, 3)
        // fixture 里同样是「(音节)」标签、consumedLen 为音节长度
        let fixtureFirst = try XCTUnwrap(fixture.visibleCandidates.first)
        let activeSpan = fixture.syllableSpans[try XCTUnwrap(fixture.activeSyllableIndex)]
        XCTAssertEqual(fixtureFirst.tag, "(\(activeSpan.codeKey))")
        XCTAssertEqual(fixtureFirst.consumedLen, activeSpan.end - activeSpan.start)
    }

    func testCommaWhileComposingMergesIntoPinyinLikeFixture() throws {
        let fixture = try Fixtures.view("commit_with_punctuation")
        let view = type("shiyan,")
        XCTAssertEqual(view.preeditText, fixture.preeditText)
        XCTAssertEqual(view.syllableSpans.map(\.codeKey), fixture.syllableSpans.map(\.codeKey))
        XCTAssertEqual(view.syllableSpans.map(\.start), fixture.syllableSpans.map(\.start))
        XCTAssertEqual(view.syllableSpans.last?.text, ",")
    }

    func testIdleViewMatchesEscapeFixture() throws {
        let fixture = try Fixtures.view("escape")
        type("ni")
        guard case .update(let view) = engine.handle(.escape) else { return XCTFail() }
        XCTAssertEqual(view, fixture)
        XCTAssertFalse(engine.isComposing)
    }

    // MARK: - 切分与候选

    func testSegmentation() {
        XCTAssertEqual(MockSegmenter.segment("nihao").map(\.code), ["ni", "hao"])
        XCTAssertEqual(MockSegmenter.segment("jintiantianqihenhao").map(\.code),
                       ["jin", "tian", "tian", "qi", "hen", "hao"])
        XCTAssertEqual(MockSegmenter.segment("zh").map(\.code), ["zh"])
        // 还没打完的音节（是某个音节的前缀）留在一起
        XCTAssertEqual(MockSegmenter.segment("niha").map(\.code), ["ni", "ha"])
        XCTAssertEqual(MockSegmenter.segment("nih").map(\.code), ["ni", "h"])
        let withApostrophe = MockSegmenter.segment("ni'hao")
        XCTAssertEqual(withApostrophe.map(\.code), ["ni", "hao"])
        XCTAssertEqual(withApostrophe.map(\.start), [0, 3], "分隔符占一个字节")
        let mixed = MockSegmenter.segment("shi,")
        XCTAssertEqual(mixed.last, MockSegmenter.Piece(code: ",", start: 3, end: 4, isPinyin: false))
    }

    func testCandidatesComeFromLexicon() {
        let view = type("nihao")
        XCTAssertEqual(Array(texts(view).prefix(3)), ["你好", "拟好", "你号"])
        XCTAssertTrue(texts(view).contains("你"))
        XCTAssertEqual(Set(texts(view)).count, texts(view).count, "候选不重复")
        XCTAssertEqual(view.visibleCandidates.first?.tag, "[词库]")
    }

    func testLongSentenceIsComposedFromWords() {
        let view = type("jintiantianqihenhao")
        XCTAssertEqual(view.composedFullSentence, "今天天气很好")
        XCTAssertEqual(view.visibleCandidates.first?.text, "今天天气很好")
        XCTAssertEqual(view.syllableSpans.map(\.text), ["今", "天", "天", "气", "很", "好"])
    }

    func testSentenceWithoutLexiconEntryIsTaggedAsSentence() {
        let view = type("womenchifan")
        XCTAssertEqual(view.composedFullSentence, "我们吃饭")
        XCTAssertEqual(view.visibleCandidates.first?.tag, "[整句]")
        XCTAssertEqual(view.visibleCandidates.first?.consumedLen, 11)
    }

    func testUnknownSyllableUsesFallback() {
        let view = type("x")
        XCTAssertEqual(texts(view), MockLexicon.fallbackCandidates(for: "x"))
    }

    func testPaging() {
        engine = MockEngine(pageSize: 3, schedule: scheduler.schedule)
        let first = type("shi")
        XCTAssertEqual(first.pageSize, 3)
        XCTAssertEqual(first.totalCandidates, 10)
        XCTAssertEqual(texts(first), ["是", "时", "事"])
        guard case .update(let second) = engine.handle(.pageDown) else { return XCTFail() }
        XCTAssertEqual(second.pageIndex, 1)
        XCTAssertEqual(texts(second), ["十", "市", "使"])
        for _ in 0..<5 { _ = engine.handle(.pageDown) }
        XCTAssertEqual(engine.currentView().pageIndex, 3, "停在最后一页")
        XCTAssertEqual(texts(engine.currentView()), ["石"])
        _ = engine.handle(.pageUp)
        XCTAssertEqual(engine.currentView().pageIndex, 2)
        // 全局下标选词
        guard case .commit(let text, _) = engine.selectCandidate(at: 9) else { return XCTFail() }
        XCTAssertEqual(text, "石")
    }

    func testCandidateRangeDoesNotMoveThePage() {
        engine = MockEngine(pageSize: 3, schedule: scheduler.schedule)
        type("shi")
        XCTAssertEqual(engine.candidates(from: 3, count: 3).map(\.text), ["十", "市", "使"])
        XCTAssertEqual(engine.candidates(from: 9, count: 5).map(\.text), ["石"])
        XCTAssertTrue(engine.candidates(from: 10, count: 3).isEmpty)
        XCTAssertEqual(engine.currentView().pageIndex, 0, "取后面的候选不翻页")
        engine.reset()
        XCTAssertTrue(engine.candidates(from: 0, count: 3).isEmpty, "空闲时没有候选")
    }

    /// 与 Rust 引擎一致：空格上屏全部候选里的第 0 个，之前翻没翻页都一样（SW-1）
    func testSpaceAfterPageDownCommitsTheFirstCandidate() {
        engine = MockEngine(pageSize: 3, schedule: scheduler.schedule)
        type("shi")
        _ = engine.handle(.pageDown)
        guard case .commit(let text, _) = engine.handle(.space) else { return XCTFail() }
        XCTAssertEqual(text, "是")
    }

    // MARK: - 选词与锁定

    func testSelectingPrefixPinsItAndContinues() throws {
        let view = type("nihao")
        let index = try XCTUnwrap(texts(view).firstIndex(of: "你"))
        guard case .update(let pinned) = engine.selectCandidate(at: index) else { return XCTFail("应继续输入") }
        XCTAssertEqual(pinned.fullSentenceParts, [IMESentencePart(text: "你", pinned: true),
                                                  IMESentencePart(text: "好", pinned: false)])
        XCTAssertEqual(pinned.visibleCandidates.first?.text, "好")
        XCTAssertEqual(pinned.visibleCandidates.first?.consumedLen, 5)
        guard case .commit(let text, let idle) = engine.selectCandidate(at: 0) else { return XCTFail("应上屏") }
        XCTAssertEqual(text, "你好")
        XCTAssertEqual(idle, .idle)
    }

    func testPickingSyllableCandidatePinsThatSyllable() {
        type("nihao")
        _ = engine.tapSyllable(at: 1)
        guard case .update(let view) = engine.selectCandidate(at: 1) else { return XCTFail() }
        XCTAssertNil(view.activeSyllableIndex)
        XCTAssertEqual(view.composedFullSentence, "你号")
        XCTAssertEqual(view.fullSentenceParts.last, IMESentencePart(text: "号", pinned: true))
    }

    func testBackspaceUndoesPinBeforeDeletingLetters() throws {
        let view = type("nihao")
        let index = try XCTUnwrap(texts(view).firstIndex(of: "你"))
        _ = engine.selectCandidate(at: index)
        guard case .update(let undone) = engine.handle(.backspace) else { return XCTFail() }
        XCTAssertEqual(undone.preeditText, "ni'hao")
        XCTAssertTrue(undone.fullSentenceParts.allSatisfy { !$0.pinned })
        guard case .update(let shorter) = engine.handle(.backspace) else { return XCTFail() }
        XCTAssertEqual(shorter.preeditText, "ni'ha")
    }

    func testBackspaceToEmptyReturnsIdle() {
        type("a")
        guard case .update(let view) = engine.handle(.backspace) else { return XCTFail() }
        XCTAssertEqual(view, .idle)
        XCTAssertFalse(engine.isComposing)
        guard case .passThrough = engine.handle(.backspace) else { return XCTFail("空闲时退格交还键盘") }
    }

    func testSyllableWithoutSingleEntryUsesCharactersFromWords() {
        type("jintian")
        guard case .update(let view) = engine.tapSyllable(at: 1) else { return XCTFail() }
        XCTAssertEqual(texts(view).prefix(2), ["天", "田"])
        XCTAssertFalse(texts(view).contains { $0.hasPrefix("〔") })
    }

    func testTapSameSyllableAgainLeavesSyllableMode() {
        type("nihao")
        _ = engine.tapSyllable(at: 0)
        guard case .update(let view) = engine.tapSyllable(at: 0) else { return XCTFail() }
        XCTAssertNil(view.activeSyllableIndex)
        XCTAssertEqual(view.visibleCandidates.first?.text, "你好")
    }

    func testCursorKeysMoveActiveSyllable() {
        type("nihao")
        _ = engine.handle(.cursorLeft)
        XCTAssertEqual(engine.currentView().activeSyllableIndex, 1)
        _ = engine.handle(.cursorLeft)
        _ = engine.handle(.cursorLeft)
        XCTAssertEqual(engine.currentView().activeSyllableIndex, 0)
        _ = engine.handle(.cursorRight)
        _ = engine.handle(.cursorRight)
        XCTAssertNil(engine.currentView().activeSyllableIndex)
    }

    // MARK: - 上屏与模式

    func testSpaceCommitsFirstCandidate() {
        type("nihao")
        guard case .commit(let text, _) = engine.handle(.space) else { return XCTFail() }
        XCTAssertEqual(text, "你好")
    }

    func testEnterCommitsRawLetters() {
        type("nihao")
        guard case .commit(let text, _) = engine.handle(.enter) else { return XCTFail() }
        XCTAssertEqual(text, "nihao")
    }

    func testCommitFullSentence() {
        type("jintiantianqihenhao")
        guard case .commit(let text, let view) = engine.commitFullSentence() else { return XCTFail() }
        XCTAssertEqual(text, "今天天气很好")
        XCTAssertEqual(view, .idle)
        guard case .passThrough = engine.commitFullSentence() else { return XCTFail("空闲时没有可上屏的") }
    }

    func testIdleKeysPassThrough() {
        for key in [IMEKey.space, .enter, .backspace, .escape, .pageDown, .character(","), .character("A")] {
            guard case .passThrough = engine.handle(key) else { return XCTFail("\(key) 应交还键盘") }
        }
    }

    func testUppercaseWhileComposingJoinsPinyin() {
        type("ni")
        let view = type("H")
        XCTAssertEqual(view.preeditText, "ni'h")
    }

    func testEnglishToggle() {
        guard case .update = engine.handle(.toggleChineseEnglish) else { return XCTFail() }
        XCTAssertTrue(engine.isEnglishMode)
        guard case .passThrough = engine.handle(.character("a")) else { return XCTFail("英文模式直通") }
        _ = engine.handle(.toggleChineseEnglish)
        XCTAssertFalse(engine.isEnglishMode)

        type("ni")
        guard case .commit(let raw, _) = engine.handle(.toggleChineseEnglish) else { return XCTFail() }
        XCTAssertEqual(raw, "ni")
        XCTAssertTrue(engine.isEnglishMode)
    }

    func testResetClearsInput() {
        type("ni")
        engine.reset()
        XCTAssertFalse(engine.isComposing)
        XCTAssertEqual(engine.currentView(), .idle)
    }

    // MARK: - 模拟的 AI

    func testSimulatedAIPromotesPickAfterDelay() {
        var updates = 0
        engine.onAsyncUpdate = { updates += 1 }
        let before = type("shiyan")
        XCTAssertEqual(before.visibleCandidates.first?.text, "实验")
        XCTAssertFalse(before.visibleCandidates.contains { $0.aiBoosted })
        XCTAssertEqual(scheduler.pending.map(\.delay), [0.25])

        scheduler.runAll()
        XCTAssertEqual(updates, 1)
        let after = engine.refreshAfterAI()
        XCTAssertEqual(after.visibleCandidates.first?.text, "誓言")
        XCTAssertEqual(after.visibleCandidates.first?.aiBoosted, true)
        XCTAssertEqual(after.visibleCandidates.first?.tag, "[AI 优选]")
        XCTAssertEqual(after.composedFullSentence, "誓言")
        XCTAssertEqual(after.visibleCandidates.filter(\.aiBoosted).count, 1)
    }

    func testStaleAIResultIsDropped() {
        var updates = 0
        engine.onAsyncUpdate = { updates += 1 }
        type("shiyan")
        _ = engine.handle(.backspace)  // 输入变了，之前排的 AI 结果作废
        scheduler.runAll()
        XCTAssertEqual(updates, 0)
    }

    func testAIPromotesWordInsideLongerSentence() {
        engine.onAsyncUpdate = {}
        type("womengongyuan")
        XCTAssertEqual(engine.currentView().composedFullSentence, "我们公元")
        scheduler.runAll()
        let view = engine.refreshAfterAI()
        XCTAssertEqual(view.composedFullSentence, "我们公园")
        XCTAssertEqual(view.visibleCandidates.first?.aiBoosted, true)
    }

    func testNoAIDelayForEntriesWithoutPick() {
        type("nihao")
        XCTAssertTrue(scheduler.pending.isEmpty, "没设 onChoiceRequest、词条也没有 aiPick 时不排异步任务")
    }

    func testChoiceRequestAndDelivery() throws {
        var requests: [IMEChoiceRequest] = []
        var updates = 0
        engine.onChoiceRequest = { requests.append($0) }
        engine.onAsyncUpdate = { updates += 1 }
        engine.setContext("我想对你说")
        type("nihao")
        scheduler.run(upTo: 0)
        let request = try XCTUnwrap(requests.last)
        XCTAssertEqual(request.pinyin, "ni'hao")
        XCTAssertEqual(request.candidates, ["你好", "拟好", "你号"])
        XCTAssertEqual(request.context, "我想对你说")
        XCTAssertEqual(requests.count, 1, "只为最新的输入发请求")

        engine.deliverChoice(id: request.id + 99, text: "拟好")
        engine.deliverChoice(id: request.id, text: "拟好")
        scheduler.runAll()
        XCTAssertEqual(updates, 1)
        let view = engine.refreshAfterAI()
        XCTAssertEqual(view.visibleCandidates.first?.text, "拟好")
        XCTAssertEqual(view.visibleCandidates.first?.aiBoosted, true)
        XCTAssertEqual(view.composedFullSentence, "拟好")
    }

    func testChoiceNilOrUnknownAnswerChangesNothing() throws {
        var requests: [IMEChoiceRequest] = []
        engine.onChoiceRequest = { requests.append($0) }
        type("nihao")
        scheduler.runAll()
        let id = try XCTUnwrap(requests.last?.id)
        engine.deliverChoice(id: id, text: nil)
        XCTAssertEqual(engine.currentView().visibleCandidates.first?.text, "你好")
        XCTAssertFalse(engine.currentView().visibleCandidates.contains { $0.aiBoosted })
    }

    func testNoChoiceRequestWhenOnlyOneWholeCandidate() {
        var requests = 0
        engine.onChoiceRequest = { _ in requests += 1 }
        type("womenchifan")
        scheduler.runAll()
        XCTAssertEqual(requests, 0)
    }

    // MARK: - 设置与本地模型

    func testLocalModelLoadAndUnload() throws {
        let missing = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("missing-\(UUID()).gguf")
        XCTAssertThrowsError(try engine.loadLocalModel(at: missing))
        XCTAssertFalse(engine.isLocalModelLoaded)

        let file = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("model-\(UUID()).gguf")
        try Data([1, 2, 3]).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        try engine.loadLocalModel(at: file)
        XCTAssertTrue(engine.isLocalModelLoaded)
        engine.unloadLocalModel()
        XCTAssertFalse(engine.isLocalModelLoaded)
    }

    func testApplySettingsAndContext() {
        var settings = IMESettings()
        settings.fuzzy.zZh = false
        engine.apply(settings)
        XCTAssertEqual(engine.settings, settings)
        engine.setContext(String(repeating: "长", count: 100))
        XCTAssertEqual(engine.context?.count, 64)
    }
}
