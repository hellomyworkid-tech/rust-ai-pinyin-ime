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
        XCTAssertEqual(rows[0].map(\.swipeUpAction), "1234567890".map { .symbol(String($0)) })
        XCTAssertEqual(letters(rows[1]), "asdfghjkl")
        XCTAssertEqual(rows[1].map(\.swipeUpAction), [
            .symbol("`"), .symbol("/"), .symbol(":"), .symbol(";"),
            .symbol("("), .symbol(")"), .symbol("~"), .symbol("“"), .symbol("”"),
        ])
        XCTAssertEqual(letters(rows[2]), "zxcvbnm")
        XCTAssertEqual(rows[2].first?.action, .shift)
        XCTAssertEqual(rows[2].last?.action, .backspace)
        XCTAssertEqual(Array(rows[2].dropFirst().dropLast()).map(\.swipeUpAction), [
            .symbol("@"), .literal("."), .symbol("#"), .symbol("\\"),
            .symbol("?"), .symbol("!"), .symbol("..."),
        ])
        XCTAssertEqual(rows[3].map(\.action), [.page(.numbers), .symbol(","), .space, .toggleLanguage, .returnKey])
        XCTAssertEqual(rows[3][1].swipeUpAction, .symbol("."))
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
        XCTAssertTrue(KeySpec(.literal(".")).showsPreview)
        XCTAssertFalse(KeySpec(.space).showsPreview)
        XCTAssertTrue(KeySpec(.shift).isFunctionKey)
        XCTAssertFalse(KeySpec(.space).isFunctionKey)
        XCTAssertFalse(KeySpec(.toggleLanguage).isFunctionKey)
    }

    func testSwipeUpGesturesAndSlideDisambiguation() {
        let bounds = CGRect(x: 0, y: 0, width: 402, height: 216)
        let rows = KeyboardLayout.rows(for: .letters, chineseMode: true, showsGlobe: false)
        var metrics = KeyboardGeometry.Metrics.portrait
        metrics.rowGap = max(4, (bounds.height / CGFloat(rows.count) * 0.2).rounded())
        let frames = KeyboardGeometry.layout(rows: rows, in: bounds, metrics: metrics)
        let specs = rows.flatMap { $0 }
        var tracker = KeyboardTouchTracker(specs: specs, frames: frames)

        func center(of target: KeyAction) -> CGPoint {
            let idx = specs.firstIndex { $0.action == target }!
            return CGPoint(x: frames[idx].frame.midX, y: frames[idx].frame.midY)
        }

        // 1. 普通点按 q → .letter("q")；上划 q → 气泡先切到 .symbol("1")，松手提交 .symbol("1")
        let q = center(of: .letter("q"))
        _ = tracker.begin(id: 1, at: q)
        XCTAssertEqual(tracker.end(id: 1).triggered, [.letter("q")])

        _ = tracker.begin(id: 2, at: q)
        let movedQ = tracker.move(id: 2, to: CGPoint(x: q.x, y: q.y - 24))
        XCTAssertEqual(movedQ.preview?.action, .symbol("1"))
        XCTAssertEqual(tracker.end(id: 2).triggered, [.symbol("1")])

        // 2. 从第二行 s 直向上滑进第一行 w 的区域，应触发 s 的上划 "/" 而不误跳到 w
        let s = center(of: .letter("s"))
        let w = center(of: .letter("w"))
        _ = tracker.begin(id: 3, at: s)
        _ = tracker.move(id: 3, to: CGPoint(x: s.x, y: s.y - 10))
        _ = tracker.move(id: 3, to: CGPoint(x: s.x, y: w.y))
        XCTAssertEqual(tracker.end(id: 3).triggered, [.symbol("/")])

        // 3. 上划 b 后未抬手又滑回键帽，取消上划回到字母 b
        let b = center(of: .letter("b"))
        _ = tracker.begin(id: 4, at: b)
        XCTAssertEqual(tracker.move(id: 4, to: CGPoint(x: b.x, y: b.y - 26)).preview?.action, .symbol("?"))
        XCTAssertEqual(tracker.move(id: 4, to: b).preview?.action, .letter("b"))
        XCTAssertEqual(tracker.end(id: 4).triggered, [.letter("b")])

        // 4. 横向从 f 滑到 g，正常换键为 g，不误触上划标点
        let f = center(of: .letter("f"))
        let g = center(of: .letter("g"))
        _ = tracker.begin(id: 5, at: f)
        XCTAssertEqual(tracker.move(id: 5, to: g).preview?.action, .letter("g"))
        XCTAssertEqual(tracker.end(id: 5).triggered, [.letter("g")])

        // 5. 底排逗号键点按输 ","，上划输 "."；x 上划输原样 "."
        let comma = center(of: .symbol(","))
        let x = center(of: .letter("x"))
        _ = tracker.begin(id: 6, at: comma)
        XCTAssertEqual(tracker.end(id: 6).triggered, [.symbol(",")])
        _ = tracker.begin(id: 7, at: comma)
        _ = tracker.move(id: 7, to: CGPoint(x: comma.x, y: comma.y - 26))
        XCTAssertEqual(tracker.end(id: 7).triggered, [.symbol(".")])
        _ = tracker.begin(id: 8, at: x)
        _ = tracker.move(id: 8, to: CGPoint(x: x.x, y: x.y - 26))
        XCTAssertEqual(tracker.end(id: 8).triggered, [.literal(".")])

        // 6. 多指连打：第一指已上划 q ("1") 尚未抬起时第二指按下 w，应先提交 "1" 再提交 "w"
        _ = tracker.begin(id: 9, at: q)
        _ = tracker.move(id: 9, to: CGPoint(x: q.x, y: q.y - 24))
        let secondDown = tracker.begin(id: 10, at: w)
        XCTAssertEqual(secondDown.triggered, [.symbol("1")])
        XCTAssertTrue(tracker.end(id: 9).triggered.isEmpty)
        XCTAssertEqual(tracker.end(id: 10).triggered, [.letter("w")])
    }
}
