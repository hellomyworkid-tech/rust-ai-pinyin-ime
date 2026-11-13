import UIKit

/// 展开的候选网格里的一格
final class CandidateGridCell: UICollectionViewCell {
    static let reuseIdentifier = "CandidateGridCell"

    private let label = UILabel()
    private let marker = UIImageView()
    private let divider = UIView()
    private let verticalDivider = UIView()

    override init(frame: CGRect) {
        super.init(frame: frame)
        label.textAlignment = .center
        label.textColor = KeyboardTheme.keyText
        label.adjustsFontSizeToFitWidth = true
        label.minimumScaleFactor = 0.7
        contentView.addSubview(label)
        marker.image = UIImage(systemName: "sparkle",
                               withConfiguration: UIImage.SymbolConfiguration(pointSize: 8, weight: .bold))
        marker.tintColor = KeyboardTheme.aiMarker
        contentView.addSubview(marker)
        divider.backgroundColor = KeyboardTheme.separator
        contentView.addSubview(divider)
        verticalDivider.backgroundColor = KeyboardTheme.separator
        contentView.addSubview(verticalDivider)
        selectedBackgroundView = nil
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(_ candidate: IMECandidate, isFirst: Bool, landscape: Bool) {
        label.text = candidate.text
        label.font = CandidateCell.font(isFirst: isFirst, landscape: landscape)
        let boosted = CandidateCell.isAIBoosted(candidate)
        marker.isHidden = !boosted
        accessibilityLabel = boosted ? "\(candidate.text)，AI 推荐" : candidate.text
    }

    override var isHighlighted: Bool {
        didSet { contentView.backgroundColor = isHighlighted ? KeyboardTheme.candidatePressed : .clear }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let bounds = contentView.bounds
        label.frame = bounds.insetBy(dx: 6, dy: 0)
        marker.frame = CGRect(x: bounds.maxX - 14, y: 6, width: 10, height: 10)
        let hairline = 1 / max(1, traitCollection.displayScale)
        divider.frame = CGRect(x: 0, y: bounds.maxY - hairline, width: bounds.width, height: hairline)
        verticalDivider.frame = CGRect(x: bounds.maxX - hairline, y: 10, width: hairline, height: bounds.height - 20)
    }
}
