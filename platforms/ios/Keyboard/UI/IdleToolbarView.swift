import UIKit

/// 没在输入时候选栏位置上的工具条：
/// - 默认：左边是快捷设置按钮 + 状态文字，右边是收起键盘；
/// - 快捷设置面板展开时：左边变为返回 `<`，中间显示面板标题或提示，右边变为关闭 `⊗`（同元输入法两级防误触交互）。
final class IdleToolbarView: UIView {
    private let settingsButton = SafeHitButton(type: .system)
    private let statusLabel = UILabel()
    private let dismissButton = UIButton(type: .system)

    var onToggleSettings: (() -> Void)?
    var onDismiss: (() -> Void)?

    private var isPanelOpen = false
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
        let iconConfig = UIImage.SymbolConfiguration(pointSize: 16, weight: .regular)
        if isPanelOpen {
            settingsButton.setImage(UIImage(systemName: "chevron.left", withConfiguration: iconConfig), for: .normal)
            settingsButton.accessibilityLabel = "返回键盘"

            let closeConfig = UIImage.SymbolConfiguration(pointSize: 17, weight: .regular)
            dismissButton.setImage(UIImage(systemName: "xmark.circle", withConfiguration: closeConfig), for: .normal)
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
            settingsButton.setImage(UIImage(systemName: "gearshape", withConfiguration: iconConfig), for: .normal)
            settingsButton.accessibilityLabel = "快捷设置"

            dismissButton.setImage(UIImage(systemName: "keyboard.chevron.compact.down", withConfiguration: iconConfig), for: .normal)
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
        let leftWidth: CGFloat = 42
        // 视觉上与文字、右侧收起按钮严格同高垂直居中；未展开面板时底部留出 6pt 防误触死区，避免敲击正下方 Q 键时误触
        settingsButton.bottomDeadZone = isPanelOpen ? 0 : (bounds.height >= 46 ? 6 : 3)
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
        let labelX = settingsButton.frame.maxX + 4
        statusLabel.frame = CGRect(
            x: labelX,
            y: 0,
            width: max(0, dismissButton.frame.minX - labelX - 8),
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
