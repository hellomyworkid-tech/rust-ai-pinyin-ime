import CoreGraphics

/// 折叠屏姿态判定（不依赖 UIKit，便于单元测试覆盖各机型与姿态）
enum KeyboardPosture {
    /// 判断当前是否处于折叠屏展开内屏（如 iPhone Duo）：
    /// - iPhone Duo 展开内屏：默认书本态 `951×669pt`、旋转 90° 后 `669×951pt`，横纵 SizeClass 均为 `.regular`；
    /// - iPhone Duo 合盖外屏竖屏：`466×678pt`（`compact × regular`），保持常规单块键盘；
    /// - 直板机与合盖外屏横屏：`verticalSizeClass == .compact`，走紧凑横屏布局；
    /// - 兜底：若宿主应用未升级新 SDK 上报 `regular` 横向 SizeClass，但在非紧凑竖高下宽度已达 `≥ 580pt`，同样视作展开内屏。
    static func isUnfolded(horizontalIsRegular: Bool, verticalIsCompact: Bool, width: CGFloat) -> Bool {
        if verticalIsCompact {
            return false
        }
        if horizontalIsRegular {
            return true
        }
        return width >= 580
    }
}

/// 按键排布：给定键位和可用区域，算出每个键的可见框和触摸框。
/// 触摸框把键之间的缝也分给相邻的键，手指落在缝里也有反应。
enum KeyboardGeometry {
    struct Metrics: Equatable {
        /// 左右边距（横屏时再加上安全区）
        var sideInset: CGFloat
        /// 键与键的水平间距
        var gap: CGFloat
        /// 上下两行键之间的间距
        var rowGap: CGFloat

        // 行距的默认值。KeyboardView 实际按行高的两成取（竖屏行高 54 时 11、横屏 40 时 8），
        // 键高约占行高的八成，和系统键盘相近（54 里键高 42）
        static let portrait = Metrics(sideInset: 3, gap: 6, rowGap: 11)
        static let landscape = Metrics(sideInset: 3, gap: 6, rowGap: 8)
    }

    struct SplitMetrics: Equatable {
        /// 左右半区离屏幕边缘（扣除安全区后）的外边距
        var outerInset: CGFloat = 8
        /// 半区内部左右内边距（半区底板边缘到键帽的距离）
        var bankPadding: CGFloat = 6
        /// 键与键的水平间距
        var gap: CGFloat = 6
        /// 上下两行键之间的间距
        var rowGap: CGFloat = 10
        /// 单侧半区最大宽度（避免在 951pt 书本展开态下把键拉得过宽导致拇指够不到内侧键）
        var maxBankWidth: CGFloat = 336
        /// 左右半区之间的最小空白中缝宽度（避开折叠中轴折痕）
        var minCenterGap: CGFloat = 72
        /// 内侧边缘键向中缝延伸的容错触摸宽度
        var innerTouchSlop: CGFloat = 14
        /// 非对称左右安全区（iPhone Duo 铰链与屏下角落镜头可能导致左右安全区不对称）
        var leftSafeInset: CGFloat = 0
        var rightSafeInset: CGFloat = 0
    }

    struct KeyFrame: Equatable {
        let row: Int
        let index: Int
        let frame: CGRect
        let touchFrame: CGRect
    }

    struct SplitLayoutResult: Equatable {
        let leftBankFrame: CGRect
        let rightBankFrame: CGRect
        let frames: [KeyFrame]
    }

    /// 一个字母键宽：第一行十个键刚好排满
    static func unitWidth(totalWidth: CGFloat, metrics: Metrics) -> CGFloat {
        max(1, (totalWidth - 2 * metrics.sideInset - 9 * metrics.gap) / 10)
    }

    /// 分离键盘单侧半区的一个字母键宽：每侧第一行五个键刚好排满
    static func splitUnitWidth(bankWidth: CGFloat, metrics: SplitMetrics) -> CGFloat {
        max(1, (bankWidth - 2 * metrics.bankPadding - 4 * metrics.gap) / 5)
    }

    static func layout(rows: [[KeySpec]], in bounds: CGRect, metrics: Metrics) -> [KeyFrame] {
        guard !rows.isEmpty, bounds.width > 0, bounds.height > 0 else { return [] }
        let unit = unitWidth(totalWidth: bounds.width, metrics: metrics)
        let pitch = bounds.height / CGFloat(rows.count)
        let keyHeight = max(1, pitch - metrics.rowGap)
        var result: [KeyFrame] = []

        for (r, row) in rows.enumerated() {
            let rowTop = bounds.minY + CGFloat(r) * pitch
            let widths = keyWidths(row, totalWidth: bounds.width, sideInset: metrics.sideInset, gap: metrics.gap, unit: unit)
            let used = widths.reduce(0, +) + CGFloat(max(0, row.count - 1)) * metrics.gap
            // 没有弹性键的行（如第二行字母）居中
            var x = bounds.minX + max(metrics.sideInset, (bounds.width - used) / 2)
            if row.contains(where: { $0.width == .flexible }) { x = bounds.minX + metrics.sideInset }

            for (i, width) in widths.enumerated() {
                let frame = CGRect(x: x, y: rowTop + metrics.rowGap / 2, width: width, height: keyHeight)
                // 触摸框：左右各分一半缝；行首行尾的键一直延伸到边上
                let left = i == 0 ? bounds.minX : x - metrics.gap / 2
                let right = i == widths.count - 1 ? bounds.maxX : x + width + metrics.gap / 2
                let touch = CGRect(x: left, y: rowTop, width: right - left, height: pitch)
                result.append(KeyFrame(row: r, index: i, frame: frame, touchFrame: touch))
                x += width + metrics.gap
            }
        }
        return result
    }

    /// 折叠屏展开态两侧分离排布：左半区贴紧左侧安全区、右半区贴紧右侧安全区，中间留出中缝供拇指舒展。
    static func layoutSplit(rows: [SplitRowSpec], in bounds: CGRect, metrics: SplitMetrics) -> SplitLayoutResult {
        guard !rows.isEmpty, bounds.width > 0, bounds.height > 0 else {
            return SplitLayoutResult(leftBankFrame: .zero, rightBankFrame: .zero, frames: [])
        }
        let leftOriginX = bounds.minX + metrics.leftSafeInset + metrics.outerInset
        let rightMaxX = bounds.maxX - metrics.rightSafeInset - metrics.outerInset
        let usableWidth = max(1, rightMaxX - leftOriginX)
        let minValley = min(usableWidth * 0.45, max(metrics.minCenterGap, (usableWidth * 0.12).rounded()))
        let bankWidth = max(60, min(metrics.maxBankWidth, floor((usableWidth - minValley) / 2)))

        let plateTop = bounds.minY + 2
        let plateHeight = max(0, bounds.height - 4)
        let leftBank = CGRect(x: leftOriginX, y: plateTop, width: bankWidth, height: plateHeight)
        let rightBank = CGRect(x: max(leftBank.maxX, rightMaxX - bankWidth), y: plateTop, width: bankWidth, height: plateHeight)
        let valleyMidX = (leftBank.maxX + rightBank.minX) / 2

        let unit = splitUnitWidth(bankWidth: bankWidth, metrics: metrics)
        let pitch = bounds.height / CGFloat(rows.count)
        let keyHeight = max(1, pitch - metrics.rowGap)
        var result: [KeyFrame] = []

        for (r, splitRow) in rows.enumerated() {
            let rowTop = bounds.minY + CGFloat(r) * pitch

            // 左半区（index: 0 ..< left.count）
            let leftWidths = keyWidths(
                splitRow.left,
                totalWidth: leftBank.width,
                sideInset: metrics.bankPadding,
                gap: metrics.gap,
                unit: unit,
                minFlexibleUnitRatio: 0.9
            )
            let leftUsed = leftWidths.reduce(0, +) + CGFloat(max(0, splitRow.left.count - 1)) * metrics.gap
            var lx = leftBank.minX + max(metrics.bankPadding, (leftBank.width - leftUsed) / 2)
            if splitRow.left.contains(where: { $0.width == .flexible }) {
                lx = leftBank.minX + metrics.bankPadding
            }
            for (i, width) in leftWidths.enumerated() {
                let frame = CGRect(x: lx, y: rowTop + metrics.rowGap / 2, width: width, height: keyHeight)
                let touchLeft = i == 0 ? bounds.minX : lx - metrics.gap / 2
                let touchRight = i == leftWidths.count - 1
                    ? min(valleyMidX, leftBank.maxX + metrics.innerTouchSlop)
                    : lx + width + metrics.gap / 2
                let touch = CGRect(x: touchLeft, y: rowTop, width: max(0, touchRight - touchLeft), height: pitch)
                result.append(KeyFrame(row: r, index: i, frame: frame, touchFrame: touch))
                lx += width + metrics.gap
            }

            // 右半区（index: left.count ..< left.count + right.count）
            let rightWidths = keyWidths(
                splitRow.right,
                totalWidth: rightBank.width,
                sideInset: metrics.bankPadding,
                gap: metrics.gap,
                unit: unit,
                minFlexibleUnitRatio: 0.9
            )
            let rightUsed = rightWidths.reduce(0, +) + CGFloat(max(0, splitRow.right.count - 1)) * metrics.gap
            var rx = rightBank.minX + max(metrics.bankPadding, (rightBank.width - rightUsed) / 2)
            if splitRow.right.contains(where: { $0.width == .flexible }) {
                rx = rightBank.minX + metrics.bankPadding
            }
            let baseIndex = splitRow.left.count
            for (i, width) in rightWidths.enumerated() {
                let frame = CGRect(x: rx, y: rowTop + metrics.rowGap / 2, width: width, height: keyHeight)
                let touchLeft = i == 0
                    ? max(valleyMidX, rightBank.minX - metrics.innerTouchSlop)
                    : rx - metrics.gap / 2
                let touchRight = i == rightWidths.count - 1 ? bounds.maxX : rx + width + metrics.gap / 2
                let touch = CGRect(x: touchLeft, y: rowTop, width: max(0, touchRight - touchLeft), height: pitch)
                result.append(KeyFrame(row: r, index: baseIndex + i, frame: frame, touchFrame: touch))
                rx += width + metrics.gap
            }
        }

        return SplitLayoutResult(leftBankFrame: leftBank, rightBankFrame: rightBank, frames: result)
    }

    private static func keyWidths(_ row: [KeySpec],
                                  totalWidth: CGFloat,
                                  sideInset: CGFloat,
                                  gap: CGFloat,
                                  unit: CGFloat,
                                  minFlexibleUnitRatio: CGFloat = 1.0) -> [CGFloat] {
        let gaps = CGFloat(max(0, row.count - 1)) * gap
        var fixed: CGFloat = 0
        var flexibleCount = 0
        for spec in row {
            switch spec.width {
            case .unit(let u): fixed += u * unit
            case .flexible: flexibleCount += 1
            }
        }
        let remaining = totalWidth - 2 * sideInset - gaps - fixed
        let flexible = flexibleCount > 0 ? max(unit * minFlexibleUnitRatio, remaining / CGFloat(flexibleCount)) : 0
        return row.map { spec in
            switch spec.width {
            case .unit(let u): return u * unit
            case .flexible: return flexible
            }
        }
    }
}

/// 按键区的纯触摸状态机：支持多指连打提前提交、上划选数字/标点、滑回取消上划、横向滑向邻键换键。
/// 不依赖 UIKit，方便直接对坐标序列做单元测试。
struct KeyboardTouchTracker {
    /// 落在所有 touchFrame 之外（如分离键盘中间的空白折痕区）时的最大吸附距离（pt）：
    /// 保留键帽边缘微偏容错，同时避免手指误碰宽幅中缝空白区时触发远处的按键
    static let maxFallbackSnapDistance: CGFloat = 28
    struct Preview: Equatable {
        let keyIndex: Int
        let action: KeyAction
    }

    struct Outcome: Equatable {
        var didTouchDown = false
        var triggered: [KeyAction] = []
        var preview: Preview? = nil
        var hidePreview = false
        var forwardGlobe = false
        var startBackspaceRepeat = false
        var stopBackspaceRepeat = false
    }

    private struct TrackedTouch {
        var keyIndex: Int
        var anchorPoint: CGPoint
        var isSwipedUp: Bool
        var consumed: Bool
    }

    var specs: [KeySpec] = []
    var frames: [KeyboardGeometry.KeyFrame] = []
    private var tracked: [AnyHashable: TrackedTouch] = [:]

    var pressedKeyIndices: Set<Int> {
        Set(tracked.values.filter { !$0.consumed || specs[$0.keyIndex].action == .backspace || specs[$0.keyIndex].action == .shift || specs[$0.keyIndex].action == .globe }.map(\.keyIndex))
    }

    var swipeUpActivateDistance: CGFloat {
        let keyHeight = frames.first?.frame.height ?? 42
        return max(14, (keyHeight * 0.38).rounded())
    }

    var swipeUpCancelDistance: CGFloat {
        max(8, (swipeUpActivateDistance * 0.55).rounded())
    }

    func keyIndex(at point: CGPoint) -> Int? {
        if let hit = frames.firstIndex(where: { $0.touchFrame.contains(point) }) { return hit }
        guard let nearest = frames.indices.min(by: { distance(point, frames[$0].frame) < distance(point, frames[$1].frame) }) else {
            return nil
        }
        let maxDistSquared = Self.maxFallbackSnapDistance * Self.maxFallbackSnapDistance
        return distance(point, frames[nearest].frame) <= maxDistSquared ? nearest : nil
    }

    private func distance(_ p: CGPoint, _ r: CGRect) -> CGFloat {
        let dx = max(r.minX - p.x, 0, p.x - r.maxX)
        let dy = max(r.minY - p.y, 0, p.y - r.maxY)
        return dx * dx + dy * dy
    }

    private func effectiveAction(for state: TrackedTouch) -> KeyAction {
        let spec = specs[state.keyIndex]
        if state.isSwipedUp, let swipeUp = spec.swipeUpAction {
            return swipeUp
        }
        return spec.action
    }

    private func preview(for index: Int, swipedUp: Bool) -> Preview? {
        guard index < specs.count, specs[index].showsPreview else { return nil }
        let spec = specs[index]
        let action = (swipedUp ? spec.swipeUpAction : nil) ?? spec.action
        return Preview(keyIndex: index, action: action)
    }

    mutating func begin(id: AnyHashable, at point: CGPoint) -> Outcome {
        var out = Outcome()
        guard let index = keyIndex(at: point), index < specs.count else { return out }
        let spec = specs[index]

        // 多指连打：新手指按下时，先让还按着的字符键按当前姿态（普通或已上划）生效
        for (otherID, state) in tracked where !state.consumed && specs[state.keyIndex].showsPreview {
            tracked[otherID]?.consumed = true
            out.triggered.append(effectiveAction(for: state))
        }

        tracked[id] = TrackedTouch(keyIndex: index, anchorPoint: point, isSwipedUp: false, consumed: false)
        out.didTouchDown = true
        switch spec.action {
        case .globe:
            out.forwardGlobe = true
            out.hidePreview = true
        case .shift:
            tracked[id]?.consumed = true
            out.triggered.append(.shift)
            out.hidePreview = true
        case .backspace:
            tracked[id]?.consumed = true
            out.triggered.append(.backspace)
            out.startBackspaceRepeat = true
            out.hidePreview = true
        default:
            if let p = preview(for: index, swipedUp: false) {
                out.preview = p
            } else {
                out.hidePreview = true
            }
        }
        return out
    }

    mutating func move(id: AnyHashable, to point: CGPoint) -> Outcome {
        var out = Outcome()
        guard var state = tracked[id], !state.consumed else { return out }
        let spec = specs[state.keyIndex]
        if spec.action == .globe {
            out.forwardGlobe = true
            return out
        }

        let dx = point.x - state.anchorPoint.x
        let dy = point.y - state.anchorPoint.y

        if spec.swipeUpAction != nil {
            if state.isSwipedUp {
                // 已处于上划状态：除非重新滑回键帽，否则锁定当前键的上划符号（即使滑入上一行或候选栏也不换键）
                if -dy < swipeUpCancelDistance {
                    state.isSwipedUp = false
                    tracked[id] = state
                    out.preview = preview(for: state.keyIndex, swipedUp: false)
                } else {
                    return out
                }
            } else {
                if -dy >= swipeUpActivateDistance && -dy >= abs(dx) {
                    state.isSwipedUp = true
                    tracked[id] = state
                    out.preview = preview(for: state.keyIndex, swipedUp: true)
                    return out
                }
                // 朝上方直滑途中，不切到上一行的键
                if dy < -4 && -dy >= abs(dx) {
                    return out
                }
            }
        }

        // 横向或向下滑到别的字符键时换键（带 3pt 边界防抖，避免按在键缝附近轻微晃动时误跳邻键）
        guard spec.showsPreview,
              !frames[state.keyIndex].touchFrame.insetBy(dx: -3, dy: 0).contains(point),
              let index = keyIndex(at: point), index != state.keyIndex,
              specs[index].showsPreview else { return out }
        state.keyIndex = index
        state.anchorPoint = point
        state.isSwipedUp = false
        tracked[id] = state
        out.preview = preview(for: index, swipedUp: false)
        return out
    }

    mutating func end(id: AnyHashable, trigger: Bool = true) -> Outcome {
        var out = Outcome()
        guard let state = tracked.removeValue(forKey: id) else { return out }
        let spec = specs[state.keyIndex]
        if spec.action == .backspace { out.stopBackspaceRepeat = true }
        if spec.action == .globe {
            out.forwardGlobe = true
        } else if trigger, !state.consumed {
            out.triggered.append(effectiveAction(for: state))
        }
        if !tracked.values.contains(where: { specs[$0.keyIndex].showsPreview && !$0.consumed }) {
            out.hidePreview = true
        }
        return out
    }

    mutating func cancelAll() {
        tracked.removeAll()
    }
}
