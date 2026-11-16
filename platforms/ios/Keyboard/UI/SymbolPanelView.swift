import UIKit

protocol SymbolPanelDelegate: AnyObject {
    func symbolPanel(_ panel: SymbolPanelView, didSelectSymbol text: String)
    func symbolPanel(_ panel: SymbolPanelView, didSelectCategory category: SymbolCategory)
    func symbolPanelDidToggleLock(_ panel: SymbolPanelView)
    func symbolPanelDidTapBack(_ panel: SymbolPanelView)
    func symbolPanelDidTriggerBackspace(_ panel: SymbolPanelView)
    func symbolPanelDidTouchDown(_ panel: SymbolPanelView)
}

/// 分类符号面板（参考元输入法交互）：
/// - 上方是按分类纵向滚动的符号网格，全角标点右上角带「全」角标，超宽网址/颜文字自动跨列对齐；
/// - 底部左侧「返回」回到主键盘，中间横向滑动切换 12 个符号分类，右侧提供「锁定」与长按连删「退格」。
///   未锁定时点选一个符号即自动回到主键盘继续打字；锁定后留在面板内连续点选多个符号。
final class SymbolPanelView: UIView, UICollectionViewDataSource, UICollectionViewDelegateFlowLayout {
    struct State: Equatable {
        var category: SymbolCategory = .common
        var isLocked = false
        var landscape = false
    }

    weak var delegate: SymbolPanelDelegate?

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    private static let portraitColumns = 5
    private static let landscapeColumns = 8
    private static let selectedPillFill = UIColor {
        $0.userInterfaceStyle == .dark
            ? UIColor(white: 1, alpha: 0.16)
            : UIColor(white: 0, alpha: 0.09)
    }

    private let layout = UICollectionViewFlowLayout()
    private lazy var collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)

    private let bottomBar = UIView()
    private let bottomTopLine = UIView()
    private let backButton = UIButton(type: .system)
    private let leftSep = UIView()
    private let categoryScroll = TouchCancellingScrollView()
    private var categoryButtons: [CategoryTabButton] = []
    private let rightSep = UIView()
    private let lockPill = UIView()
    private let lockButton = UIButton(type: .system)
    private let backspaceButton = UIButton(type: .system)

    private var state = State()
    private var items: [SymbolItem] = SymbolCatalog.items(for: .common)
    private var slots: [SymbolGridSlot] = []
    private var packedWidth: CGFloat = 0
    private var packedLandscape = false
    private var repeatTimer: Timer?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = KeyboardTheme.background

        layout.minimumInteritemSpacing = 0
        layout.minimumLineSpacing = 0
        collectionView.backgroundColor = .clear
        collectionView.dataSource = self
        collectionView.delegate = self
        collectionView.alwaysBounceVertical = true
        collectionView.delaysContentTouches = false
        collectionView.hideEdgeEffects()
        collectionView.register(SymbolGridCell.self, forCellWithReuseIdentifier: SymbolGridCell.reuseIdentifier)
        addSubview(collectionView)

        bottomBar.backgroundColor = KeyboardTheme.background
        addSubview(bottomBar)

        bottomTopLine.backgroundColor = KeyboardTheme.separator
        bottomBar.addSubview(bottomTopLine)

        backButton.setTitle("返回", for: .normal)
        backButton.tintColor = KeyboardTheme.keyText
        backButton.accessibilityLabel = "返回"
        backButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.delegate?.symbolPanelDidTouchDown(self)
            self.delegate?.symbolPanelDidTapBack(self)
        }, for: .touchUpInside)
        bottomBar.addSubview(backButton)

        leftSep.backgroundColor = KeyboardTheme.separator
        bottomBar.addSubview(leftSep)

        categoryScroll.showsHorizontalScrollIndicator = false
        categoryScroll.showsVerticalScrollIndicator = false
        categoryScroll.alwaysBounceHorizontal = true
        categoryScroll.canCancelContentTouches = true
        categoryScroll.delaysContentTouches = false
        categoryScroll.hideEdgeEffects()
        bottomBar.addSubview(categoryScroll)

        for category in SymbolCategory.allCases {
            let btn = CategoryTabButton(title: category.title)
            btn.accessibilityLabel = "\(category.title)符号"
            btn.addAction(UIAction { [weak self] _ in
                guard let self else { return }
                self.delegate?.symbolPanelDidTouchDown(self)
                self.delegate?.symbolPanel(self, didSelectCategory: category)
            }, for: .touchUpInside)
            categoryScroll.addSubview(btn)
            categoryButtons.append(btn)
        }

        rightSep.backgroundColor = KeyboardTheme.separator
        bottomBar.addSubview(rightSep)

        lockPill.isUserInteractionEnabled = false
        lockPill.layer.cornerRadius = 8
        lockPill.layer.cornerCurve = .continuous
        bottomBar.addSubview(lockPill)

        lockButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.delegate?.symbolPanelDidTouchDown(self)
            self.delegate?.symbolPanelDidToggleLock(self)
        }, for: .touchUpInside)
        bottomBar.addSubview(lockButton)

        backspaceButton.accessibilityLabel = "删除"
        backspaceButton.tintColor = KeyboardTheme.keyText
        backspaceButton.addTarget(self, action: #selector(backspaceTouchDown), for: .touchDown)
        backspaceButton.addTarget(self, action: #selector(backspaceTouchEnd), for: [.touchUpInside, .touchUpOutside, .touchCancel])
        bottomBar.addSubview(backspaceButton)

        apply(state, force: true)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    override var isHidden: Bool {
        didSet {
            if isHidden { stopRepeating() }
        }
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { stopRepeating() }
    }

    func apply(_ newState: State, force: Bool = false) {
        let categoryChanged = force || newState.category != state.category
        let landscapeChanged = force || newState.landscape != state.landscape
        state = newState

        if categoryChanged {
            items = SymbolCatalog.items(for: newState.category)
        }
        let needsInitialPack = slots.isEmpty && floor(collectionView.bounds.width) > 0
        if categoryChanged || landscapeChanged || needsInitialPack {
            pack(force: true)
            collectionView.reloadData()
            if categoryChanged {
                collectionView.setContentOffset(.zero, animated: false)
            }
            setNeedsLayout()
        }

        updateBottomBarAppearance(scrollToSelected: categoryChanged)
    }

    private func updateBottomBarAppearance(scrollToSelected: Bool) {
        let compact = state.landscape
        backButton.titleLabel?.font = .systemFont(ofSize: compact ? 13.5 : 15, weight: .semibold)
        backButton.setTitleColor(KeyboardTheme.keyText, for: .normal)

        let iconPt: CGFloat = compact ? 14 : 15.5
        let lockIcon = state.isLocked ? "lock.fill" : "lock.open"
        lockButton.setImage(
            UIImage(systemName: lockIcon, withConfiguration: UIImage.SymbolConfiguration(pointSize: iconPt, weight: .medium)),
            for: .normal
        )
        lockButton.tintColor = state.isLocked ? KeyboardTheme.pinnedText : KeyboardTheme.keyText
        lockPill.backgroundColor = state.isLocked ? KeyboardTheme.pinnedText.withAlphaComponent(0.15) : .clear
        lockButton.accessibilityLabel = "锁定符号面板"
        lockButton.accessibilityValue = state.isLocked ? "已锁定" : "未锁定"
        lockButton.accessibilityTraits = state.isLocked ? [.button, .selected] : [.button]

        backspaceButton.setImage(
            UIImage(systemName: "delete.left", withConfiguration: UIImage.SymbolConfiguration(pointSize: compact ? 15 : 17, weight: .regular)),
            for: .normal
        )
        backspaceButton.tintColor = KeyboardTheme.keyText

        let allCategories = SymbolCategory.allCases
        for (idx, btn) in categoryButtons.enumerated() where idx < allCategories.count {
            let selected = allCategories[idx] == state.category
            btn.configure(selected: selected, compact: compact, selectedFill: Self.selectedPillFill)
        }

        if scrollToSelected {
            layoutIfNeeded()
            scrollSelectedCategoryIntoView(animated: window != nil)
        }
    }

    private func scrollSelectedCategoryIntoView(animated: Bool) {
        guard let idx = SymbolCategory.allCases.firstIndex(of: state.category),
              idx < categoryButtons.count,
              categoryScroll.bounds.width > 0 else { return }
        let target = categoryButtons[idx].frame.insetBy(dx: -16, dy: 0)
        categoryScroll.scrollRectToVisible(target, animated: animated)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let compact = state.landscape
        let bottomH: CGFloat = compact ? 36 : 44
        let gridH = max(0, bounds.height - bottomH)
        let contentX = horizontalInset
        let contentW = max(0, bounds.width - 2 * horizontalInset)

        let gridFrame = CGRect(x: contentX, y: 0, width: contentW, height: gridH)
        if collectionView.frame != gridFrame {
            let widthChanged = floor(collectionView.bounds.width) != floor(gridFrame.width)
            collectionView.frame = gridFrame
            if widthChanged {
                pack(force: true)
                collectionView.reloadData()
            } else {
                layout.invalidateLayout()
            }
        }

        bottomBar.frame = CGRect(x: contentX, y: gridH, width: contentW, height: bottomH)
        let hairline = 1 / max(1, traitCollection.displayScale)
        bottomTopLine.frame = CGRect(x: 0, y: 0, width: contentW, height: hairline)

        let backW: CGFloat = compact ? 48 : 54
        let lockW: CGFloat = compact ? 34 : 38
        let deleteW: CGFloat = compact ? 38 : 42

        backButton.frame = CGRect(x: 2, y: 0, width: backW, height: bottomH)
        leftSep.frame = CGRect(x: backButton.frame.maxX, y: 9, width: hairline, height: max(0, bottomH - 18))

        backspaceButton.frame = CGRect(x: contentW - deleteW - 2, y: 0, width: deleteW, height: bottomH)
        lockButton.frame = CGRect(x: backspaceButton.frame.minX - lockW, y: 0, width: lockW, height: bottomH)
        let lockPillH: CGFloat = compact ? 26 : 30
        let lockPillW: CGFloat = compact ? 28 : 32
        lockPill.frame = CGRect(
            x: lockButton.frame.midX - lockPillW / 2,
            y: (bottomH - lockPillH) / 2,
            width: lockPillW,
            height: lockPillH
        )
        rightSep.frame = CGRect(x: lockButton.frame.minX - hairline, y: 9, width: hairline, height: max(0, bottomH - 18))

        let scrollX = leftSep.frame.maxX + 2
        let scrollW = max(0, rightSep.frame.minX - 2 - scrollX)
        let scrollChanged = categoryScroll.frame.width != scrollW
        categoryScroll.frame = CGRect(x: scrollX, y: 0, width: scrollW, height: bottomH)

        let pillH: CGFloat = compact ? 26 : 30
        var cursorX: CGFloat = 4
        let allCategories = SymbolCategory.allCases
        for (idx, btn) in categoryButtons.enumerated() where idx < allCategories.count {
            let font = UIFont.systemFont(ofSize: compact ? 13 : 14.5, weight: .semibold)
            let textW = ceil((allCategories[idx].title as NSString).size(withAttributes: [.font: font]).width)
            let w = textW + (compact ? 16 : 18)
            btn.pillHeight = pillH
            btn.frame = CGRect(x: cursorX, y: 0, width: w, height: bottomH)
            cursorX += w + 3
        }
        categoryScroll.contentSize = CGSize(width: max(scrollW + 1, cursorX + 2), height: bottomH)
        if scrollChanged {
            scrollSelectedCategoryIntoView(animated: false)
        }
    }

    private func pack(force: Bool = false) {
        let available = floor(collectionView.bounds.width)
        guard force || available != packedWidth || state.landscape != packedLandscape else { return }
        packedWidth = available
        packedLandscape = state.landscape
        guard available > 0 else {
            slots = []
            return
        }
        let cols = state.landscape ? Self.landscapeColumns : Self.portraitColumns
        let needed = items.map { SymbolGridCell.neededWidth(for: $0.text, landscape: state.landscape) }
        slots = SymbolGridPacking.pack(items: items, neededWidths: needed, availableWidth: available, columns: cols)
    }

    private var rowHeight: CGFloat { state.landscape ? 38 : 45 }

    // MARK: - 退格连删

    @objc private func backspaceTouchDown() {
        delegate?.symbolPanelDidTouchDown(self)
        delegate?.symbolPanelDidTriggerBackspace(self)
        stopRepeating()
        repeatTimer = Timer.scheduledTimer(withTimeInterval: KeyboardView.repeatDelay, repeats: false) { [weak self] _ in
            guard let self else { return }
            self.repeatTimer = Timer.scheduledTimer(withTimeInterval: KeyboardView.repeatInterval, repeats: true) { [weak self] _ in
                guard let self else { return }
                self.delegate?.symbolPanelDidTriggerBackspace(self)
            }
        }
    }

    @objc private func backspaceTouchEnd() {
        stopRepeating()
    }

    private func stopRepeating() {
        repeatTimer?.invalidate()
        repeatTimer = nil
    }

    // MARK: - UICollectionView

    func collectionView(_ collectionView: UICollectionView, numberOfItemsInSection section: Int) -> Int {
        slots.count
    }

    func collectionView(_ collectionView: UICollectionView, cellForItemAt indexPath: IndexPath) -> UICollectionViewCell {
        let cell = collectionView.dequeueReusableCell(withReuseIdentifier: SymbolGridCell.reuseIdentifier, for: indexPath)
        if let symbolCell = cell as? SymbolGridCell, indexPath.item < slots.count {
            symbolCell.configure(slots[indexPath.item], landscape: state.landscape)
        }
        return cell
    }

    func collectionView(_ collectionView: UICollectionView, layout collectionViewLayout: UICollectionViewLayout,
                        sizeForItemAt indexPath: IndexPath) -> CGSize {
        if floor(collectionView.bounds.width) != packedWidth || state.landscape != packedLandscape {
            pack(force: true)
        }
        let width = indexPath.item < slots.count ? slots[indexPath.item].width : max(1, floor(collectionView.bounds.width))
        return CGSize(width: max(1, width), height: rowHeight)
    }

    func collectionView(_ collectionView: UICollectionView, shouldHighlightItemAt indexPath: IndexPath) -> Bool {
        guard indexPath.item < slots.count else { return false }
        return slots[indexPath.item].item != nil
    }

    func collectionView(_ collectionView: UICollectionView, didHighlightItemAt indexPath: IndexPath) {
        delegate?.symbolPanelDidTouchDown(self)
    }

    func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        guard indexPath.item < slots.count, let item = slots[indexPath.item].item else { return }
        delegate?.symbolPanel(self, didSelectSymbol: item.text)
    }
}

// MARK: - 分类胶囊按钮

/// 触控高度铺满整条底栏（44pt），视觉胶囊居中显示；配合 TouchCancellingScrollView 既好点按又能从按钮上直接起手横滑
private final class CategoryTabButton: UIControl {
    private let pill = UIView()
    private let titleLabel = UILabel()
    var pillHeight: CGFloat = 30 {
        didSet { if pillHeight != oldValue { setNeedsLayout() } }
    }

    init(title: String) {
        super.init(frame: .zero)
        isAccessibilityElement = true
        pill.isUserInteractionEnabled = false
        pill.layer.cornerRadius = 8
        pill.layer.cornerCurve = .continuous
        addSubview(pill)

        titleLabel.text = title
        titleLabel.textAlignment = .center
        titleLabel.isUserInteractionEnabled = false
        addSubview(titleLabel)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(selected: Bool, compact: Bool, selectedFill: UIColor) {
        titleLabel.font = .systemFont(ofSize: compact ? 13 : 14.5, weight: selected ? .semibold : .regular)
        titleLabel.textColor = selected ? KeyboardTheme.keyText : KeyboardTheme.secondaryText
        pill.backgroundColor = selected ? selectedFill : .clear
        accessibilityTraits = selected ? [.button, .selected] : [.button]
    }

    override var isHighlighted: Bool {
        didSet {
            alpha = isHighlighted ? 0.65 : 1.0
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let y = floor((bounds.height - pillHeight) / 2)
        pill.frame = CGRect(x: 0, y: y, width: bounds.width, height: pillHeight)
        titleLabel.frame = bounds
    }
}

// MARK: - 符号单元格

private final class SymbolGridCell: UICollectionViewCell {
    static let reuseIdentifier = "SymbolGridCell"

    private let label = UILabel()
    private let badgeLabel = UILabel()
    private let bottomDivider = UIView()
    private let rightDivider = UIView()
    private var isRowEnd = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        label.textAlignment = .center
        label.textColor = KeyboardTheme.keyText
        label.adjustsFontSizeToFitWidth = true
        label.minimumScaleFactor = 0.65
        label.baselineAdjustment = .alignCenters
        contentView.addSubview(label)

        badgeLabel.text = "全"
        badgeLabel.textAlignment = .right
        badgeLabel.textColor = KeyboardTheme.keyHintText
        badgeLabel.font = .systemFont(ofSize: 9.5, weight: .regular)
        contentView.addSubview(badgeLabel)

        bottomDivider.backgroundColor = KeyboardTheme.separator
        contentView.addSubview(bottomDivider)

        rightDivider.backgroundColor = KeyboardTheme.separator
        contentView.addSubview(rightDivider)

        selectedBackgroundView = nil
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    static func font(for text: String, landscape: Bool) -> UIFont {
        let count = text.count
        let size: CGFloat
        if count <= 1 {
            size = landscape ? 18 : 20.5
        } else if count <= 3 {
            size = landscape ? 15 : 16.5
        } else {
            size = landscape ? 13 : 14.5
        }
        return KeyTypography.font(ofSize: size, weight: .regular, for: text)
    }

    static func neededWidth(for text: String, landscape: Bool) -> CGFloat {
        // 1～2 个字符的符号（含 ……、——、℃、㎡、kHz）固定占 1 列，只有长网址与颜文字按实际宽度跨列
        if text.count <= 3 && !text.contains("(") && !text.contains("¯") {
            return 44
        }
        let font = Self.font(for: text, landscape: landscape)
        let width = ceil((text as NSString).size(withAttributes: [.font: font]).width)
        return max(44, width + 22)
    }

    func configure(_ slot: SymbolGridSlot, landscape: Bool) {
        isRowEnd = slot.isRowEnd
        rightDivider.isHidden = slot.isRowEnd

        if let item = slot.item {
            isUserInteractionEnabled = true
            isAccessibilityElement = true
            accessibilityTraits = .keyboardKey
            accessibilityLabel = item.text
            accessibilityValue = item.showsFullWidthBadge ? "全角" : nil
            label.text = item.text
            label.font = Self.font(for: item.text, landscape: landscape)
            label.isHidden = false
            badgeLabel.isHidden = !item.showsFullWidthBadge
        } else {
            isUserInteractionEnabled = false
            isAccessibilityElement = false
            accessibilityLabel = nil
            accessibilityValue = nil
            label.text = nil
            label.isHidden = true
            badgeLabel.isHidden = true
            contentView.backgroundColor = .clear
        }
    }

    override var isHighlighted: Bool {
        didSet {
            contentView.backgroundColor = isHighlighted ? KeyboardTheme.candidatePressed : .clear
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let b = contentView.bounds
        label.frame = b.insetBy(dx: 4, dy: 2)
        badgeLabel.frame = CGRect(x: max(0, b.width - 16), y: 2.5, width: 12.5, height: 12)
        let hairline = 1 / max(1, traitCollection.displayScale)
        bottomDivider.frame = CGRect(x: 0, y: b.maxY - hairline, width: b.width, height: hairline)
        rightDivider.frame = CGRect(x: b.maxX - hairline, y: 0, width: hairline, height: b.height)
    }
}
