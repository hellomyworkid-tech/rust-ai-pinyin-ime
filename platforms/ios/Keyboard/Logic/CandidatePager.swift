import Foundation

/// 候选栏和展开的候选网格共用的连续候选列表。
/// 第一页来自引擎的视图（引擎始终停在第 0 页）；往后滚动时用 engine.candidates(from:count:) 取更多接在后面，
/// 不翻引擎的页。
///
/// 系统字体显示不出来的候选（生僻字显示成带问号的方框，见 [`CandidateRenderability`]）不放进列表，
/// 所以列表里的下标不一定等于引擎的全局下标：选词时用 `engineIndex(at:)` 换算。
struct CandidatePager: Equatable {
    private(set) var candidates: [IMECandidate] = []
    /// `candidates[i]` 在引擎全部候选里的下标
    private(set) var engineIndices: [Int] = []
    /// 已经从引擎取到的候选个数（含没显示的）。下一次「加载更多」从这里开始取
    private(set) var fetchedCount = 0
    private(set) var total = 0
    private(set) var pageSize = 0

    /// 判断候选能否显示。测试里可以换掉
    static var isDisplayable: (IMECandidate) -> Bool = { CandidateRenderability.isRenderable($0.text) }

    static func == (lhs: CandidatePager, rhs: CandidatePager) -> Bool {
        lhs.candidates == rhs.candidates && lhs.engineIndices == rhs.engineIndices
            && lhs.fetchedCount == rhs.fetchedCount && lhs.total == rhs.total && lhs.pageSize == rhs.pageSize
    }

    var hasMore: Bool { fetchedCount < total && pageSize > 0 }

    /// 第一页之后的候选也加载了：用户正在往后翻
    var hasLoadedMore: Bool { fetchedCount > pageSize }

    /// 列表第 `index` 个候选在引擎里的全局下标（交给 selectCandidate）
    func engineIndex(at index: Int) -> Int? {
        engineIndices.indices.contains(index) ? engineIndices[index] : nil
    }

    /// 收下引擎的一个视图：第 0 页从头来过；后面的页（触屏键盘不再翻页，只为兼容）接在对应位置上
    mutating func apply(_ view: IMEView) {
        guard view.visible else {
            self = CandidatePager()
            return
        }
        let start = view.pageIndex * view.pageSize
        if view.pageIndex == 0 {
            candidates = []
            engineIndices = []
        } else if start <= fetchedCount {
            let keep = engineIndices.firstIndex { $0 >= start } ?? engineIndices.count
            candidates.removeSubrange(keep...)
            engineIndices.removeSubrange(keep...)
        } else {
            // 中间缺了页，接不上（不应发生）：保留已有的
            return
        }
        add(view.visibleCandidates, from: start)
        fetchedCount = start + view.visibleCandidates.count
        pageSize = view.pageSize
        total = view.totalCandidates
    }

    /// 接上从 offset 开始取到的更多候选。offset 必须正好是已取到的个数，否则（列表已经换过）丢弃
    mutating func append(_ more: [IMECandidate], at offset: Int) {
        guard offset == fetchedCount else { return }
        let accepted = Array(more.prefix(max(0, total - fetchedCount)))
        add(accepted, from: offset)
        fetchedCount += accepted.count
    }

    private mutating func add(_ items: [IMECandidate], from offset: Int) {
        for (i, candidate) in items.enumerated() where Self.isDisplayable(candidate) {
            candidates.append(candidate)
            engineIndices.append(offset + i)
        }
    }
}
