import UIKit

protocol CandidateGridDelegate: AnyObject {
    func candidateGrid(_ grid: CandidateGridView, didSelect index: Int)
    func candidateGrid(_ grid: CandidateGridView, isTouching: Bool)
    /// 快滚到底了，需要下一页
    func candidateGridNeedsMore(_ grid: CandidateGridView)
}

/// 展开后盖在按键区上的候选网格，纵向滚动，滚到底时按需取下一页。
final class CandidateGridView: UIView, UICollectionViewDataSource, UICollectionViewDelegateFlowLayout {
    weak var delegate: CandidateGridDelegate?

    private let layout = UICollectionViewFlowLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
    private var candidates: [IMECandidate] = []
    private var hasMore = false
    private var landscape = false
    private var requestedMore = false
    /// 每个候选的宽度，按 GridPacking 排好，每行正好排满
    private var cellWidths: [CGFloat] = []
    private var packedWidth: CGFloat = 0

    /// 每行最多几格
    static let columns: CGFloat = 6

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = KeyboardTheme.background
        layout.minimumInteritemSpacing = 0
        layout.minimumLineSpacing = 0
        collectionView.backgroundColor = .clear
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.alwaysBounceVertical = true
        // 按下立刻高亮（didHighlightItemAt），压着的 AI 刷新才能在短促的点按里也拦住；默认会推迟到快松手时
        collectionView.delaysContentTouches = false
        collectionView.hideEdgeEffects()
        collectionView.register(CandidateGridCell.self, forCellWithReuseIdentifier: CandidateGridCell.reuseIdentifier)
        addSubview(collectionView)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func show(_ newCandidates: [IMECandidate], hasMore newHasMore: Bool, landscape newLandscape: Bool) {
        hasMore = newHasMore
        requestedMore = false
        guard newCandidates != candidates || newLandscape != landscape else { return }
        let restart = newCandidates.first != candidates.first
        candidates = newCandidates
        landscape = newLandscape
        pack()
        collectionView.reloadData()
        if restart { collectionView.setContentOffset(.zero, animated: false) }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let frame = bounds.insetBy(dx: horizontalInset, dy: 0)
        if collectionView.frame != frame {
            collectionView.frame = frame
            pack()
            layout.invalidateLayout()
        }
    }

    private func pack() {
        let available = collectionView.bounds.width
        packedWidth = available
        guard available > 0 else {
            cellWidths = []
            return
        }
        let unit = available / Self.columns
        let needed = candidates.map { CandidateCell.width(for: $0.text, isFirst: true, landscape: landscape) }
        let spans = GridPacking.spans(neededWidths: needed, unit: unit, columns: Int(Self.columns))
        // 向下取整，免得一行的总宽因为小数超出而折行
        cellWidths = spans.map { floor(CGFloat($0) * unit) }
    }

    private var rowHeight: CGFloat { landscape ? 40 : 48 }

    // MARK: - UICollectionView

    func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int {
        candidates.count
    }

    func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
        let cell = collectionView.dequeueReusableCell(withReuseIdentifier: CandidateGridCell.reuseIdentifier, for: indexPath)
        if let gridCell = cell as? CandidateGridCell {
            gridCell.configure(candidates[indexPath.item], isFirst: indexPath.item == 0, landscape: landscape)
        }
        return cell
    }

    func collectionView(_ collectionView: UICollectionView, layout collectionViewLayout: UICollectionViewLayout,
                        sizeForItemAt indexPath: IndexPath) -> CGSize {
        if packedWidth != collectionView.bounds.width || cellWidths.count != candidates.count { pack() }
        let width = indexPath.item < cellWidths.count ? cellWidths[indexPath.item] : collectionView.bounds.width
        return CGSize(width: max(1, width), height: rowHeight)
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        delegate?.candidateGrid(self, didSelect: indexPath.item)
    }

    func collectionView(_ collectionView: UICollectionView, didHighlightItemAt indexPath: IndexPath) {
        delegate?.candidateGrid(self, isTouching: true)
    }

    func collectionView(_ collectionView: UICollectionView, didUnhighlightItemAt indexPath: IndexPath) {
        // 取消高亮发生在 didSelect 之前；推迟一拍，保证选词先于压着的 AI 刷新
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.delegate?.candidateGrid(self, isTouching: false)
        }
    }

    func collectionView(_ collectionView: UICollectionView, willDisplay cell: UICollectionViewCell,
                        forItemAt indexPath: IndexPath) {
        guard hasMore, !requestedMore, indexPath.item >= candidates.count - 6 else { return }
        requestedMore = true
        // 不在 willDisplay 里直接改数据源
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.delegate?.candidateGridNeedsMore(self)
        }
    }
}
