import UIKit

/// 一个键的外观。不接收触摸，触摸统一由 KeyboardView 处理（键缝里也能按到）。
final class KeyView: UIView {
    /// 键上显示什么
    struct Face: Equatable {
        var title: String?
        var symbolName: String?
        var fontSize: CGFloat
        var isFunction: Bool
        /// 回车键在发送、搜索等场景下用强调色
        var isProminent = false
        /// 上档键锁定 / 单次大写、当前语言等「开启」状态，用字母键底色
        var isOn = false
    }

    private let label = UILabel()
    private let icon = UIImageView()
    private(set) var face: Face?

    var isPressed = false {
        didSet { if isPressed != oldValue { updateColors() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = false
        isAccessibilityElement = true
        accessibilityTraits = .keyboardKey
        layer.cornerRadius = 6
        layer.cornerCurve = .continuous
        layer.shadowOffset = CGSize(width: 0, height: 1)
        layer.shadowRadius = 0
        layer.shadowOpacity = 1

        label.textAlignment = .center
        label.adjustsFontSizeToFitWidth = true
        label.minimumScaleFactor = 0.6
        label.baselineAdjustment = .alignCenters
        icon.contentMode = .center
        addSubview(label)
        addSubview(icon)

        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: KeyView, _: UITraitCollection) in
            view.updateColors()
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(_ newFace: Face, accessibilityLabel: String) {
        self.accessibilityLabel = accessibilityLabel
        guard newFace != face else { return }
        let fontChanged = newFace.fontSize != face?.fontSize || newFace.symbolName != face?.symbolName
        face = newFace
        label.text = newFace.title
        label.isHidden = newFace.title == nil
        label.font = .systemFont(ofSize: newFace.fontSize, weight: .regular)
        if let name = newFace.symbolName {
            let config = UIImage.SymbolConfiguration(pointSize: newFace.fontSize, weight: .regular)
            icon.image = UIImage(systemName: name, withConfiguration: config)
            icon.isHidden = false
        } else {
            icon.image = nil
            icon.isHidden = true
        }
        if fontChanged { setNeedsLayout() }
        updateColors()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let inset = bounds.insetBy(dx: 2, dy: 0)
        label.frame = inset
        icon.frame = bounds
        layer.shadowPath = UIBezierPath(roundedRect: bounds, cornerRadius: layer.cornerRadius).cgPath
    }

    private func updateColors() {
        guard let face else { return }
        let background: UIColor
        let foreground: UIColor
        if face.isProminent && !isPressed {
            background = KeyboardTheme.accent
            foreground = KeyboardTheme.accentText
        } else if face.isFunction {
            background = (isPressed || face.isOn) ? KeyboardTheme.functionKeyPressed : KeyboardTheme.functionKey
            foreground = KeyboardTheme.keyText
        } else {
            background = isPressed ? KeyboardTheme.letterKeyPressed : KeyboardTheme.letterKey
            foreground = KeyboardTheme.keyText
        }
        backgroundColor = background
        label.textColor = foreground
        icon.tintColor = foreground
        layer.shadowColor = KeyboardTheme.keyShadow.resolvedColor(with: traitCollection).cgColor
    }
}
