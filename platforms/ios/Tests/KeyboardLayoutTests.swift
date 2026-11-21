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

    func testPostureDetectsIPhoneDuoFoldedVsUnfolded() {
        // 直板机竖屏（iPhone 18 Pro 402pt / Pro Max 440pt）
        XCTAssertFalse(KeyboardPosture.isUnfolded(horizontalIsRegular: false, verticalIsCompact: false, width: 402))
        XCTAssertFalse(KeyboardPosture.isUnfolded(horizontalIsRegular: false, verticalIsCompact: false, width: 440))

        // iPhone Duo 合盖外屏竖屏（1398×2034 @3x = 466×678pt，compact × regular）→ 常规单块键盘
        XCTAssertFalse(KeyboardPosture.isUnfolded(horizontalIsRegular: false, verticalIsCompact: false, width: 466))

        // iPhone Duo 合盖外屏横屏与直板机横屏（verticalSizeClass == .compact）→ 紧凑横屏模式
        XCTAssertFalse(KeyboardPosture.isUnfolded(horizontalIsRegular: false, verticalIsCompact: true, width: 678))
        XCTAssertFalse(KeyboardPosture.isUnfolded(horizontalIsRegular: true, verticalIsCompact: true, width: 956))

        // iPhone Duo 展开内屏：默认书本态 951×669pt 与旋转态 669×951pt（regular × regular）→ 展开态
        XCTAssertTrue(KeyboardPosture.isUnfolded(horizontalIsRegular: true, verticalIsCompact: false, width: 951))
        XCTAssertTrue(KeyboardPosture.isUnfolded(horizontalIsRegular: true, verticalIsCompact: false, width: 669))
        // 即使冷启动布局前 width 暂为 0，只要系统上报 regular × regular 也能在首帧准确识别
        XCTAssertTrue(KeyboardPosture.isUnfolded(horizontalIsRegular: true, verticalIsCompact: false, width: 0))
    }

    func testSplitLetterNumberAndSymbolRows() {
        let split = KeyboardLayout.splitRows(for: .letters, chineseMode: true, showsGlobe: true)
        XCTAssertEqual(split.count, 4)
        XCTAssertEqual(letters(split[0].left), "qwert")
        XCTAssertEqual(letters(split[0].right), "yuiop")
        XCTAssertEqual(letters(split[1].left), "asdfg")
        XCTAssertEqual(letters(split[1].right), "hjkl")
        XCTAssertEqual(split[2].left.first?.action, .shift)
        XCTAssertEqual(letters(split[2].left), "zxcv")
        XCTAssertEqual(letters(split[2].right), "bnm")
        XCTAssertEqual(split[2].right.last?.action, .backspace)

        // 底排左右各有一个空格键，方便左右拇指任意一侧直接上屏
        XCTAssertTrue(split[3].left.contains { $0.action == .space })
        XCTAssertTrue(split[3].right.contains { $0.action == .space })
        XCTAssertTrue(split[3].left.contains { $0.action == .globe })
        XCTAssertTrue(split[3].right.contains { $0.action == .toggleLanguage })
        XCTAssertEqual(split[3].right.last?.action, .returnKey)

        // 26 个字母键及其上划副符号与整排模式逐一完全一致
        let mergedLetters = KeyboardLayout.rows(for: .letters, chineseMode: true, showsGlobe: true)
            .flatMap { $0 }
            .filter { if case .letter = $0.action { return true }; return false }
        let splitLetters = split
            .flatMap { $0.left + $0.right }
            .filter { if case .letter = $0.action { return true }; return false }
        XCTAssertEqual(splitLetters.map(\.action), mergedLetters.map(\.action))
        XCTAssertEqual(splitLetters.map(\.swipeUpAction), mergedLetters.map(\.swipeUpAction))

        for page in [KeyboardPage.letters, .numbers, .symbols] {
            let withoutGlobe = KeyboardLayout.splitRows(for: page, chineseMode: true, showsGlobe: false)
                .flatMap { $0.left + $0.right }
            let withGlobe = KeyboardLayout.splitRows(for: page, chineseMode: true, showsGlobe: true)
                .flatMap { $0.left + $0.right }
            XCTAssertFalse(withoutGlobe.contains { $0.action == .globe })
            XCTAssertEqual(withGlobe.filter { $0.action == .globe }.count, 1)
            XCTAssertEqual(withGlobe.filter { $0.action == .space }.count, 2, "\(page) 分离态应提供左右双空格")
            XCTAssertEqual(withGlobe.filter { $0.action == .backspace }.count, 1)
            XCTAssertEqual(withGlobe.filter { $0.action == .returnKey }.count, 1)
        }
    }

    func testSplitGeometryOnIPhoneDuoUnfoldedPostures() {
        // 覆盖 iPhone Duo 书本展开态 (951pt)、旋转展开态 (669pt) 及不对称安全区场景
        let cases: [(CGFloat, CGFloat, CGFloat, CGFloat, CGFloat)] = [
            (951, 208, 0, 0, 200),
            (669, 208, 0, 0, 72),
            (951, 208, 24, 0, 180),
        ]
        for (width, height, leftSafe, rightSafe, expectedMinValley) in cases {
            var metrics = KeyboardGeometry.SplitMetrics()
            metrics.leftSafeInset = leftSafe
            metrics.rightSafeInset = rightSafe
            let bounds = CGRect(x: 0, y: 0, width: width, height: height)

            for page in [KeyboardPage.letters, .numbers, .symbols] {
                for globe in [false, true] {
                    let rows = KeyboardLayout.splitRows(for: page, chineseMode: true, showsGlobe: globe)
                    let result = KeyboardGeometry.layoutSplit(rows: rows, in: bounds, metrics: metrics)
                    let valley = result.rightBankFrame.minX - result.leftBankFrame.maxX
                    XCTAssertGreaterThanOrEqual(valley, expectedMinValley, "中缝宽度不足：\(valley)")
                    XCTAssertEqual(result.frames.count, rows.reduce(0) { $0 + $1.left.count + $1.right.count })

                    for (r, splitRow) in rows.enumerated() {
                        let inRow = result.frames.filter { $0.row == r }
                        let leftFrames = Array(inRow.prefix(splitRow.left.count))
                        let rightFrames = Array(inRow.suffix(splitRow.right.count))

                        for f in leftFrames {
                            XCTAssertGreaterThanOrEqual(f.frame.minX, result.leftBankFrame.minX - 0.5)
                            XCTAssertLessThanOrEqual(f.frame.maxX, result.leftBankFrame.maxX + 0.5)
                            XCTAssertGreaterThan(f.frame.width, 32)
                            XCTAssertTrue(f.touchFrame.contains(f.frame))
                        }
                        for f in rightFrames {
                            XCTAssertGreaterThanOrEqual(f.frame.minX, result.rightBankFrame.minX - 0.5)
                            XCTAssertLessThanOrEqual(f.frame.maxX, result.rightBankFrame.maxX + 0.5)
                            XCTAssertGreaterThan(f.frame.width, 32)
                            XCTAssertTrue(f.touchFrame.contains(f.frame))
                        }
                        for (a, b) in zip(leftFrames, leftFrames.dropFirst()) {
                            XCTAssertLessThanOrEqual(a.frame.maxX, b.frame.minX + 0.01)
                            XCTAssertEqual(a.touchFrame.maxX, b.touchFrame.minX, accuracy: 0.01)
                        }
                        for (a, b) in zip(rightFrames, rightFrames.dropFirst()) {
                            XCTAssertLessThanOrEqual(a.frame.maxX, b.frame.minX + 0.01)
                            XCTAssertEqual(a.touchFrame.maxX, b.touchFrame.minX, accuracy: 0.01)
                        }
                        if let lastLeft = leftFrames.last, let firstRight = rightFrames.first {
                            XCTAssertLessThanOrEqual(lastLeft.touchFrame.maxX, firstRight.touchFrame.minX + 0.01,
                                                     "左右半区触摸框在中缝处不应交叉重叠")
                        }
                    }
                }
            }
        }
    }

    func testSplitTouchTrackerIgnoresCenterValleyAndSupportsDualSpacebars() {
        let bounds = CGRect(x: 0, y: 0, width: 951, height: 208)
        let rows = KeyboardLayout.splitRows(for: .letters, chineseMode: true, showsGlobe: false)
        let result = KeyboardGeometry.layoutSplit(rows: rows, in: bounds, metrics: .init())
        let specs = rows.flatMap { $0.left + $0.right }
        var tracker = KeyboardTouchTracker(specs: specs, frames: result.frames)

        // 1. 手指落在 951pt 展开屏正中央折痕空白区（约 x=475.5）不应误触发两侧的 t 或 y
        let centerPoint = CGPoint(x: bounds.midX, y: bounds.midY)
        let centerDown = tracker.begin(id: "valley", at: centerPoint)
        XCTAssertFalse(centerDown.didTouchDown)
        XCTAssertTrue(centerDown.triggered.isEmpty)

        // 2. 左空格与右空格均可独立触发 .space
        let spaceIndices = specs.indices.filter { specs[$0].action == .space }
        XCTAssertEqual(spaceIndices.count, 2)
        for (k, idx) in spaceIndices.enumerated() {
            let f = result.frames[idx].frame
            _ = tracker.begin(id: "space-\(k)", at: CGPoint(x: f.midX, y: f.midY))
            XCTAssertEqual(tracker.end(id: "space-\(k)").triggered, [.space])
        }

        // 3. 左半区内部从 r 横滑到 t 可正常换键，但继续往中央空白区外拖不会跨区跳到右半区的 y
        let rIdx = specs.firstIndex { $0.action == .letter("r") }!
        let tIdx = specs.firstIndex { $0.action == .letter("t") }!
        let rCenter = CGPoint(x: result.frames[rIdx].frame.midX, y: result.frames[rIdx].frame.midY)
        let tCenter = CGPoint(x: result.frames[tIdx].frame.midX, y: result.frames[tIdx].frame.midY)
        _ = tracker.begin(id: "slide", at: rCenter)
        XCTAssertEqual(tracker.move(id: "slide", to: tCenter).preview?.action, .letter("t"))
        _ = tracker.move(id: "slide", to: CGPoint(x: bounds.midX, y: tCenter.y))
        XCTAssertEqual(tracker.end(id: "slide").triggered, [.letter("t")])
    }
}
