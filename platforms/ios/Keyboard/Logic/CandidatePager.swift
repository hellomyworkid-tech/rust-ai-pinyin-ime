import Foundation

/// 候选栏和展开的候选网格共用的连续候选列表。
/// 第一页来自引擎的视图（引擎始终停在第 0 页）；往后滚动时用 engine.candidates(from:count:) 取更多接在后面，
/// 不翻引擎的页。下标就是引擎的全局下标，可以直接交给 selectCandidate。
struct CandidatePager: Equatable {
    private(set) var candidates: [IMECandidate] = []
    private(set) var total = 0
    private(set) var pageSize = 0

    var hasMore: Bool { candidates.count < total && pageSize > 0 }

    /// 第一页之后的候选也加载了：用户正在往后翻
    var hasLoadedMore: Bool { candidates.count > pageSize }

    /// 收下引擎的一个视图：第 0 页从头来过；后面的页（触屏键盘不再翻页，只为兼容）接在对应位置上
    mutating func apply(_ view: IMEView) {
        guard view.visible else {
            self = CandidatePager()
            return
        }
        let start = view.pageIndex * view.pageSize
        if view.pageIndex == 0 {
            candidates = view.visibleCandidates
        } else if start <= candidates.count {
            candidates.removeSubrange(start...)
            candidates.append(contentsOf: view.visibleCandidates)
        } else {
            // 中间缺了页，接不上（不应发生）：保留已有的
            return
        }
        pageSize = view.pageSize
        total = view.totalCandidates
    }

    /// 接上从 offset 开始取到的更多候选。offset 必须正好是已有的个数，否则（列表已经换过）丢弃
    mutating func append(_ more: [IMECandidate], at offset: Int) {
        guard offset == candidates.count else { return }
        candidates.append(contentsOf: more.prefix(max(0, total - candidates.count)))
    }
}
