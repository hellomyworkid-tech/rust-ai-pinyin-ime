import XCTest

/// 每个 fixture 都要能解码，且解码结果和 Rust 侧的含义一致
final class IMEEngineJSONTests: XCTestCase {
    func testEveryFixtureDecodes() throws {
        for name in Fixtures.all {
            XCTAssertNoThrow(try Fixtures.result(name), name)
        }
    }

    func testUpdateNihao() throws {
        let view = try Fixtures.view("update_nihao")
        XCTAssertTrue(view.visible)
        XCTAssertEqual(view.preeditText, "ni'hao")
        XCTAssertEqual(view.pageIndex, 0)
        XCTAssertEqual(view.pageSize, 30)
        XCTAssertEqual(view.totalCandidates, 30)
        XCTAssertEqual(view.visibleCandidates.count, 30)
        // 「你好」后面跟着表情候选（iOS 默认打开）
        XCTAssertEqual(view.visibleCandidates[1].text, "👋")
        XCTAssertEqual(view.visibleCandidates[1].tag, "[表情]")
        XCTAssertEqual(view.selectedCandidateIndex, 0)
        XCTAssertEqual(view.composedFullSentence, "你好")
        XCTAssertEqual(view.fullSentenceParts, [IMESentencePart(text: "你好", pinned: false)])
        XCTAssertEqual(view.syllableSpans, [
            IMESpan(start: 0, end: 2, text: "你", codeKey: "ni"),
            IMESpan(start: 2, end: 5, text: "好", codeKey: "hao"),
        ])
        XCTAssertEqual(view.cursorBytePos, 5)
        XCTAssertNil(view.activeSyllableIndex)
        XCTAssertNil(view.ghostTextPreview)
        XCTAssertEqual(view.aiSemanticHint, "通用语境")

        let first = try XCTUnwrap(view.visibleCandidates.first)
        XCTAssertEqual(first, IMECandidate(text: "你好", preedit: "ni'hao", consumedLen: 5, tag: "[词库]", aiBoosted: false))
        // 表情插在「你好」后面，前缀候选「你」顺延一位
        XCTAssertEqual(view.visibleCandidates[3].text, "你")
        XCTAssertEqual(view.visibleCandidates[3].consumedLen, 2)
        XCTAssertEqual(view.visibleCandidates[3].tag, "[前缀选词]")
    }

    func testUpdateLongSentence() throws {
        let view = try Fixtures.view("update_long_sentence")
        XCTAssertEqual(view.preeditText, "wo'xiang'qu'gong'yuan'wan")
        XCTAssertEqual(view.syllableSpans.map(\.codeKey), ["wo", "xiang", "qu", "gong", "yuan", "wan"])
        XCTAssertEqual(view.syllableSpans.map(\.text).joined(), "我想去公园完")
        XCTAssertEqual(view.totalCandidates, 35)
        XCTAssertEqual(view.visibleCandidates.count, 30)
        XCTAssertEqual(view.visibleCandidates.first?.tag, "[整句]")
        XCTAssertEqual(view.visibleCandidates.first?.consumedLen, 20)
        XCTAssertEqual(view.composedFullSentence, "我想去公园完")
        XCTAssertNil(view.activeSyllableIndex)
    }

    func testUpdateSyllableCursor() throws {
        let view = try Fixtures.view("update_syllable_cursor")
        XCTAssertEqual(view.activeSyllableIndex, 2)
        XCTAssertEqual(view.cursorBytePos, 7)
        XCTAssertEqual(view.syllableSpans[2].codeKey, "qu")
        XCTAssertEqual(view.visibleCandidates.first?.text, "去")
        XCTAssertEqual(view.visibleCandidates.first?.tag, "(qu)")
        // 单音节选词时 consumedLen 是这个音节的长度
        XCTAssertEqual(view.visibleCandidates.first?.consumedLen, 2)
    }

    func testCommitWithPunctuationShowsCommaMergedIntoPinyin() throws {
        let view = try Fixtures.view("commit_with_punctuation")
        XCTAssertEqual(view.preeditText, "shi'yan',")
        XCTAssertEqual(view.syllableSpans.last, IMESpan(start: 6, end: 7, text: ",", codeKey: ","))
        XCTAssertEqual(view.visibleCandidates.first?.text, "实验 ,")
        XCTAssertEqual(view.visibleCandidates.first?.tag, "[中英混输]")
        XCTAssertEqual(view.totalCandidates, 114)
    }

    func testCommitNihaoSpace() throws {
        guard case .commit(let text, let view) = try Fixtures.result("commit_nihao_space") else {
            return XCTFail("应为 commit")
        }
        XCTAssertEqual(text, "你好")
        XCTAssertEqual(view, .idle)
    }

    func testEscapeReturnsIdleView() throws {
        let view = try Fixtures.view("escape")
        XCTAssertFalse(view.visible)
        XCTAssertTrue(view.visibleCandidates.isEmpty)
        XCTAssertTrue(view.syllableSpans.isEmpty)
        XCTAssertEqual(view, .idle)
    }

    func testIdleSpacePassesThrough() throws {
        guard case .passThrough = try Fixtures.result("idle_space") else {
            return XCTFail("应为 passThrough")
        }
    }

    func testUnknownActionThrows() {
        let data = Data(#"{"action": "explode"}"#.utf8)
        XCTAssertThrowsError(try IMEEngineJSON.decodeResult(data)) { error in
            XCTAssertEqual(error as? IMEEngineJSON.DecodingFailure, .unknownAction("explode"))
        }
    }

    func testUpdateWithoutViewThrows() {
        let data = Data(#"{"action": "update"}"#.utf8)
        XCTAssertThrowsError(try IMEEngineJSON.decodeResult(data)) { error in
            XCTAssertEqual(error as? IMEEngineJSON.DecodingFailure, .missingField("view"))
        }
    }

    func testDecodeViewDirectly() throws {
        let data = Data(#"{"visible": true, "preedit_text": "a", "page_size": 30, "total_candidates": 0, "visible_candidates": [], "full_sentence_parts": [["啊", true]], "syllable_spans": [], "page_index": 0, "selected_candidate_index": 0, "composed_full_sentence": "啊", "cursor_byte_pos": 1, "active_syllable_index": null, "ai_semantic_hint": "", "ghost_text_preview": null}"#.utf8)
        let view = try IMEEngineJSON.decodeView(data)
        XCTAssertEqual(view.fullSentenceParts, [IMESentencePart(text: "啊", pinned: true)])
        XCTAssertEqual(view.cursorBytePos, 1)
    }
}
