import UIKit

protocol CandidateBarDelegate: AnyObject {
    func candidateBar(_ bar: CandidateBarView, didSelect index: Int)
    /// 手指按在 / 离开候选上（期间到达的 AI 结果先不刷新）
    func candidateBar(_ bar: CandidateBarView, isTouching: Bool)
    /// 滑到了末尾，需要下一页
    func candidateBarNeedsMore(_ bar: CandidateBarView)
    func candidateBarDidToggleExpand(_ bar: CandidateBarView)
}

/// 横向滚动的候选栏，右侧是展开候选网格的按钮。候选格子复用。
final class CandidateBarView: UIView, UIScrollViewDelegate {
    weak var delegate: CandidateBarDelegate?

    private let scrollView = TouchCancellingScrollView()
    private let expandButton = UIButton(type: .system)
    private let expandSeparator = UIView()
    /// 展开网格时候选栏位置上的提示
    private let summaryLabel = UILabel()
    private var cells: [CandidateCell] = []
    private var candidates: [IMECandidate] = []
    private var hasMore = false
    private var landscape = false
    private var requestedMore = false

    static let expandButtonWidth: CGFloat = 44

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        scrollView.showsHorizontalScrollIndicator = false
        scrollView.showsVerticalScrollIndicator = false
        scrollView.alwaysBounceHorizontal = true
        scrollView.delaysContentTouches = false
        scrollView.delegate = self
        scrollView.hideEdgeEffects()
        addSubview(scrollView)

        summaryLabel.text = "全部候选"
        summaryLabel.font = .systemFont(ofSize: 15, weight: .medium)
        summaryLabel.textColor = KeyboardTheme.secondaryText
        addSubview(summaryLabel)
        expandSeparator.backgroundColor = KeyboardTheme.separator
        addSubview(expandSeparator)
        expandButton.tintColor = KeyboardTheme.keyText
        expandButton.accessibilityLabel = "展开候选"
        expandButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.delegate?.candidateBarDidToggleExpand(self)
        }, for: .touchUpInside)
        addSubview(expandButton)
        setExpanded(false)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func setExpanded(_ expanded: Bool) {
        let config = UIImage.SymbolConfiguration(pointSize: 15, weight: .semibold)
        expandButton.setImage(UIImage(systemName: expanded ? "chevron.up" : "chevron.down", withConfiguration: config),
                              for: .normal)
        expandButton.accessibilityLabel = expanded ? "收起候选" : "展开候选"
        scrollView.isHidden = expanded
        summaryLabel.isHidden = !expanded
    }

    /// 显示候选。列表没变就什么都不做；第一项变了（新输入）就滚回开头
    func show(_ newCandidates: [IMECandidate], hasMore newHasMore: Bool, landscape newLandscape: Bool) {
        hasMore = newHasMore
        requestedMore = false
        guard newCandidates != candidates || newLandscape != landscape else { return }
        let restart = newCandidates.first != candidates.first || newCandidates.count < candidates.count
        candidates = newCandidates
        landscape = newLandscape
        expandButton.isHidden = newCandidates.isEmpty
        expandSeparator.isHidden = newCandidates.isEmpty
        layoutCells()
        if restart { scrollView.contentOffset = .zero }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let buttonX = bounds.maxX - Self.expandButtonWidth - horizontalInset
        expandButton.frame = CGRect(x: buttonX, y: 0, width: Self.expandButtonWidth, height: bounds.height)
        expandSeparator.frame = CGRect(x: buttonX, y: bounds.height * 0.25, width: 1 / max(1, traitCollection.displayScale),
                                       height: bounds.height * 0.5)
        let newFrame = CGRect(x: horizontalInset, y: 0, width: buttonX - horizontalInset, height: bounds.height)
        summaryLabel.frame = newFrame.insetBy(dx: 12, dy: 0)
        if scrollView.frame != newFrame {
            scrollView.frame = newFrame
            layoutCells()
        }
    }

    private func layoutCells() {
        while cells.count < candidates.count {
            let cell = CandidateCell()
            cell.addTarget(self, action: #selector(cellTouchDown(_:)), for: .touchDown)
            cell.addTarget(self, action: #selector(cellTouchUpInside(_:)), for: .touchUpInside)
            cell.addTarget(self, action: #selector(cellTouchEnded(_:)), for: [.touchUpOutside, .touchCancel])
            scrollView.addSubview(cell)
            cells.append(cell)
        }
        var x: CGFloat = 4
        let height = scrollView.bounds.height
        for (i, cell) in cells.enumerated() {
            guard i < candidates.count else {
                cell.isHidden = true
                continue
            }
            let isFirst = i == 0
            let width = CandidateCell.width(for: candidates[i].text, isFirst: isFirst, landscape: landscape)
            cell.isHidden = false
            cell.configure(candidates[i], index: i, isFirst: isFirst, landscape: landscape)
            cell.frame = CGRect(x: x, y: 0, width: width, height: height)
            x += width
        }
        scrollView.contentSize = CGSize(width: x + 4, height: height)
    }

    @objc private func cellTouchDown(_ cell: CandidateCell) {
        delegate?.candidateBar(self, isTouching: true)
    }

    @objc private func cellTouchUpInside(_ cell: CandidateCell) {
        // 先选，再松开「正在触摸」：压着的 AI 刷新不能抢在选词之前改掉下标
        delegate?.candidateBar(self, didSelect: cell.index)
        delegate?.candidateBar(self, isTouching: false)
    }

    @objc private func cellTouchEnded(_ cell: CandidateCell) {
        delegate?.candidateBar(self, isTouching: false)
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        guard hasMore, !requestedMore else { return }
        let remaining = scrollView.contentSize.width - scrollView.contentOffset.x - scrollView.bounds.width
        if remaining < scrollView.bounds.width / 2 {
            requestedMore = true
            delegate?.candidateBarNeedsMore(self)
        }
    }
}

/// 手指按在候选上也能拖动滚动（UIScrollView 默认不取消 UIControl 上的触摸）
final class TouchCancellingScrollView: UIScrollView {
    override func touchesShouldCancel(in view: UIView) -> Bool {
        true
    }
}
