import UIKit

/// 键盘的根视图。用普通 UIView 而不是 UIInputView：iOS 18.4–27 上 UIInputView 有引用环，
/// 每次显示 / 收起都会泄漏整棵视图树。
///
/// 从上到下：音节栏 + 候选栏（输入中）或工具条（空闲），下面是按键区；展开候选时网格盖住按键区。
/// 子视图全部手动排布，首帧快；只有决定键盘高度的那个隐形视图用 Auto Layout（见 updateHeight）。
final class KeyboardRootView: UIView, UIInputViewAudioFeedback {
    let toolbar = IdleToolbarView()
    let keyboard = KeyboardView()
    let preview = KeyPreviewView()

    /// 音节栏、候选栏、展开候选网格、符号面板、快捷设置面板均按需懒加载：
    /// 冷启动唤起键盘时固定处于空闲态（只显示工具栏 + 字母按键区），首帧不创建任何 UIScrollView / UICollectionView / SwiftUI 视图树。
    private(set) var stripIfLoaded: CompositionStripView?
    private(set) var barIfLoaded: CandidateBarView?
    private(set) var gridIfLoaded: CandidateGridView?
    private(set) var symbolPanelIfLoaded: SymbolPanelView?
    private(set) var settingsPanelIfLoaded: QuickSettingsPanelView?

    var onCreateStrip: ((CompositionStripView) -> Void)?
    var onCreateBar: ((CandidateBarView) -> Void)?
    var onCreateGrid: ((CandidateGridView) -> Void)?
    var onCreateSymbolPanel: ((SymbolPanelView) -> Void)?
    var onCreateSettingsPanel: ((QuickSettingsPanelView) -> Void)?

    var strip: CompositionStripView {
        if let existing = stripIfLoaded { return existing }
        let created = CompositionStripView()
        stripIfLoaded = created
        insertSubview(created, belowSubview: preview)
        onCreateStrip?(created)
        setNeedsLayout()
        return created
    }

    var bar: CandidateBarView {
        if let existing = barIfLoaded { return existing }
        let created = CandidateBarView()
        barIfLoaded = created
        created.setExpanded(isGridExpanded)
        insertSubview(created, belowSubview: preview)
        onCreateBar?(created)
        setNeedsLayout()
        return created
    }

    var grid: CandidateGridView {
        if let existing = gridIfLoaded { return existing }
        let created = CandidateGridView()
        gridIfLoaded = created
        insertSubview(created, belowSubview: preview)
        onCreateGrid?(created)
        setNeedsLayout()
        return created
    }

    var symbolPanel: SymbolPanelView {
        if let existing = symbolPanelIfLoaded { return existing }
        let created = SymbolPanelView()
        symbolPanelIfLoaded = created
        insertSubview(created, belowSubview: preview)
        onCreateSymbolPanel?(created)
        setNeedsLayout()
        return created
    }

    var settingsPanel: QuickSettingsPanelView {
        if let existing = settingsPanelIfLoaded { return existing }
        let created = QuickSettingsPanelView()
        settingsPanelIfLoaded = created
        insertSubview(created, belowSubview: preview)
        onCreateSettingsPanel?(created)
        setNeedsLayout()
        return created
    }

    /// 撑出键盘高度的隐形视图
    private let heightSpacer = UIView()
    private let heightConstraint: NSLayoutConstraint

    var isComposing = false {
        didSet {
            guard isComposing != oldValue else { return }
            updateVisibility()
            setNeedsLayout()
        }
    }

    var isGridExpanded = false {
        didSet {
            guard isGridExpanded != oldValue else { return }
            barIfLoaded?.setExpanded(isGridExpanded)
            updateVisibility()
        }
    }

    var isSettingsExpanded = false {
        didSet {
            guard isSettingsExpanded != oldValue else { return }
            updateVisibility()
        }
    }

    var isSymbolPanelVisible = false {
        didSet {
            guard isSymbolPanelVisible != oldValue else { return }
            updateVisibility()
            setNeedsLayout()
        }
    }

    var landscape = false {
        didSet { if landscape != oldValue { setNeedsLayout() } }
    }

    var unfolded = false {
        didSet { if unfolded != oldValue { setNeedsLayout() } }
    }

    // MARK: - 尺寸

    /// 顶部（空闲时的工具栏，打字时的拼音条 + 候选栏）高度固定，按键区拿剩下的全部高度
    static func topHeight(landscape: Bool) -> CGFloat { landscape ? 40 : 50 }
    static func stripHeight(landscape: Bool) -> CGFloat { landscape ? 15 : 18 }

    /// 一行键的高度（含上下行距），和系统键盘一样：竖屏 54，宽 414 以上的大屏手机（Plus、Max）56，横屏 40；
    /// iPhone Duo 展开内屏（951×669 / 669×951）取 52（键高 42pt，总高 258pt，兼顾双手敲击高度与内屏视野）。
    static func rowPitch(landscape: Bool, unfolded: Bool = false, width: CGFloat) -> CGFloat {
        if landscape { return 40 }
        if unfolded { return 52 }
        return width >= 414 ? 56 : 54
    }

    /// 键盘总高度：顶部加四行键。竖屏 266（大屏 274，展开内屏 258），比系统键盘高 8pt 左右，多出来的是拼音条
    static func preferredHeight(landscape: Bool, unfolded: Bool = false, width: CGFloat) -> CGFloat {
        topHeight(landscape: landscape) + 4 * rowPitch(landscape: landscape, unfolded: unfolded, width: width)
    }

    override init(frame: CGRect) {
        heightConstraint = heightSpacer.heightAnchor.constraint(
            equalToConstant: Self.preferredHeight(landscape: false, width: frame.width))
        super.init(frame: frame)
        backgroundColor = KeyboardTheme.background
        for view in [toolbar, keyboard, preview] as [UIView] {
            addSubview(view)
        }
        keyboard.preview = preview
        addHeightSpacer()
        updateVisibility()
    }

    /// 键盘高度。iOS 27 不理会挂在根视图自己身上的高度约束（2026-11-12 试过 9 种：优先级 750 / 999 / required，
    /// 不同时机加或改，系统的 UIInputView 做根视图，allowsSelfSizing，高度都停在系统默认的 216 / 224，
    /// 比系统键盘矮约 45pt），却会按子视图的约束算出键盘该多高。仓输入法（和元书同一个开发者）就是让内容的约束
    /// 撑出高度。所以这里挂一个隐形子视图，上下贴住根视图，高度约束挂在它身上。
    /// 优先级 999：系统真要别的高度（比如转屏的过程中）时让系统说了算，不报约束冲突
    private func addHeightSpacer() {
        heightSpacer.isHidden = true
        heightSpacer.isUserInteractionEnabled = false
        heightSpacer.translatesAutoresizingMaskIntoConstraints = false
        addSubview(heightSpacer)
        heightConstraint.priority = UILayoutPriority(999)
        NSLayoutConstraint.activate([
            heightSpacer.topAnchor.constraint(equalTo: topAnchor),
            heightSpacer.bottomAnchor.constraint(equalTo: bottomAnchor),
            heightSpacer.leadingAnchor.constraint(equalTo: leadingAnchor),
            heightSpacer.widthAnchor.constraint(equalToConstant: 0),
            heightConstraint,
        ])
    }

    /// 按横竖屏、折叠展开态和键盘宽度更新想要的高度（控制器在 viewWillLayoutSubviews 里调用）
    func updateHeight(landscape: Bool, unfolded: Bool = false, width: CGFloat) {
        let height = Self.preferredHeight(landscape: landscape, unfolded: unfolded, width: width)
        if heightConstraint.constant != height { heightConstraint.constant = height }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    /// 打开按键音（UIDevice.playInputClick 需要）
    var enableInputClicksWhenVisible: Bool { true }

    override func layoutSubviews() {
        super.layoutSubviews()
        let barInset = (landscape || unfolded) ? max(safeAreaInsets.left, safeAreaInsets.right) : 0
        let top = Self.topHeight(landscape: landscape)
        let stripHeight = Self.stripHeight(landscape: landscape)
        let width = bounds.width

        toolbar.frame = CGRect(x: 0, y: 0, width: width, height: top)
        stripIfLoaded?.frame = CGRect(x: 0, y: 1, width: width, height: stripHeight)
        barIfLoaded?.frame = CGRect(x: 0, y: stripHeight + 1, width: width, height: top - stripHeight - 1)
        let keysFrame = CGRect(x: 0, y: top, width: width, height: max(0, bounds.height - top))
        keyboard.frame = keysFrame
        gridIfLoaded?.frame = keysFrame
        symbolPanelIfLoaded?.frame = isComposing ? keysFrame : bounds
        settingsPanelIfLoaded?.frame = keysFrame
        toolbar.horizontalInset = barInset
        stripIfLoaded?.horizontalInset = barInset
        barIfLoaded?.horizontalInset = barInset
        gridIfLoaded?.horizontalInset = barInset
        symbolPanelIfLoaded?.horizontalInset = barInset
        settingsPanelIfLoaded?.horizontalInset = barInset
        if unfolded && !landscape {
            keyboard.horizontalInset = 0
            keyboard.leftSafeInset = safeAreaInsets.left
            keyboard.rightSafeInset = safeAreaInsets.right
        } else {
            keyboard.horizontalInset = barInset
            keyboard.leftSafeInset = safeAreaInsets.left
            keyboard.rightSafeInset = safeAreaInsets.right
        }
    }

    private func updateVisibility() {
        let showGrid = isComposing && isGridExpanded
        let showSettings = !isComposing && isSettingsExpanded
        let showSymbols = isSymbolPanelVisible && !showSettings && !showGrid
        toolbar.isHidden = isComposing || showSymbols
        if isComposing {
            _ = strip
            _ = bar
        }
        stripIfLoaded?.isHidden = !isComposing
        barIfLoaded?.isHidden = !isComposing
        if showGrid { _ = grid }
        if showSymbols { _ = symbolPanel }
        if showSettings { _ = settingsPanel }
        gridIfLoaded?.isHidden = !showGrid
        symbolPanelIfLoaded?.isHidden = !showSymbols
        settingsPanelIfLoaded?.isHidden = !showSettings
        keyboard.isHidden = showGrid || showSettings || showSymbols
        if keyboard.isHidden { preview.hide() }
    }

    /// 内存紧张时释放当前不可见的重型面板（UICollectionView 与 SwiftUI 宿主），降低扩展常驻内存
    func releaseHiddenPanels() {
        if let existing = gridIfLoaded, existing.isHidden {
            existing.removeFromSuperview()
            gridIfLoaded = nil
        }
        if let existing = symbolPanelIfLoaded, existing.isHidden {
            existing.removeFromSuperview()
            symbolPanelIfLoaded = nil
        }
        if let existing = settingsPanelIfLoaded, existing.isHidden {
            existing.removeLinkHosts()
            existing.removeFromSuperview()
            settingsPanelIfLoaded = nil
        }
    }
}
