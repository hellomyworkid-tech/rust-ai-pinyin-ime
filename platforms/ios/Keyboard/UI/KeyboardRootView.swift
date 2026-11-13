import UIKit

/// 键盘的根视图。用普通 UIView 而不是 UIInputView：iOS 18.4–27 上 UIInputView 有引用环，
/// 每次显示 / 收起都会泄漏整棵视图树。
///
/// 从上到下：音节栏 + 候选栏（输入中）或工具条（空闲），下面是按键区；展开候选时网格盖住按键区。
/// 子视图全部手动排布，首帧快；只有决定键盘高度的那个隐形视图用 Auto Layout（见 updateHeight）。
final class KeyboardRootView: UIView, UIInputViewAudioFeedback {
    let strip = CompositionStripView()
    let bar = CandidateBarView()
    let toolbar = IdleToolbarView()
    let keyboard = KeyboardView()
    let grid = CandidateGridView()
    let preview = KeyPreviewView()
    /// 撑出键盘高度的隐形视图
    private let heightSpacer = UIView()
    private let heightConstraint: NSLayoutConstraint

    var isComposing = false {
        didSet {
            guard isComposing != oldValue else { return }
            updateVisibility()
        }
    }

    var isGridExpanded = false {
        didSet {
            guard isGridExpanded != oldValue else { return }
            bar.setExpanded(isGridExpanded)
            updateVisibility()
        }
    }

    var landscape = false {
        didSet { if landscape != oldValue { setNeedsLayout() } }
    }

    // MARK: - 尺寸

    /// 顶部（空闲时的工具栏，打字时的拼音条 + 候选栏）高度固定，按键区拿剩下的全部高度
    static func topHeight(landscape: Bool) -> CGFloat { landscape ? 40 : 50 }
    static func stripHeight(landscape: Bool) -> CGFloat { landscape ? 15 : 18 }

    /// 一行键的高度（含上下行距），和系统键盘一样：竖屏 54，宽 414 以上的大屏手机（Plus、Max）56，横屏 40。
    /// 2026-11-12 在 iPhone 18 Pro 模拟器（iOS 27）上量过系统拼音键盘：竖屏行高 54、键高 42，横屏行高 40、键高 32
    static func rowPitch(landscape: Bool, width: CGFloat) -> CGFloat {
        if landscape { return 40 }
        return width >= 414 ? 56 : 54
    }

    /// 键盘总高度：顶部加四行键。竖屏 266（大屏 274），比系统键盘高 8pt 左右，多出来的是拼音条
    static func preferredHeight(landscape: Bool, width: CGFloat) -> CGFloat {
        topHeight(landscape: landscape) + 4 * rowPitch(landscape: landscape, width: width)
    }

    override init(frame: CGRect) {
        heightConstraint = heightSpacer.heightAnchor.constraint(
            equalToConstant: Self.preferredHeight(landscape: false, width: frame.width))
        super.init(frame: frame)
        backgroundColor = KeyboardTheme.background
        for view in [toolbar, strip, bar, keyboard, grid, preview] as [UIView] {
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

    /// 按横竖屏和键盘宽度更新想要的高度（控制器在 viewWillLayoutSubviews 里调用）
    func updateHeight(landscape: Bool, width: CGFloat) {
        let height = Self.preferredHeight(landscape: landscape, width: width)
        if heightConstraint.constant != height { heightConstraint.constant = height }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    /// 打开按键音（UIDevice.playInputClick 需要）
    var enableInputClicksWhenVisible: Bool { true }

    override func layoutSubviews() {
        super.layoutSubviews()
        let inset = landscape ? max(safeAreaInsets.left, safeAreaInsets.right) : 0
        let top = Self.topHeight(landscape: landscape)
        let stripHeight = Self.stripHeight(landscape: landscape)
        let width = bounds.width

        toolbar.frame = CGRect(x: 0, y: 0, width: width, height: top)
        strip.frame = CGRect(x: 0, y: 1, width: width, height: stripHeight)
        bar.frame = CGRect(x: 0, y: stripHeight + 1, width: width, height: top - stripHeight - 1)
        let keysFrame = CGRect(x: 0, y: top, width: width, height: max(0, bounds.height - top))
        keyboard.frame = keysFrame
        grid.frame = keysFrame
        toolbar.horizontalInset = inset
        strip.horizontalInset = inset
        bar.horizontalInset = inset
        grid.horizontalInset = inset
        keyboard.horizontalInset = inset
    }

    private func updateVisibility() {
        toolbar.isHidden = isComposing
        strip.isHidden = !isComposing
        bar.isHidden = !isComposing
        grid.isHidden = !(isComposing && isGridExpanded)
        keyboard.isHidden = isComposing && isGridExpanded
        if keyboard.isHidden { preview.hide() }
    }
}
