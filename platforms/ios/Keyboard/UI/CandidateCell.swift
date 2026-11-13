import UIKit

/// 候选栏里的一个候选。首选加粗并带浅色底；AI 提上来的候选右上角有个小星标。
final class CandidateCell: UIControl {
    static let horizontalPadding: CGFloat = 12
    static let minimumWidth: CGFloat = 40

    private let label = UILabel()
    private let marker = UIImageView()
    private let backdrop = UIView()
    /// 在全部候选里的下标
    var index = 0

    override init(frame: CGRect) {
        super.init(frame: frame)
        backdrop.isUserInteractionEnabled = false
        backdrop.layer.cornerRadius = 6
        backdrop.layer.cornerCurve = .continuous
        addSubview(backdrop)
        label.textAlignment = .center
        label.textColor = KeyboardTheme.keyText
        addSubview(label)
        marker.image = UIImage(systemName: "sparkle",
                               withConfiguration: UIImage.SymbolConfiguration(pointSize: 8, weight: .bold))
        marker.tintColor = KeyboardTheme.aiMarker
        marker.contentMode = .center
        addSubview(marker)
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    static func font(isFirst: Bool, landscape: Bool) -> UIFont {
        .systemFont(ofSize: landscape ? 17 : 20, weight: isFirst ? .semibold : .regular)
    }

    /// 按文字算出这个候选需要的宽度
    static func width(for text: String, isFirst: Bool, landscape: Bool) -> CGFloat {
        let size = (text as NSString).size(withAttributes: [.font: font(isFirst: isFirst, landscape: landscape)])
        return max(minimumWidth, ceil(size.width) + 2 * horizontalPadding)
    }

    func configure(_ candidate: IMECandidate, index: Int, isFirst: Bool, landscape: Bool) {
        self.index = index
        label.text = candidate.text
        label.font = Self.font(isFirst: isFirst, landscape: landscape)
        let boosted = Self.isAIBoosted(candidate)
        marker.isHidden = !boosted
        backdrop.backgroundColor = isFirst ? KeyboardTheme.firstCandidateBackground : .clear
        accessibilityLabel = boosted ? "\(candidate.text)，AI 推荐" : candidate.text
        setNeedsLayout()
    }

    /// AI 提上来的候选：aiBoosted 为真，或标签里写着 AI
    static func isAIBoosted(_ candidate: IMECandidate) -> Bool {
        candidate.aiBoosted || candidate.tag.contains("AI")
    }

    override var isHighlighted: Bool {
        didSet { backgroundColor = isHighlighted ? KeyboardTheme.candidatePressed : .clear }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        layer.cornerRadius = 6
        backdrop.frame = bounds.insetBy(dx: 2, dy: 4)
        label.frame = bounds.insetBy(dx: 4, dy: 0)
        marker.frame = CGRect(x: bounds.maxX - 14, y: 5, width: 10, height: 10)
    }
}
