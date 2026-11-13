import UIKit

/// 音节栏里的一个音节
final class SyllableChip: UIControl {
    private let label = UILabel()
    private(set) var index = 0
    private var isActive = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        layer.cornerRadius = 4
        layer.cornerCurve = .continuous
        label.textAlignment = .center
        addSubview(label)
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(_ syllable: CompositionModel.Syllable, index: Int, font: UIFont) {
        self.index = index
        isActive = syllable.isActive
        label.font = font
        label.text = syllable.code
        label.textColor = syllable.isActive ? KeyboardTheme.accentText
            : (syllable.isPinned ? KeyboardTheme.pinnedText : KeyboardTheme.keyText)
        accessibilityLabel = "音节 \(syllable.code)" + (syllable.isPinned ? "，已选定" : "")
        // 当前点着的音节标成「已选中」（读屏和 UI 测试据此知道选的是哪个）
        accessibilityTraits = syllable.isActive ? [.button, .selected] : .button
        updateBackground()
    }

    var preferredWidth: CGFloat {
        let text = (label.text ?? "") as NSString
        return ceil(text.size(withAttributes: [.font: label.font as Any]).width) + 8
    }

    override var isHighlighted: Bool {
        didSet { updateBackground() }
    }

    private func updateBackground() {
        if isActive {
            backgroundColor = KeyboardTheme.accent
        } else {
            backgroundColor = isHighlighted ? KeyboardTheme.candidatePressed : .clear
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        label.frame = bounds
    }
}
