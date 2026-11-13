import UIKit

protocol CompositionStripDelegate: AnyObject {
    func compositionStrip(_ strip: CompositionStripView, didTapSyllable index: Int)
}

/// 候选栏上方的音节栏：按音节显示拼音，点哪个音节光标就移到哪；
/// 当前音节高亮，已选定的音节和整句预览里选定的字用强调色。
final class CompositionStripView: UIView {
    weak var delegate: CompositionStripDelegate?

    private let scrollView = UIScrollView()
    private var chips: [SyllableChip] = []
    private let sentenceLabel = UILabel()
    private var model: CompositionModel?
    private var landscape = false

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        scrollView.showsHorizontalScrollIndicator = false
        scrollView.alwaysBounceHorizontal = false
        scrollView.delaysContentTouches = false
        scrollView.hideEdgeEffects()
        addSubview(scrollView)
        sentenceLabel.isAccessibilityElement = true
        scrollView.addSubview(sentenceLabel)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func show(_ newModel: CompositionModel, landscape newLandscape: Bool) {
        guard newModel != model || newLandscape != landscape else { return }
        model = newModel
        landscape = newLandscape
        layoutContent()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let frame = bounds.insetBy(dx: horizontalInset, dy: 0)
        if scrollView.frame != frame {
            scrollView.frame = frame
            layoutContent()
        }
    }

    private var font: UIFont { .systemFont(ofSize: landscape ? 12 : 14, weight: .regular) }

    private func layoutContent() {
        guard let model else { return }
        let syllables = model.syllables
        while chips.count < syllables.count {
            let chip = SyllableChip()
            chip.addTarget(self, action: #selector(chipTapped(_:)), for: .touchUpInside)
            scrollView.addSubview(chip)
            chips.append(chip)
        }
        let height = scrollView.bounds.height
        var x: CGFloat = 8
        var activeFrame: CGRect?
        for (i, chip) in chips.enumerated() {
            guard i < syllables.count else {
                chip.isHidden = true
                continue
            }
            let syllable = syllables[i]
            chip.isHidden = false
            chip.configure(syllable, index: i, font: font)
            let width = chip.preferredWidth
            chip.frame = CGRect(x: x, y: 1, width: width, height: max(1, height - 2))
            if syllable.isActive { activeFrame = chip.frame }
            x += width + 1
        }

        // 整句预览：选定的字用强调色，其余灰色
        let sentence = NSMutableAttributedString()
        for part in model.sentence {
            let color = part.pinned ? KeyboardTheme.pinnedText : KeyboardTheme.secondaryText
            sentence.append(NSAttributedString(string: part.text, attributes: [.font: font, .foregroundColor: color]))
        }
        sentenceLabel.attributedText = sentence
        sentenceLabel.accessibilityLabel = "整句预览：" + sentence.string
        let sentenceWidth = ceil(sentence.size().width)
        sentenceLabel.isHidden = sentence.length == 0
        sentenceLabel.frame = CGRect(x: x + 10, y: 0, width: sentenceWidth, height: height)
        let contentWidth = sentence.length == 0 ? x + 8 : x + 10 + sentenceWidth + 8
        scrollView.contentSize = CGSize(width: contentWidth, height: height)

        // 让当前音节露出来；没有当前音节时停在末尾（刚打的字母处）
        let visible = scrollView.bounds.width
        let target = activeFrame.map { $0.midX - visible / 2 } ?? (x - visible + 8)
        scrollView.contentOffset.x = max(0, min(target, contentWidth - visible))
    }

    @objc private func chipTapped(_ chip: SyllableChip) {
        delegate?.compositionStrip(self, didTapSyllable: chip.index)
    }
}
