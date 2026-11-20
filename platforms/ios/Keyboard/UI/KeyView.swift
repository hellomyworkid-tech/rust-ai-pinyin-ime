import CoreText
import UIKit

enum KeyTypography {
    private static let fullWidthPunctuation: Set<Character> = [
        "，", "。", "、", "：", "；", "？", "！", "（", "）",
        "「", "」", "『", "』", "【", "】", "《", "》", "〈", "〉",
        "〔", "〕", "〖", "〗", "〘", "〙", "〚", "〛", "［", "］", "｛", "｝",
    ]

    private static var halfWidthFontCache: [Int: UIFont] = [:]

    /// 单字全角中文标点在苹方里默认占满 1em、墨迹偏左；开启半宽字距后裁掉右侧留白，在键帽与放大气泡里水平正居中
    static func font(ofSize size: CGFloat, weight: UIFont.Weight = .regular, for text: String?) -> UIFont {
        guard let text, text.count == 1, let ch = text.first, fullWidthPunctuation.contains(ch) else {
            return .systemFont(ofSize: size, weight: weight)
        }
        let cacheKey = Int((size * 10).rounded())
        if let cached = halfWidthFontCache[cacheKey] {
            return cached
        }
        let base = UIFont(name: "PingFangSC-Regular", size: size) ?? .systemFont(ofSize: size, weight: weight)
        let desc = base.fontDescriptor.addingAttributes([
            .featureSettings: [[
                UIFontDescriptor.FeatureKey.type: kTextSpacingType,
                UIFontDescriptor.FeatureKey.selector: kHalfWidthTextSelector,
            ]]
        ])
        let font = UIFont(descriptor: desc, size: size)
        halfWidthFontCache[cacheKey] = font
        return font
    }
}

/// 进程内复用常用 SF Symbols 图标，避免每次呼出与重排键位时重复向系统符号表查询
enum KeyboardSymbolCache {
    static let hasSpaceSymbol: Bool = UIImage(systemName: "space") != nil

    private struct Key: Hashable {
        let name: String
        let tenthsPointSize: Int
        let weightRaw: Int
    }

    private static var cache: [Key: UIImage] = [:]

    static func image(named name: String, pointSize: CGFloat, weight: UIImage.SymbolWeight = .regular) -> UIImage? {
        let key = Key(name: name, tenthsPointSize: Int((pointSize * 10).rounded()), weightRaw: weight.rawValue)
        if let cached = cache[key] {
            return cached
        }
        let config = UIImage.SymbolConfiguration(pointSize: pointSize, weight: weight)
        guard let image = UIImage(systemName: name, withConfiguration: config) else { return nil }
        cache[key] = image
        return image
    }
}

/// 一个键的外观。不接收触摸，触摸统一由 KeyboardView 处理（键缝里也能按到）。
final class KeyView: UIView {
    /// 键上显示什么
    struct Face: Equatable {
        var title: String?
        /// 双语切换键（中/En、En/中）右下角的副标识
        var badgeSuffix: String? = nil
        /// 键帽顶部的上划副符号提示
        var subTitle: String? = nil
        var symbolName: String?
        var fontSize: CGFloat
        var subFontSize: CGFloat = 10.5
        var isFunction: Bool
        /// 回车键在发送、搜索等场景下用强调色
        var isProminent = false
        /// 上档键锁定 / 单次大写、当前语言等「开启」状态，用字母键底色
        var isOn = false
    }

    private let subLabel = UILabel()
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

        subLabel.textAlignment = .center
        subLabel.adjustsFontSizeToFitWidth = true
        subLabel.minimumScaleFactor = 0.65
        subLabel.baselineAdjustment = .alignCenters

        label.textAlignment = .center
        label.adjustsFontSizeToFitWidth = true
        label.minimumScaleFactor = 0.6
        label.baselineAdjustment = .alignCenters
        icon.contentMode = .center
        addSubview(subLabel)
        addSubview(label)
        addSubview(icon)

        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (view: KeyView, _: UITraitCollection) in
            view.updateColors()
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(_ newFace: Face, accessibilityLabel: String, accessibilityHint: String? = nil) {
        self.accessibilityLabel = accessibilityLabel
        self.accessibilityHint = accessibilityHint
        guard newFace != face else { return }
        let layoutChanged = (newFace.subTitle != nil) != (face?.subTitle != nil)
            || newFace.fontSize != face?.fontSize
            || newFace.subFontSize != face?.subFontSize
            || newFace.symbolName != face?.symbolName
        face = newFace

        subLabel.text = newFace.subTitle
        subLabel.isHidden = newFace.subTitle == nil
        if newFace.subTitle != nil {
            subLabel.font = KeyTypography.font(ofSize: newFace.subFontSize, weight: .regular, for: newFace.subTitle)
        }

        label.isHidden = newFace.title == nil
        if let name = newFace.symbolName,
           let image = KeyboardSymbolCache.image(named: name, pointSize: newFace.fontSize, weight: .regular) {
            icon.image = image
            icon.isHidden = false
        } else {
            icon.image = nil
            icon.isHidden = true
        }
        if layoutChanged { setNeedsLayout() }
        updateColors()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if face?.subTitle != nil {
            let compact = bounds.height < 36
            let subTop: CGFloat = compact ? 1.5 : 2.5
            let subHeight: CGFloat = compact ? 10 : 13
            subLabel.frame = CGRect(x: 2, y: subTop, width: max(0, bounds.width - 4), height: subHeight)
            let mainTop = round(bounds.height * (compact ? 0.26 : 0.22))
            label.frame = CGRect(x: 2, y: mainTop, width: max(0, bounds.width - 4),
                                 height: max(0, bounds.height - mainTop - 1))
        } else {
            subLabel.frame = .zero
            label.frame = bounds.insetBy(dx: 2, dy: 0)
        }
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
        subLabel.textColor = KeyboardTheme.keyHintText
        if let title = face.title, let suffix = face.badgeSuffix {
            let text = NSMutableAttributedString(
                string: title,
                attributes: [
                    .font: UIFont.systemFont(ofSize: face.fontSize, weight: .regular),
                    .foregroundColor: foreground,
                ]
            )
            text.append(NSAttributedString(
                string: suffix,
                attributes: [
                    .font: UIFont.systemFont(ofSize: face.subFontSize, weight: .regular),
                    .foregroundColor: KeyboardTheme.keyHintText,
                    .baselineOffset: -2,
                ]
            ))
            label.attributedText = text
        } else {
            label.attributedText = nil
            label.text = face.title
            label.font = KeyTypography.font(ofSize: face.fontSize, weight: .regular, for: face.title)
            label.textColor = foreground
        }
        icon.tintColor = foreground
        layer.shadowColor = KeyboardTheme.keyShadow.resolvedColor(with: traitCollection).cgColor
    }
}
