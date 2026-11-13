import UIKit
import XCTest

/// 键位数据和排布计算
final class KeyboardLayoutTests: XCTestCase {
    private func letters(_ row: [KeySpec]) -> String {
        String(row.compactMap {
            if case .letter(let c) = $0.action { return c }
            return nil
        })
    }

    func testLetterRows() {
        let rows = KeyboardLayout.rows(for: .letters, chineseMode: true, showsGlobe: false)
        XCTAssertEqual(rows.count, 4)
        XCTAssertEqual(letters(rows[0]), "qwertyuiop")
        XCTAssertEqual(letters(rows[1]), "asdfghjkl")
        XCTAssertEqual(letters(rows[2]), "zxcvbnm")
        XCTAssertEqual(rows[2].first?.action, .shift)
        XCTAssertEqual(rows[2].last?.action, .backspace)
        XCTAssertEqual(rows[3].map(\.action), [.page(.numbers), .toggleLanguage, .symbol(","), .space, .symbol("."), .returnKey])
    }

    func testGlobeOnlyWhenNeeded() {
        for page in [KeyboardPage.letters, .numbers, .symbols] {
            let without = KeyboardLayout.rows(for: page, chineseMode: true, showsGlobe: false).flatMap { $0 }
            let with = KeyboardLayout.rows(for: page, chineseMode: true, showsGlobe: true).flatMap { $0 }
            XCTAssertFalse(without.contains { $0.action == .globe })
            XCTAssertEqual(with.filter { $0.action == .globe }.count, 1)
        }
    }

    func testNumberAndSymbolPages() {
        let numbers = KeyboardLayout.rows(for: .numbers, chineseMode: true, showsGlobe: false)
        XCTAssertEqual(numbers[0].map(\.action), "1234567890".map { .symbol(String($0)) })
        XCTAssertEqual(numbers[2].first?.action, .page(.symbols))
        XCTAssertTrue(numbers[2].contains { $0.action == .symbol("\\") }, "中文数字页有顿号")
        XCTAssertEqual(numbers[3].first?.action, .page(.letters))
        let english = KeyboardLayout.rows(for: .numbers, chineseMode: false, showsGlobe: false)
        XCTAssertTrue(english[2].contains { $0.action == .symbol("'") })

        let symbols = KeyboardLayout.rows(for: .symbols, chineseMode: true, showsGlobe: false)
        XCTAssertEqual(symbols[2].first?.action, .page(.numbers))
        XCTAssertEqual(symbols.flatMap { $0 }.filter { $0.action == .backspace }.count, 1)
    }

    func testEveryPageHasSpaceAndReturn() {
        for page in [KeyboardPage.letters, .numbers, .symbols] {
            let keys = KeyboardLayout.rows(for: page, chineseMode: true, showsGlobe: true).flatMap { $0 }
            XCTAssertTrue(keys.contains { $0.action == .space })
            XCTAssertTrue(keys.contains { $0.action == .returnKey })
            XCTAssertTrue(keys.contains { $0.action == .backspace })
        }
    }

    func testGeometryFitsAndDoesNotOverlap() {
        for (width, height, metrics) in [(CGFloat(402), CGFloat(216), KeyboardGeometry.Metrics.portrait),
                                         (CGFloat(320), CGFloat(216), .portrait),
                                         (CGFloat(874), CGFloat(160), .landscape)] {
            for page in [KeyboardPage.letters, .numbers, .symbols] {
                for globe in [false, true] {
                    let rows = KeyboardLayout.rows(for: page, chineseMode: true, showsGlobe: globe)
                    let bounds = CGRect(x: 0, y: 0, width: width, height: height)
                    let frames = KeyboardGeometry.layout(rows: rows, in: bounds, metrics: metrics)
                    XCTAssertEqual(frames.count, rows.flatMap { $0 }.count)
                    for f in frames {
                        XCTAssertTrue(bounds.insetBy(dx: -0.5, dy: -0.5).contains(f.frame), "\(page) \(width) \(f)")
                        XCTAssertGreaterThan(f.frame.width, 20)
                        XCTAssertTrue(f.touchFrame.contains(f.frame))
                    }
                    for row in 0..<rows.count {
                        let inRow = frames.filter { $0.row == row }
                        for (a, b) in zip(inRow, inRow.dropFirst()) {
                            XCTAssertLessThanOrEqual(a.frame.maxX, b.frame.minX + 0.01, "键重叠")
                            XCTAssertEqual(a.touchFrame.maxX, b.touchFrame.minX, accuracy: 0.01, "触摸区要连续")
                        }
                    }
                }
            }
        }
    }

    func testSecondLetterRowIsCentered() {
        let rows = KeyboardLayout.rows(for: .letters, chineseMode: true, showsGlobe: false)
        let frames = KeyboardGeometry.layout(rows: rows, in: CGRect(x: 0, y: 0, width: 402, height: 216),
                                             metrics: .portrait)
        let second = frames.filter { $0.row == 1 }
        let left = second.first!.frame.minX
        let right = 402 - second.last!.frame.maxX
        XCTAssertEqual(left, right, accuracy: 0.5)
        // 第三行的 z 和 m 也居中，且上档、退格同宽
        let third = frames.filter { $0.row == 2 }
        XCTAssertEqual(third.first!.frame.width, third.last!.frame.width, accuracy: 0.01)
    }

    func testGridPackingFillsEveryRow() {
        // 每格 50：宽 40 占 1 格，宽 120 占 3 格
        let spans = GridPacking.spans(neededWidths: [40, 120, 40, 40, 120, 40, 400], unit: 50, columns: 6)
        XCTAssertEqual(spans, [1, 3, 1, 1, 3, 3, 6])
        var used = 0
        for span in spans {
            used += span
            if used == 6 { used = 0 }
            XCTAssertLessThanOrEqual(used, 6)
        }
        XCTAssertEqual(used, 0, "最后一行也补满")
        XCTAssertEqual(GridPacking.spans(neededWidths: [], unit: 50, columns: 6), [])
    }

    func testFunctionKeyFlags() {
        XCTAssertTrue(KeySpec(.letter("a")).showsPreview)
        XCTAssertTrue(KeySpec(.symbol(",")).showsPreview)
        XCTAssertFalse(KeySpec(.space).showsPreview)
        XCTAssertTrue(KeySpec(.shift).isFunctionKey)
        XCTAssertFalse(KeySpec(.space).isFunctionKey)
    }
}
