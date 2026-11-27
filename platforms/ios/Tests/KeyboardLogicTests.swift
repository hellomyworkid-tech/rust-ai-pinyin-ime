import UIKit
import XCTest

/// 上档键、上文跟踪、候选分页、输入框特征、音节栏
final class KeyboardLogicTests: XCTestCase {
    // MARK: - ShiftState

    func testShiftOnceThenOff() {
        var shift = ShiftState()
        shift.tap(at: 0)
        XCTAssertEqual(shift.mode, .once)
        XCTAssertTrue(shift.isUppercase)
        shift.didTypeLetter()
        XCTAssertEqual(shift.mode, .off)
    }

    func testDoubleTapLocksAndTapUnlocks() {
        var shift = ShiftState()
        shift.tap(at: 10)
        shift.tap(at: 10.2)
        XCTAssertEqual(shift.mode, .locked)
        shift.didTypeLetter()
        XCTAssertEqual(shift.mode, .locked, "锁定后打字不解除")
        shift.tap(at: 11)
        XCTAssertEqual(shift.mode, .off)
    }

    func testSlowSecondTapTurnsShiftOff() {
        var shift = ShiftState()
        shift.tap(at: 0)
        shift.tap(at: 1)
        XCTAssertEqual(shift.mode, .off)
    }

    func testTapAfterUnlockIsNotADoubleTap() {
        var shift = ShiftState()
        shift.tap(at: 0)
        shift.tap(at: 0.1)  // 锁定
        shift.tap(at: 0.2)  // 解除
        shift.tap(at: 0.3)  // 单次
        XCTAssertEqual(shift.mode, .once)
    }

    // MARK: - ContextTracker

    func testContextTrackerFollowsLocalEdits() {
        var tracker = ContextTracker()
        XCTAssertFalse(tracker.isFresh)
        tracker.didInsert("不该记")
        XCTAssertNil(tracker.text, "不可信时不记录")
        tracker.refresh(from: "你好")
        tracker.didInsert("，世界")
        XCTAssertEqual(tracker.text, "你好，世界")
        tracker.didDeleteBackward()
        XCTAssertEqual(tracker.text, "你好，世")
        tracker.invalidate()
        XCTAssertFalse(tracker.isFresh)
    }

    func testContextTrackerKeepsTail() {
        var tracker = ContextTracker()
        tracker.refresh(from: String(repeating: "甲", count: 100))
        XCTAssertEqual(tracker.text?.count, ContextTracker.maxLength)
        tracker.didInsert("乙")
        XCTAssertEqual(tracker.text?.count, ContextTracker.maxLength)
        XCTAssertEqual(tracker.text?.last, "乙")
        // 满长度时删字：不知道前面露出来的是什么，下次重新读
        tracker.didDeleteBackward()
        XCTAssertFalse(tracker.isFresh)
    }

    func testContextTrackerNilFromHost() {
        var tracker = ContextTracker()
        tracker.refresh(from: nil)
        XCTAssertTrue(tracker.isFresh)
        XCTAssertNil(tracker.text)
        tracker.didInsert("好")
        XCTAssertEqual(tracker.text, "好")
    }

    // MARK: - CandidatePager

    private func view(page: Int, size: Int, total: Int, prefix: String = "c") -> IMEView {
        var v = IMEView()
        v.visible = true
        v.pageIndex = page
        v.pageSize = size
        v.totalCandidates = total
        let start = page * size
        v.visibleCandidates = (start..<min(total, start + size)).map {
            IMECandidate(text: "\(prefix)\($0)", preedit: "", consumedLen: 0, tag: "", aiBoosted: false)
        }
        return v
    }

    func testPagerAccumulatesPages() {
        var pager = CandidatePager()
        pager.apply(view(page: 0, size: 2, total: 5))
        XCTAssertTrue(pager.hasMore)
        pager.apply(view(page: 1, size: 2, total: 5))
        pager.apply(view(page: 2, size: 2, total: 5))
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c1", "c2", "c3", "c4"])
        XCTAssertFalse(pager.hasMore)
        // 某页被引擎重排后重新给出：替换那一页及以后
        pager.apply(view(page: 1, size: 2, total: 5, prefix: "x"))
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c1", "x2", "x3"])
        // 第 0 页从头来过
        pager.apply(view(page: 0, size: 2, total: 5, prefix: "n"))
        XCTAssertEqual(pager.candidates.map(\.text), ["n0", "n1"])
        pager.apply(.idle)
        XCTAssertTrue(pager.candidates.isEmpty)
        XCTAssertFalse(pager.hasMore)
    }

    func testPagerIgnoresGap() {
        var pager = CandidatePager()
        pager.apply(view(page: 0, size: 2, total: 9))
        pager.apply(view(page: 3, size: 2, total: 9))
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c1"])
        XCTAssertFalse(pager.hasLoadedMore)
    }

    /// 「加载更多」接在后面；offset 对不上（列表已经换过）的结果丢弃
    func testPagerAppendsMoreCandidates() {
        var pager = CandidatePager()
        pager.apply(view(page: 0, size: 2, total: 5))
        XCTAssertFalse(pager.hasLoadedMore)
        let more = view(page: 1, size: 2, total: 5).visibleCandidates
        pager.append(more, at: 2)
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c1", "c2", "c3"])
        XCTAssertTrue(pager.hasLoadedMore)
        XCTAssertTrue(pager.hasMore)
        pager.append(more, at: 2)
        XCTAssertEqual(pager.candidates.count, 4, "下标对不上的结果不能接上去")
        pager.append(view(page: 2, size: 2, total: 5).visibleCandidates, at: 4)
        XCTAssertFalse(pager.hasMore)
        // 新的第 0 页从头来过
        pager.apply(view(page: 0, size: 2, total: 5, prefix: "n"))
        XCTAssertEqual(pager.candidates.map(\.text), ["n0", "n1"])
        XCTAssertFalse(pager.hasLoadedMore)
    }

    func testPagerWithFixture() throws {
        var pager = CandidatePager()
        pager.apply(try Fixtures.view("update_long_sentence"))
        XCTAssertEqual(pager.candidates.count, 30)
        XCTAssertEqual(pager.total, 35)
        XCTAssertTrue(pager.hasMore)
    }

    /// 显示不出来的候选不进列表；列表下标经 engineIndex(at:) 换算回引擎的全局下标，加载更多从已取到的个数接着取
    func testPagerSkipsUndisplayableCandidates() {
        let saved = CandidatePager.isDisplayable
        defer { CandidatePager.isDisplayable = saved }
        CandidatePager.isDisplayable = { !["c1", "c3"].contains($0.text) }
        var pager = CandidatePager()
        pager.apply(view(page: 0, size: 2, total: 5))
        XCTAssertEqual(pager.candidates.map(\.text), ["c0"])
        XCTAssertEqual(pager.fetchedCount, 2)
        pager.append(view(page: 1, size: 2, total: 5).visibleCandidates, at: 2)
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c2"])
        XCTAssertEqual(pager.engineIndex(at: 1), 2)
        pager.append(view(page: 2, size: 2, total: 5).visibleCandidates, at: 4)
        XCTAssertEqual(pager.candidates.map(\.text), ["c0", "c2", "c4"])
        XCTAssertEqual(pager.engineIndex(at: 2), 4)
        XCTAssertNil(pager.engineIndex(at: 3))
        XCTAssertFalse(pager.hasMore)
    }

    func testRenderabilityOfCommonAndRareCharacters() {
        XCTAssertTrue(CandidateRenderability.isRenderable("查询余额"))
        XCTAssertTrue(CandidateRenderability.isRenderable("YU yu 😀🌍"))
        // 私用区码位在系统字体里没有字形
        XCTAssertFalse(CandidateRenderability.isRenderable("\u{F0001}"))
        // chaxunyue 点 yu 时出现的方框字（CJK 扩展 C，U+2B6AA）
        XCTAssertFalse(CandidateRenderability.isRenderable("\u{2B6AA}"))
    }

    // MARK: - FieldTraits

    func testStartModes() {
        for type in [UIKeyboardType.URL, .emailAddress, .asciiCapable, .numberPad, .phonePad, .decimalPad,
                     .asciiCapableNumberPad, .numbersAndPunctuation] {
            XCTAssertTrue(FieldTraits(keyboardType: type).startsInEnglish, "\(type.rawValue)")
        }
        for type in [UIKeyboardType.default, .twitter, .webSearch, .namePhonePad] {
            XCTAssertFalse(FieldTraits(keyboardType: type).startsInEnglish, "\(type.rawValue)")
        }
        XCTAssertEqual(FieldTraits(keyboardType: .numberPad).startPage, .numbers)
        XCTAssertEqual(FieldTraits(keyboardType: .URL).startPage, .letters)
    }

    func testReturnKeyTitles() {
        XCTAssertEqual(FieldTraits(returnKeyType: .default).returnKeyTitle(isComposing: false), "换行")
        XCTAssertEqual(FieldTraits(returnKeyType: .send).returnKeyTitle(isComposing: false), "发送")
        XCTAssertEqual(FieldTraits(returnKeyType: .search).returnKeyTitle(isComposing: false), "搜索")
        XCTAssertEqual(FieldTraits(returnKeyType: .go).returnKeyTitle(isComposing: false), "前往")
        XCTAssertEqual(FieldTraits(returnKeyType: .done).returnKeyTitle(isComposing: false), "完成")
        XCTAssertEqual(FieldTraits(returnKeyType: .send).returnKeyTitle(isComposing: true), "确认")
        XCTAssertTrue(FieldTraits(returnKeyType: .send).returnKeyIsProminent)
        XCTAssertFalse(FieldTraits(returnKeyType: .default).returnKeyIsProminent)
    }

    // MARK: - CompositionModel

    func testCompositionFromFixture() throws {
        let model = CompositionModel(view: try Fixtures.view("update_syllable_cursor"))
        XCTAssertEqual(model.syllables.map(\.code), ["wo", "xiang", "qu", "gong", "yuan", "wan"])
        XCTAssertEqual(model.syllables.map(\.isActive), [false, false, true, false, false, false])
        XCTAssertFalse(model.syllables.contains { $0.isPinned })
        XCTAssertEqual(model.sentence, [
            IMESentencePart(text: "我想", pinned: false),
            IMESentencePart(text: "去", pinned: true),
            IMESentencePart(text: "公园玩", pinned: false),
        ])
    }

    func testEditedByteRangeHighlightsMultipleSyllablesAndCharacters() {
        var view = IMEView()
        view.visible = true
        view.composedFullSentence = "你好他们在这里"
        view.fullSentenceParts = [IMESentencePart(text: "你好他们在这里", pinned: false)]
        view.syllableSpans = [
            IMESpan(start: 0, end: 2, text: "你", codeKey: "ni"),
            IMESpan(start: 2, end: 5, text: "好", codeKey: "hao"),
            IMESpan(start: 5, end: 7, text: "他", codeKey: "ta"),
            IMESpan(start: 7, end: 10, text: "们", codeKey: "men"),
            IMESpan(start: 10, end: 13, text: "在", codeKey: "zai"),
            IMESpan(start: 13, end: 16, text: "这", codeKey: "zhe"),
            IMESpan(start: 16, end: 18, text: "里", codeKey: "li"),
        ]
        view.editedByteRange = IMEByteRange(start: 5, end: 10)
        view.cursorBytePos = 10

        let model = CompositionModel(view: view)
        XCTAssertEqual(model.syllables.map(\.isActive), [false, false, true, true, false, false, false])
        XCTAssertFalse(model.syllables.contains { $0.isPinned })
        XCTAssertEqual(model.sentence, [
            IMESentencePart(text: "你好", pinned: false),
            IMESentencePart(text: "他们", pinned: true),
            IMESentencePart(text: "在这里", pinned: false),
        ])
        XCTAssertEqual(model.scrollFocusIndex, 3)
    }

    func testPinnedSyllablesFollowSentenceParts() {
        let spans = ["我", "想", "去", "公", "园"].enumerated().map {
            IMESpan(start: $0.offset, end: $0.offset + 1, text: $0.element, codeKey: "s\($0.offset)")
        }
        let parts = [IMESentencePart(text: "我想", pinned: true), IMESentencePart(text: "去", pinned: false),
                     IMESentencePart(text: "公园", pinned: true)]
        XCTAssertEqual(CompositionModel.pinnedSyllables(spans: spans, parts: parts), [true, true, false, true, true])
    }

    func testPinnedWordOnFirstSyllable() {
        let spans = [IMESpan(start: 0, end: 2, text: "你好", codeKey: "ni"),
                     IMESpan(start: 2, end: 5, text: "", codeKey: "hao"),
                     IMESpan(start: 5, end: 7, text: "吗", codeKey: "ma")]
        let parts = [IMESentencePart(text: "你好", pinned: true), IMESentencePart(text: "吗", pinned: false)]
        XCTAssertEqual(CompositionModel.pinnedSyllables(spans: spans, parts: parts), [true, true, false])
    }

    func testCompositionFromMockEngineAfterPin() throws {
        let engine = MockEngine(schedule: { _, _ in })
        for c in "nihao" { _ = engine.handle(.character(c)) }
        let index = try XCTUnwrap(engine.currentView().visibleCandidates.firstIndex { $0.text == "你" })
        _ = engine.selectCandidate(at: index)
        let model = CompositionModel(view: engine.currentView())
        XCTAssertEqual(model.syllables.map(\.isPinned), [true, false])
        XCTAssertEqual(model.sentence.first, IMESentencePart(text: "你", pinned: true))
    }

    func testScrollFocusTracksMidSentenceCursor() {
        var view = IMEView()
        view.visible = true
        view.syllableSpans = [
            IMESpan(start: 0, end: 5, text: "中", codeKey: "zhong"),
            IMESpan(start: 5, end: 8, text: "国", codeKey: "guo"),
            IMESpan(start: 8, end: 11, text: "没", codeKey: "mei"),
            IMESpan(start: 11, end: 14, text: "有", codeKey: "you"),
        ]
        view.activeSyllableIndex = nil
        view.cursorBytePos = 11 // 刚在句中把 nei 改写成 mei，光标停在 byte 11
        XCTAssertEqual(CompositionModel(view: view).scrollFocusIndex, 2)

        view.cursorBytePos = 14 // 光标在句尾正常输入
        XCTAssertNil(CompositionModel(view: view).scrollFocusIndex)
    }

    // MARK: - Pronunciation Hint Preservation & Grid Packing

    func testGridPackingSpansWidePronunciationHintAcrossColumns() {
        // 展开网格 6 列（单列宽 65pt）：「混沌」（~64pt）占 1 列，「馄饨 馄 (hun) 饨 (tun)」（~162pt）自动跨 3 列
        let spans = GridPacking.spans(neededWidths: [64, 162, 44, 44], unit: 65, columns: 6)
        XCTAssertEqual(spans, [1, 3, 1, 1])

        let hinted = IMECandidate(
            text: "馄饨",
            preedit: "hun'dun",
            consumedLen: 6,
            tag: "[错音]",
            aiBoosted: false,
            pronunciationHint: "馄 (hun) 饨 (tun)"
        )
        XCTAssertTrue(CandidateRenderability.isRenderable(hinted.text))
        XCTAssertEqual(hinted.pronunciationHint, "馄 (hun) 饨 (tun)")
    }
}
