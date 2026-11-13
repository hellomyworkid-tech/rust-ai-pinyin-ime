import CoreGraphics

/// 候选网格的排法：每行固定若干格，每个候选按文字宽度占一格或几格；
/// 一行放不下下一个候选时，把这一行最后一个候选拉宽到行尾，最后一行同样补满。
/// 这样每一行都正好排满，竖线对齐，不会出现系统流式布局那种忽宽忽窄的间距。
enum GridPacking {
    /// 返回每个候选占几格
    static func spans(neededWidths: [CGFloat], unit: CGFloat, columns: Int) -> [Int] {
        guard unit > 0, columns > 0 else { return neededWidths.map { _ in 1 } }
        var spans: [Int] = []
        var used = 0
        for width in neededWidths {
            let need = min(columns, max(1, Int((width / unit).rounded(.up))))
            if used + need > columns, !spans.isEmpty {
                spans[spans.count - 1] += columns - used
                used = 0
            }
            spans.append(need)
            used += need
            if used == columns { used = 0 }
        }
        if used > 0, !spans.isEmpty { spans[spans.count - 1] += columns - used }
        return spans
    }
}
