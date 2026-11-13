import CoreGraphics

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

    struct KeyFrame: Equatable {
        let row: Int
        let index: Int
        let frame: CGRect
        let touchFrame: CGRect
    }

    /// 一个字母键宽：第一行十个键刚好排满
    static func unitWidth(totalWidth: CGFloat, metrics: Metrics) -> CGFloat {
        max(1, (totalWidth - 2 * metrics.sideInset - 9 * metrics.gap) / 10)
    }

    static func layout(rows: [[KeySpec]], in bounds: CGRect, metrics: Metrics) -> [KeyFrame] {
        guard !rows.isEmpty, bounds.width > 0, bounds.height > 0 else { return [] }
        let unit = unitWidth(totalWidth: bounds.width, metrics: metrics)
        let pitch = bounds.height / CGFloat(rows.count)
        let keyHeight = max(1, pitch - metrics.rowGap)
        var result: [KeyFrame] = []

        for (r, row) in rows.enumerated() {
            let rowTop = bounds.minY + CGFloat(r) * pitch
            let widths = keyWidths(row, totalWidth: bounds.width, unit: unit, metrics: metrics)
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

    private static func keyWidths(_ row: [KeySpec], totalWidth: CGFloat, unit: CGFloat, metrics: Metrics) -> [CGFloat] {
        let gaps = CGFloat(max(0, row.count - 1)) * metrics.gap
        var fixed: CGFloat = 0
        var flexibleCount = 0
        for spec in row {
            switch spec.width {
            case .unit(let u): fixed += u * unit
            case .flexible: flexibleCount += 1
            }
        }
        let remaining = totalWidth - 2 * metrics.sideInset - gaps - fixed
        let flexible = flexibleCount > 0 ? max(unit, remaining / CGFloat(flexibleCount)) : 0
        return row.map { spec in
            switch spec.width {
            case .unit(let u): return u * unit
            case .flexible: return flexible
            }
        }
    }
}
