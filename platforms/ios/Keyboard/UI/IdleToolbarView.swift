import UIKit

/// 没在输入时候选栏位置上的工具条：左边是 AI 选词状态，右边是收起键盘
final class IdleToolbarView: UIView {
    private let statusLabel = UILabel()
    private let dismissButton = UIButton(type: .system)
    var onDismiss: (() -> Void)?

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        statusLabel.font = .systemFont(ofSize: 13, weight: .regular)
        statusLabel.textColor = KeyboardTheme.secondaryText
        statusLabel.adjustsFontSizeToFitWidth = true
        statusLabel.minimumScaleFactor = 0.8
        addSubview(statusLabel)

        let config = UIImage.SymbolConfiguration(pointSize: 16, weight: .regular)
        dismissButton.setImage(UIImage(systemName: "keyboard.chevron.compact.down", withConfiguration: config), for: .normal)
        dismissButton.tintColor = KeyboardTheme.keyText
        dismissButton.accessibilityLabel = "收起键盘"
        dismissButton.addAction(UIAction { [weak self] _ in self?.onDismiss?() }, for: .touchUpInside)
        addSubview(dismissButton)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func setStatus(_ text: String) {
        statusLabel.text = text
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let buttonWidth: CGFloat = 48
        dismissButton.frame = CGRect(x: bounds.maxX - horizontalInset - buttonWidth, y: 0,
                                     width: buttonWidth, height: bounds.height)
        statusLabel.frame = CGRect(x: horizontalInset + 12, y: 0,
                                   width: max(0, dismissButton.frame.minX - horizontalInset - 16), height: bounds.height)
    }
}
