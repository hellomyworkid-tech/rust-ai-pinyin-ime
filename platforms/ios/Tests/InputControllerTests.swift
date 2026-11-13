import XCTest

/// 键盘按键规则，对着记录型的假引擎和假输入框测
final class InputControllerTests: XCTestCase {
    private var engine: RecordingEngine!
    private var output: RecordingTextOutput!
    private var controller: InputController!
    private var changes = 0

    override func setUp() {
        super.setUp()
        engine = RecordingEngine()
        output = RecordingTextOutput()
        let engine = self.engine!
        controller = InputController(makeEngine: { engine })
        controller.output = output
        controller.onChange = { [weak self] in self?.changes += 1 }
    }

    private func type(_ letters: String) {
        letters.forEach { controller.typeLetter($0) }
    }

    // MARK: - 中文模式

    func testLettersGoToEngineAndCommitIsInserted() {
        type("ni")
        XCTAssertEqual(engine.handledKeys, [.character("n"), .character("i")])
        XCTAssertTrue(output.operations.isEmpty, "输入中途不应写入宿主")
        XCTAssertTrue(controller.isComposing)
        XCTAssertEqual(controller.pager.candidates.map(\.text), ["c0", "c1", "c2"])

        controller.space()
        XCTAssertEqual(output.operations, [.insert("<ni>")])
        XCTAssertFalse(controller.isComposing)
        XCTAssertEqual(controller.view, .idle)
        XCTAssertTrue(controller.pager.candidates.isEmpty)
    }

    func testEngineIsCreatedLazilyOnFirstLetter() {
        var created = 0
        controller.onEngineCreated = { _ in created += 1 }
        XCTAssertNil(controller.engine)
        type("a")
        XCTAssertNotNil(controller.engine)
        XCTAssertEqual(created, 1)
        type("b")
        XCTAssertEqual(created, 1)
    }

    func testIdleKeysPassThroughToDefaultAction() {
        controller.ensureEngine()
        controller.space()
        controller.returnKey()
        controller.backspace()
        XCTAssertEqual(engine.handledKeys, [.space, .enter, .backspace])
        XCTAssertEqual(output.operations, [.insert(" "), .insert("\n"), .delete])
    }

    func testIdleKeysWorkBeforeEngineExists() {
        controller.space()
        controller.backspace()
        XCTAssertNil(controller.engine)
        XCTAssertEqual(output.operations, [.insert(" "), .delete])
    }

    func testBackspaceWhileComposingGoesToEngine() {
        type("ni")
        controller.backspace()
        XCTAssertEqual(engine.handledKeys.last, .backspace)
        XCTAssertTrue(output.operations.isEmpty)
        XCTAssertEqual(controller.view.preeditText, "n")
    }

    func testReturnWhileComposingCommitsRawPinyin() {
        type("ni")
        controller.returnKey()
        XCTAssertEqual(output.operations, [.insert("ni")])
    }

    func testSelectCandidateCommits() {
        type("ni")
        controller.selectCandidate(at: 2)
        XCTAssertEqual(engine.calls.last, .select(2))
        XCTAssertEqual(output.inserted, ["c2"])
    }

    func testTapSyllableGoesToEngine() {
        type("nihao")
        controller.tapSyllable(at: 1)
        XCTAssertEqual(engine.calls.last, .tapSyllable(1))
        XCTAssertEqual(controller.view.activeSyllableIndex, 1)
    }

    // MARK: - 标点与数字

    func testPunctuationWhileComposingCommitsSentenceFirst() {
        type("ni")
        controller.typeSymbol(",")
        XCTAssertEqual(output.inserted, ["[ni]", "，"])
        XCTAssertEqual(engine.count(.commitFullSentence), 1)
        XCTAssertFalse(engine.handledKeys.contains(.character(",")), "标点不能送进引擎")
        XCTAssertFalse(controller.isComposing)
    }

    func testDigitWhileComposingCommitsSentenceFirst() {
        type("hao")
        controller.typeSymbol("1")
        XCTAssertEqual(output.inserted, ["[hao]", "1"])
    }

    func testIdlePunctuationIsChineseInChineseModeAndAsciiInEnglishMode() {
        controller.typeSymbol(".")
        controller.toggleLanguage()
        XCTAssertFalse(controller.isChineseMode)
        controller.typeSymbol(".")
        XCTAssertEqual(output.inserted, ["。", "."])
        XCTAssertEqual(engine.count(.commitFullSentence), 0, "空闲时不必调用 commitFullSentence")
    }

    func testChineseQuotesPairUsingLocalContext() {
        output.hostText = "他说"
        controller.typeSymbol("\"")
        type("ni")
        controller.space()
        controller.typeSymbol("\"")
        XCTAssertEqual(output.inserted, ["“", "<ni>", "”"])
    }

    // MARK: - 上文

    func testContextIsReadOncePerCompositionAndTrackedLocally() {
        output.hostText = "今天天气"
        type("ni")
        XCTAssertEqual(engine.contexts, ["今天天气"])
        XCTAssertEqual(output.contextReads, 1, "只在输入开始时读一次")
        controller.space()
        type("h")
        XCTAssertEqual(output.contextReads, 1, "上屏之后用本地副本，不再问宿主")
        XCTAssertEqual(engine.contexts, ["今天天气", "今天天气<ni>"])
    }

    func testHostChangeInvalidatesLocalContext() {
        output.hostText = "甲"
        type("a")
        controller.space()
        output.hostText = "乙"
        controller.hostTextDidChange()
        type("b")
        XCTAssertEqual(output.contextReads, 2)
        XCTAssertEqual(engine.contexts.last, "乙")
    }

    func testContextIsTrimmedTo64Characters() {
        output.hostText = String(repeating: "一二三四五", count: 30)
        type("a")
        let context = engine.contexts.first ?? nil
        XCTAssertEqual(context?.count, 64)
        XCTAssertEqual(context, String(output.hostText.suffix(64)))
    }

    /// 宿主给 nil（空输入框）就是没有上文：传 ""，不能传 nil 让引擎沿用之前输入框里的上屏历史（SW-3）
    func testNilHostContextIsPassedAsEmptyText() {
        output.contextUnavailable = true
        type("a")
        XCTAssertEqual(engine.contexts, [""])
        controller.space()
        type("b")
        XCTAssertEqual(engine.contexts, ["", "<a>"], "之后用本地副本")
    }

    func testBackspaceUpdatesLocalContext() {
        output.hostText = "你好吗"
        type("a")
        controller.space()  // 上文：你好吗<a>
        controller.backspace()
        type("b")
        XCTAssertEqual(engine.contexts.last, "你好吗<a")
        XCTAssertEqual(output.contextReads, 1)
    }

    // MARK: - 中英文与输入框

    func testEnglishModeInsertsLettersDirectly() {
        controller.toggleLanguage()
        type("Hi")
        controller.space()
        XCTAssertEqual(output.inserted, ["H", "i", " "])
        XCTAssertFalse(engine.handledKeys.contains(.character("H")))
    }

    func testToggleWhileComposingCommitsRawLetters() {
        type("ni")
        controller.toggleLanguage()
        XCTAssertEqual(output.inserted, ["ni"])
        XCTAssertFalse(controller.isChineseMode)
    }

    func testAsciiFieldStartsInEnglishAndNormalFieldRestoresChinese() {
        controller.beginField(FieldTraits(keyboardType: .URL))
        XCTAssertFalse(controller.isChineseMode)
        type("a")
        XCTAssertEqual(output.inserted, ["a"])
        // 引擎此时才建，也应是英文
        controller.ensureEngine()
        XCTAssertTrue(engine.isEnglishMode)

        controller.beginField(FieldTraits(keyboardType: .default))
        XCTAssertTrue(controller.isChineseMode)
    }

    func testUserChoiceOfEnglishSurvivesFieldChange() {
        controller.ensureEngine()
        controller.toggleLanguage()
        controller.beginField(FieldTraits(keyboardType: .default))
        XCTAssertFalse(controller.isChineseMode)
    }

    func testTemporaryToggleInAsciiFieldDoesNotChangePreference() {
        controller.ensureEngine()
        controller.beginField(FieldTraits(keyboardType: .emailAddress))
        controller.toggleLanguage()
        XCTAssertTrue(controller.isChineseMode)
        controller.beginField(FieldTraits(keyboardType: .emailAddress))
        XCTAssertFalse(controller.isChineseMode)
        controller.beginField(FieldTraits(keyboardType: .default))
        XCTAssertTrue(controller.isChineseMode)
    }

    func testFieldChangeResetsComposition() {
        type("ni")
        controller.beginField(FieldTraits())
        XCTAssertTrue(engine.calls.contains(.reset))
        XCTAssertFalse(controller.isComposing)
        XCTAssertEqual(controller.view, .idle)
        XCTAssertTrue(output.operations.isEmpty, "换输入框时丢弃拼音，不上屏")
    }

    func testResetComposition() {
        type("ni")
        controller.resetComposition()
        XCTAssertFalse(controller.isComposing)
        XCTAssertTrue(controller.pager.candidates.isEmpty)
    }

    // MARK: - 候选翻页

    func testLoadMoreCandidatesAppendsPagesWithoutPagingTheEngine() {
        engine.candidateCount = 25
        engine.pageSize = 10
        type("a")
        XCTAssertEqual(controller.pager.candidates.count, 10)
        let before = changes
        XCTAssertTrue(controller.loadMoreCandidates())
        XCTAssertEqual(controller.pager.candidates.count, 20)
        XCTAssertGreaterThan(changes, before, "加载了更多要重画")
        XCTAssertTrue(controller.loadMoreCandidates())
        XCTAssertEqual(controller.pager.candidates.map(\.text), (0..<25).map { "c\($0)" })
        XCTAssertFalse(controller.loadMoreCandidates())
        XCTAssertEqual(engine.calls.filter { if case .candidates = $0 { return true } else { return false } },
                       [.candidates(10, 10), .candidates(20, 10)])
        XCTAssertFalse(engine.handledKeys.contains(.pageDown), "不翻引擎的页（SW-1）")
        XCTAssertEqual(controller.view.pageIndex, 0)
    }

    func testSpaceAfterLoadingMoreGoesToTheEngineOnPageZero() {
        engine.candidateCount = 25
        engine.pageSize = 10
        type("a")
        controller.loadMoreCandidates()
        controller.space()
        XCTAssertEqual(engine.handledKeys.last, .space)
        XCTAssertEqual(output.inserted, ["<a>"])
    }

    // MARK: - AI 异步结果

    func testAsyncUpdateRefreshesImmediately() {
        type("ni")
        engine.fireAsyncUpdate()
        XCTAssertEqual(engine.count(.refreshAfterAI), 1)
    }

    func testAsyncUpdateWaitsWhileTouchingCandidate() {
        type("ni")
        controller.isTouchingCandidate = true
        engine.fireAsyncUpdate()
        XCTAssertEqual(engine.count(.refreshAfterAI), 0, "手指按着候选时不能刷新")
        controller.isTouchingCandidate = false
        XCTAssertEqual(engine.count(.refreshAfterAI), 1)
        controller.isTouchingCandidate = true
        controller.isTouchingCandidate = false
        XCTAssertEqual(engine.count(.refreshAfterAI), 1, "没有新结果就不刷新")
    }

    func testAsyncUpdateWhenIdleIsIgnored() {
        controller.ensureEngine()
        engine.fireAsyncUpdate()
        XCTAssertEqual(engine.count(.refreshAfterAI), 0)
    }

    /// 已经往后加载了候选：AI 结果先压着，不把用户正在看的列表抽走；下一次视图变化时补上（SW-2）
    func testAsyncUpdateWaitsWhileBrowsingMoreCandidates() {
        engine.candidateCount = 25
        engine.pageSize = 10
        type("ab")
        controller.loadMoreCandidates()
        engine.fireAsyncUpdate()
        XCTAssertEqual(engine.count(.refreshAfterAI), 0, "浏览中不刷新")
        XCTAssertEqual(controller.pager.candidates.count, 20, "加载的候选还在")

        // 下一次视图变化（这里是退格）回到第一页，压着的结果随即补上
        controller.backspace()
        XCTAssertEqual(engine.count(.refreshAfterAI), 1)
        XCTAssertEqual(controller.pager.candidates.count, 10)
        XCTAssertEqual(controller.view.pageIndex, 0)
        controller.backspace()
        XCTAssertEqual(engine.count(.refreshAfterAI), 1, "补过一次就不再刷新")
    }

    func testAsyncUpdateWaitsWhileGridIsExpanded() {
        type("ni")
        controller.isGridExpanded = true
        engine.fireAsyncUpdate()
        XCTAssertEqual(engine.count(.refreshAfterAI), 0, "网格开着不刷新")
        controller.isGridExpanded = false
        XCTAssertEqual(engine.count(.refreshAfterAI), 1, "收起后补上")
    }

    func testHeldAsyncUpdateIsDroppedWhenCompositionEnds() {
        type("ni")
        controller.isGridExpanded = true
        engine.fireAsyncUpdate()
        controller.selectCandidate(at: 0)
        controller.isGridExpanded = false
        XCTAssertEqual(engine.count(.refreshAfterAI), 0, "输入结束了，压着的结果没用了")
        type("a")
        XCTAssertEqual(engine.count(.refreshAfterAI), 0)
    }

    /// SW-5：键盘再次出现时让已有的引擎检查个人词库；还没有引擎时什么都不做（也不为此建引擎）
    func testKeyboardAppearanceReloadsTheUserDictionary() {
        controller.keyboardWillAppear()
        XCTAssertNil(controller.engine)
        controller.ensureEngine()
        controller.keyboardWillAppear()
        XCTAssertEqual(engine.count(.reloadUserDictionary), 1)
    }

    func testSettingsAreAppliedToEngine() {
        controller.apply(IMESettings())
        controller.ensureEngine()
        controller.apply(IMESettings())
        XCTAssertEqual(engine.count(.apply), 2)
    }
}
