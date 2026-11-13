import XCTest

final class PunctuationMapTests: XCTestCase {
    private func chinese(_ ascii: String, after context: String? = nil) -> String {
        PunctuationMap.output(for: ascii, chineseMode: true, contextBefore: context)
    }

    func testChineseModeMapsToFullWidth() {
        let expected: [String: String] = [
            ",": "，", ".": "。", "?": "？", "!": "！", ":": "：", ";": "；",
            "(": "（", ")": "）", "[": "【", "]": "】", "<": "《", ">": "》",
            "\\": "、", "^": "……", "_": "——", "~": "～", "$": "￥", "`": "·",
        ]
        for (ascii, full) in expected {
            XCTAssertEqual(chinese(ascii), full, ascii)
        }
    }

    func testDigitsAndPlainSymbolsAreUnchanged() {
        for s in ["0", "7", "#", "%", "+", "=", "@", "&", "*", "/", "-", "|"] {
            XCTAssertEqual(chinese(s), s)
        }
    }

    func testEnglishModeIsAscii() {
        for s in [",", ".", "?", "\"", "'", "\\", "^"] {
            XCTAssertEqual(PunctuationMap.output(for: s, chineseMode: false, contextBefore: nil), s)
            XCTAssertEqual(PunctuationMap.label(for: s, chineseMode: false), s)
        }
    }

    func testQuotesPairWithContext() {
        XCTAssertEqual(chinese("\""), "“")
        XCTAssertEqual(chinese("\"", after: "他说：“你好"), "”")
        XCTAssertEqual(chinese("\"", after: "“你好”，然后"), "“")
        XCTAssertEqual(chinese("'", after: "‘甲"), "’")
        XCTAssertEqual(chinese("'", after: "“甲"), "‘", "单双引号各自配对")
    }

    func testLabels() {
        XCTAssertEqual(PunctuationMap.label(for: ",", chineseMode: true), "，")
        XCTAssertEqual(PunctuationMap.label(for: "\"", chineseMode: true), "“”")
        XCTAssertEqual(PunctuationMap.label(for: "1", chineseMode: true), "1")
    }
}
