import UIKit

/// 没在输入时候选栏位置上的工具条：
/// - 默认：左边是快捷设置按钮 + 状态文字，右边是收起键盘；
/// - 快捷设置面板展开时：左边变为返回 `<`，中间显示面板标题或提示，右边变为关闭 `⊗`（同元输入法两级防误触交互）。
final class IdleToolbarView: UIView {
    private let settingsButton = SafeHitButton(type: .system)
    private let statusLabel = UILabel()
    private let splitButton = SafeHitButton(type: .system)
    private let dismissButton = UIButton(type: .system)

    var onToggleSettings: (() -> Void)?
    var onToggleSplit: (() -> Void)?
    var onDismiss: (() -> Void)?

    private var isPanelOpen = false
    private var showsSplitToggle = false
    private var isSplitActive = false
    private var normalStatus = ""
    private var panelNotice: String?

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)

        settingsButton.tintColor = KeyboardTheme.keyText
        settingsButton.addAction(UIAction { [weak self] _ in self?.onToggleSettings?() }, for: .touchUpInside)
        addSubview(settingsButton)

        statusLabel.font = .systemFont(ofSize: 13, weight: .regular)
        statusLabel.textColor = KeyboardTheme.secondaryText
        statusLabel.adjustsFontSizeToFitWidth = true
        statusLabel.minimumScaleFactor = 0.8
        addSubview(statusLabel)

        splitButton.addAction(UIAction { [weak self] _ in self?.onToggleSplit?() }, for: .touchUpInside)
        addSubview(splitButton)

        dismissButton.tintColor = KeyboardTheme.keyText
        dismissButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            if self.isPanelOpen {
                self.onToggleSettings?()
            } else {
                self.onDismiss?()
            }
        }, for: .touchUpInside)
        addSubview(dismissButton)

        applyAppearance()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func setStatus(_ text: String) {
        normalStatus = text
        if !isPanelOpen {
            statusLabel.text = text
        }
    }

    func setSplitControl(visible: Bool, isSplit: Bool) {
        guard visible != showsSplitToggle || isSplit != isSplitActive else { return }
        showsSplitToggle = visible
        isSplitActive = isSplit
        applyAppearance()
    }

    func setPanelOpen(_ open: Bool, notice: String? = nil) {
        guard open != isPanelOpen || notice != panelNotice else {
            if !open { statusLabel.text = normalStatus }
            return
        }
        isPanelOpen = open
        panelNotice = notice
        applyAppearance()
    }

    private func applyAppearance() {
        if isPanelOpen {
            settingsButton.setImage(KeyboardSymbolCache.image(named: "chevron.left", pointSize: 16), for: .normal)
            settingsButton.accessibilityLabel = "返回键盘"

            splitButton.isHidden = true

            dismissButton.setImage(KeyboardSymbolCache.image(named: "xmark.circle", pointSize: 17), for: .normal)
            dismissButton.accessibilityLabel = "关闭设置"

            if let notice = panelNotice, !notice.isEmpty {
                statusLabel.text = notice
                statusLabel.font = .systemFont(ofSize: 12.5, weight: .medium)
                statusLabel.textColor = KeyboardTheme.pinnedText
            } else {
                statusLabel.text = "快捷设置"
                statusLabel.font = .systemFont(ofSize: 13.5, weight: .medium)
                statusLabel.textColor = KeyboardTheme.keyText
            }
        } else {
            settingsButton.setImage(KeyboardSymbolCache.image(named: "gearshape", pointSize: 16), for: .normal)
            settingsButton.accessibilityLabel = "快捷设置"

            splitButton.isHidden = !showsSplitToggle
            if showsSplitToggle {
                let symbol = isSplitActive ? "rectangle.split.2x1.fill" : "rectangle.split.2x1"
                splitButton.setImage(KeyboardSymbolCache.image(named: symbol, pointSize: 15.5), for: .normal)
                splitButton.tintColor = isSplitActive ? KeyboardTheme.pinnedText : KeyboardTheme.keyText
                splitButton.accessibilityLabel = isSplitActive ? "合并键盘" : "分离键盘"
                splitButton.accessibilityValue = isSplitActive ? "当前为两侧分离" : "当前为整排合并"
            }

            dismissButton.setImage(KeyboardSymbolCache.image(named: "keyboard.chevron.compact.down", pointSize: 16), for: .normal)
            dismissButton.accessibilityLabel = "收起键盘"

            statusLabel.text = normalStatus
            statusLabel.font = .systemFont(ofSize: 13, weight: .regular)
            statusLabel.textColor = KeyboardTheme.secondaryText
        }
        setNeedsLayout()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let rightWidth: CGFloat = 48
        let splitWidth: CGFloat = 42
        let leftWidth: CGFloat = 42
        let deadZone: CGFloat = isPanelOpen ? 0 : (bounds.height >= 46 ? 6 : 3)
        // 视觉上与文字、右侧收起按钮严格同高垂直居中；未展开面板时底部留出 6pt 防误触死区，避免敲击正下方首行键时误触
        settingsButton.bottomDeadZone = deadZone
        splitButton.bottomDeadZone = deadZone
        settingsButton.frame = CGRect(
            x: horizontalInset + 4,
            y: 0,
            width: leftWidth,
            height: bounds.height
        )
        dismissButton.frame = CGRect(
            x: bounds.maxX - horizontalInset - rightWidth,
            y: 0,
            width: rightWidth,
            height: bounds.height
        )
        let trailingLimitX: CGFloat
        if !splitButton.isHidden {
            splitButton.frame = CGRect(
                x: dismissButton.frame.minX - splitWidth,
                y: 0,
                width: splitWidth,
                height: bounds.height
            )
            trailingLimitX = splitButton.frame.minX
        } else {
            splitButton.frame = .zero
            trailingLimitX = dismissButton.frame.minX
        }
        let labelX = settingsButton.frame.maxX + 4
        statusLabel.frame = CGRect(
            x: labelX,
            y: 0,
            width: max(0, trailingLimitX - labelX - 8),
            height: bounds.height
        )
    }
}

private final class SafeHitButton: UIButton {
    var bottomDeadZone: CGFloat = 0

    override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
        guard point.y < bounds.height - bottomDeadZone else { return false }
        return super.point(inside: point, with: event)
    }
}
