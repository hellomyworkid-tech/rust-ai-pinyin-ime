import UIKit

enum QuickSettingsToggle: CaseIterable {
    case emoji
    case typoCorrection
    case keyClickSound
    case haptics
}

protocol QuickSettingsPanelDelegate: AnyObject {
    func quickSettingsPanel(_ panel: QuickSettingsPanelView, didSelectAIMode mode: IMESettings.AIMode)
    func quickSettingsPanel(_ panel: QuickSettingsPanelView, didToggle item: QuickSettingsToggle)
    func quickSettingsPanelDidTapOpenApp(_ panel: QuickSettingsPanelView)
    func quickSettingsPanelDidTouchDownLink(_ panel: QuickSettingsPanelView)
}

/// 点击工具栏左上角设置按钮后，在按键区原位展开的快捷配置面板（不改变键盘高度、不直接跳出当前宿主应用）。
final class QuickSettingsPanelView: UIView {
    struct State: Equatable {
        var settings = IMESettings()
        var hasFullAccess = false
        var appleAvailable = false
        var localModelSubtitle = "未下载"
        var hasDownloadedLocalModel = false
        var landscape = false
    }

    weak var delegate: QuickSettingsPanelDelegate?

    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    private let appleCard = ModeCardButton()
    private let localCard = ModeCardButton()
    private let offCard = ModeCardButton()

    private let emojiTile = ToggleTileButton()
    private let typoTile = ToggleTileButton()
    private let soundTile = ToggleTileButton()
    private let hapticsTile = ToggleTileButton()

    private let openAppButton = OpenAppRowButton()

    private static let settingsURL = URL(string: "aipinyin://settings")!
    private static let aiURL = URL(string: "aipinyin://ai")!

    private var openAppLinkHost: AuxLinkHostControlling?
    private var localModelLinkHost: AuxLinkHostControlling?
    private var linksInteractive = false

    private var state = State()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = KeyboardTheme.background

        let modePairs: [(ModeCardButton, IMESettings.AIMode)] = [
            (appleCard, .apple),
            (localCard, .local),
            (offCard, .off),
        ]
        for (card, mode) in modePairs {
            card.addAction(UIAction { [weak self] _ in
                guard let self else { return }
                self.delegate?.quickSettingsPanel(self, didSelectAIMode: mode)
            }, for: .touchUpInside)
            addSubview(card)
        }

        let togglePairs: [(ToggleTileButton, QuickSettingsToggle)] = [
            (emojiTile, .emoji),
            (typoTile, .typoCorrection),
            (soundTile, .keyClickSound),
            (hapticsTile, .haptics),
        ]
        for (tile, item) in togglePairs {
            tile.addAction(UIAction { [weak self] _ in
                guard let self else { return }
                self.delegate?.quickSettingsPanel(self, didToggle: item)
            }, for: .touchUpInside)
            addSubview(tile)
        }

        openAppButton.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.delegate?.quickSettingsPanelDidTapOpenApp(self)
        }, for: .touchUpInside)
        addSubview(openAppButton)

        apply(state)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    /// 按需从 `AIPinyinAux.framework` 挂载原生 SwiftUI `Link` 子控制器：
    /// 仅在用户主动点开设置面板时才触发加载 `SwiftUI.framework`，正常呼出键盘全程零 SwiftUI 映射与初始化开销。
    func installLinkHosts(in parent: UIViewController) {
        guard openAppLinkHost == nil, let bridge = AuxBridgeLoader.shared() else { return }

        let openParams = makeOpenAppLinkParams(for: state)
        let openHost = bridge.makeLinkHost(
            destination: Self.settingsURL,
            isInteractive: linksInteractive,
            accessibilityLabel: openParams.label,
            accessibilityValue: openParams.value,
            onPressChanged: openParams.onPressChanged
        )
        let openVC = openHost.viewController
        parent.addChild(openVC)
        addSubview(openVC.view)
        openVC.didMove(toParent: parent)
        openAppLinkHost = openHost

        let localParams = makeLocalModelLinkParams(for: state)
        let localHost = bridge.makeLinkHost(
            destination: Self.aiURL,
            isInteractive: linksInteractive,
            accessibilityLabel: localParams.label,
            accessibilityValue: localParams.value,
            onPressChanged: localParams.onPressChanged
        )
        let localVC = localHost.viewController
        parent.addChild(localVC)
        addSubview(localVC.view)
        localVC.didMove(toParent: parent)
        localModelLinkHost = localHost

        syncLinkHosts()
    }

    /// 内存警告释放隐藏面板时，完整解除 SwiftUI 子控制器绑定
    func removeLinkHosts() {
        for host in [openAppLinkHost, localModelLinkHost].compactMap({ $0 }) {
            let vc = host.viewController
            vc.willMove(toParent: nil)
            vc.view.removeFromSuperview()
            vc.removeFromParent()
        }
        openAppLinkHost = nil
        localModelLinkHost = nil
    }

    func setLinksInteractive(_ interactive: Bool) {
        guard linksInteractive != interactive else { return }
        linksInteractive = interactive
        if !interactive {
            openAppButton.isLinkHighlighted = false
            localCard.isLinkHighlighted = false
        }
        syncLinkHosts()
    }

    func apply(_ newState: State) {
        let layoutChanged = newState.landscape != state.landscape
        state = newState
        let s = newState.settings
        let compact = newState.landscape

        appleCard.configure(
            icon: "sparkles",
            title: "Apple 智能",
            subtitle: newState.appleAvailable ? "系统端侧" : "设备不支持",
            selected: s.aiMode == .apple,
            compact: compact
        )
        localCard.configure(
            icon: "cpu",
            title: "本地模型",
            subtitle: newState.localModelSubtitle,
            selected: s.aiMode == .local && newState.hasDownloadedLocalModel,
            compact: compact
        )
        offCard.configure(
            icon: "bolt.slash",
            title: "关闭 AI",
            subtitle: "仅词频规则",
            selected: s.aiMode == .off,
            compact: compact
        )

        emojiTile.configure(
            icon: "face.smiling",
            title: "候选表情",
            subtitle: s.emojiCandidates ? "已开启" : "已关闭",
            isOn: s.emojiCandidates,
            dimmed: false,
            compact: compact
        )
        typoTile.configure(
            icon: "character.textbox",
            title: "拼写纠错",
            subtitle: s.fuzzy.typoCorrection ? "已开启" : "已关闭",
            isOn: s.fuzzy.typoCorrection,
            dimmed: false,
            compact: compact
        )

        let soundSubtitle: String
        if !s.keyClickSound {
            soundSubtitle = "已关闭"
        } else if !newState.hasFullAccess {
            soundSubtitle = "需完全访问"
        } else {
            soundSubtitle = "已开启"
        }
        soundTile.configure(
            icon: s.keyClickSound ? "speaker.wave.2" : "speaker.slash",
            title: "按键音",
            subtitle: soundSubtitle,
            isOn: s.keyClickSound,
            dimmed: s.keyClickSound && !newState.hasFullAccess,
            compact: compact
        )

        let hapticsSubtitle: String
        if !s.haptics {
            hapticsSubtitle = "已关闭"
        } else if !newState.hasFullAccess {
            hapticsSubtitle = "需完全访问"
        } else {
            hapticsSubtitle = "已开启"
        }
        hapticsTile.configure(
            icon: "hand.tap",
            title: "触感反馈",
            subtitle: hapticsSubtitle,
            isOn: s.haptics,
            dimmed: s.haptics && !newState.hasFullAccess,
            compact: compact
        )

        openAppButton.configure(
            hasFullAccess: newState.hasFullAccess,
            compact: compact
        )

        syncLinkHosts()

        if layoutChanged {
            setNeedsLayout()
        }
    }

    private func syncLinkHosts() {
        let canOpenSettings = state.hasFullAccess
        let canOpenAIDownload = state.hasFullAccess && !state.hasDownloadedLocalModel

        openAppButton.isAccessibilityElement = !canOpenSettings
        if !canOpenSettings { openAppButton.isLinkHighlighted = false }
        if let openHost = openAppLinkHost {
            openHost.viewController.view.isHidden = !canOpenSettings
            let p = makeOpenAppLinkParams(for: state)
            openHost.update(
                destination: Self.settingsURL,
                isInteractive: linksInteractive,
                accessibilityLabel: p.label,
                accessibilityValue: p.value,
                onPressChanged: p.onPressChanged
            )
        }

        localCard.isAccessibilityElement = !canOpenAIDownload
        if !canOpenAIDownload { localCard.isLinkHighlighted = false }
        if let localHost = localModelLinkHost {
            localHost.viewController.view.isHidden = !canOpenAIDownload
            let p = makeLocalModelLinkParams(for: state)
            localHost.update(
                destination: Self.aiURL,
                isInteractive: linksInteractive,
                accessibilityLabel: p.label,
                accessibilityValue: p.value,
                onPressChanged: p.onPressChanged
            )
        }
    }

    private func makeOpenAppLinkParams(for state: State) -> (label: String, value: String, onPressChanged: (Bool) -> Void) {
        let subtitle = state.hasFullAccess ? "模糊音 · 词库与模型下载" : "模糊音 · 模型下载（需完全访问跳转）"
        return (
            label: "打开 AI 拼音设置",
            value: subtitle,
            onPressChanged: { [weak self] pressed in
                guard let self else { return }
                self.openAppButton.isLinkHighlighted = pressed
                if pressed {
                    self.delegate?.quickSettingsPanelDidTouchDownLink(self)
                }
            }
        )
    }

    private func makeLocalModelLinkParams(for state: State) -> (label: String, value: String, onPressChanged: (Bool) -> Void) {
        (
            label: "本地模型",
            value: state.localModelSubtitle,
            onPressChanged: { [weak self] pressed in
                guard let self else { return }
                self.localCard.isLinkHighlighted = pressed
                if pressed {
                    self.delegate?.quickSettingsPanelDidTouchDownLink(self)
                }
            }
        )
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let compact = state.landscape
        let padX = horizontalInset + 10
        let topPad: CGFloat = compact ? 4 : 6
        let bottomPad: CGFloat = compact ? 6 : 8
        let rowGap: CGFloat = compact ? 6 : 8
        let colGap: CGFloat = 8

        let contentWidth = max(0, bounds.width - 2 * padX)
        let usableHeight = max(0, bounds.height - topPad - bottomPad - 2 * rowGap)

        let aiHeight = round(usableHeight * 0.30)
        let toggleHeight = round(usableHeight * 0.44)
        let openHeight = max(0, usableHeight - aiHeight - toggleHeight)

        // 第一行：3 个 AI 选词模式卡片
        let modeCards = [appleCard, localCard, offCard]
        let modeWidth = max(0, floor((contentWidth - 2 * colGap) / 3))
        for (idx, card) in modeCards.enumerated() {
            let x = padX + CGFloat(idx) * (modeWidth + colGap)
            let w = idx == modeCards.count - 1 ? max(0, padX + contentWidth - x) : modeWidth
            card.frame = CGRect(x: x, y: topPad, width: w, height: aiHeight)
        }
        localModelLinkHost?.viewController.view.frame = localCard.frame

        // 第二行：4 个常用开关磁贴
        let tiles = [emojiTile, typoTile, soundTile, hapticsTile]
        let tileY = topPad + aiHeight + rowGap
        let tileWidth = max(0, floor((contentWidth - 3 * colGap) / 4))
        for (idx, tile) in tiles.enumerated() {
            let x = padX + CGFloat(idx) * (tileWidth + colGap)
            let w = idx == tiles.count - 1 ? max(0, padX + contentWidth - x) : tileWidth
            tile.frame = CGRect(x: x, y: tileY, width: w, height: toggleHeight)
        }

        // 第三行：打开宿主 App 入口
        let openY = tileY + toggleHeight + rowGap
        openAppButton.frame = CGRect(x: padX, y: openY, width: contentWidth, height: openHeight)
        openAppLinkHost?.viewController.view.frame = openAppButton.frame
    }
}

// MARK: - 子控件

private final class ModeCardButton: UIControl {
    private let iconView = UIImageView()
    private let titleLabel = UILabel()
    private let subtitleLabel = UILabel()
    private var isChosen = false
    private var compact = false

    override var isHighlighted: Bool {
        didSet { updateColors() }
    }

    var isLinkHighlighted = false {
        didSet {
            if isLinkHighlighted != oldValue { updateColors() }
        }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        layer.cornerRadius = 11
        layer.cornerCurve = .continuous
        isAccessibilityElement = true

        iconView.contentMode = .scaleAspectFit
        addSubview(iconView)

        titleLabel.textAlignment = .left
        titleLabel.adjustsFontSizeToFitWidth = true
        titleLabel.minimumScaleFactor = 0.8
        addSubview(titleLabel)

        subtitleLabel.textAlignment = .left
        subtitleLabel.adjustsFontSizeToFitWidth = true
        subtitleLabel.minimumScaleFactor = 0.75
        addSubview(subtitleLabel)

        registerForTraitChanges([UITraitUserInterfaceStyle.self]) { (self: Self, _) in
            self.updateColors()
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(icon: String, title: String, subtitle: String, selected: Bool, compact: Bool) {
        isChosen = selected
        self.compact = compact
        let pt: CGFloat = compact ? 13 : 14.5
        iconView.image = UIImage(systemName: icon, withConfiguration: UIImage.SymbolConfiguration(pointSize: pt, weight: .semibold))
        titleLabel.text = title
        titleLabel.font = .systemFont(ofSize: compact ? 12.5 : 13.5, weight: .semibold)
        subtitleLabel.text = subtitle
        subtitleLabel.font = .systemFont(ofSize: compact ? 10 : 11, weight: .regular)

        accessibilityLabel = title
        accessibilityValue = subtitle
        accessibilityTraits = selected ? [.button, .selected] : [.button]
        updateColors()
        setNeedsLayout()
    }

    private func updateColors() {
        if isHighlighted || isLinkHighlighted {
            backgroundColor = KeyboardTheme.panelCardPressed
        } else {
            backgroundColor = isChosen ? KeyboardTheme.panelAccentFill : KeyboardTheme.panelCard
        }
        layer.borderWidth = isChosen ? 1.2 : 0
        layer.borderColor = isChosen ? KeyboardTheme.panelAccentBorder.resolvedColor(with: traitCollection).cgColor : nil

        let fg = isChosen ? KeyboardTheme.pinnedText : KeyboardTheme.keyText
        iconView.tintColor = fg
        titleLabel.textColor = fg
        subtitleLabel.textColor = isChosen ? KeyboardTheme.pinnedText : KeyboardTheme.secondaryText
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let padX: CGFloat = 10
        let iconSize: CGFloat = compact ? 15 : 17
        let topHeight: CGFloat = compact ? 17 : 20
        let subHeight: CGFloat = compact ? 13 : 15
        let gap: CGFloat = compact ? 1 : 3
        let totalH = topHeight + gap + subHeight
        let startY = floor((bounds.height - totalH) / 2)

        iconView.frame = CGRect(x: padX, y: startY + floor((topHeight - iconSize) / 2), width: iconSize, height: iconSize)
        let titleX = iconView.frame.maxX + 5
        titleLabel.frame = CGRect(x: titleX, y: startY, width: max(0, bounds.width - titleX - 6), height: topHeight)
        subtitleLabel.frame = CGRect(x: padX, y: startY + topHeight + gap, width: max(0, bounds.width - 2 * padX), height: subHeight)
    }
}

private final class ToggleTileButton: UIControl {
    private let iconView = UIImageView()
    private let titleLabel = UILabel()
    private let stateLabel = UILabel()
    private var isOn = false
    private var dimmed = false
    private var compact = false

    override var isHighlighted: Bool {
        didSet { updateColors() }
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        layer.cornerRadius = 12
        layer.cornerCurve = .continuous
        isAccessibilityElement = true

        iconView.contentMode = .scaleAspectFit
        addSubview(iconView)

        titleLabel.textAlignment = .center
        titleLabel.adjustsFontSizeToFitWidth = true
        titleLabel.minimumScaleFactor = 0.8
        addSubview(titleLabel)

        stateLabel.textAlignment = .center
        stateLabel.adjustsFontSizeToFitWidth = true
        stateLabel.minimumScaleFactor = 0.75
        addSubview(stateLabel)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(icon: String, title: String, subtitle: String, isOn: Bool, dimmed: Bool, compact: Bool) {
        self.isOn = isOn
        self.dimmed = dimmed
        self.compact = compact

        let iconPt: CGFloat = compact ? 15 : 19
        iconView.image = UIImage(systemName: icon, withConfiguration: UIImage.SymbolConfiguration(pointSize: iconPt, weight: .medium))
        titleLabel.text = title
        titleLabel.font = .systemFont(ofSize: compact ? 11.5 : 12.5, weight: .medium)
        stateLabel.text = subtitle
        stateLabel.font = .systemFont(ofSize: compact ? 9.5 : 10.5, weight: .regular)

        accessibilityLabel = title
        accessibilityValue = subtitle
        accessibilityTraits = isOn ? [.button, .selected] : [.button]
        updateColors()
        setNeedsLayout()
    }

    private func updateColors() {
        if isHighlighted {
            backgroundColor = KeyboardTheme.panelCardPressed
        } else {
            backgroundColor = isOn ? KeyboardTheme.panelCardActive : KeyboardTheme.panelCard
        }
        if isOn && !dimmed {
            iconView.tintColor = KeyboardTheme.pinnedText
            stateLabel.textColor = KeyboardTheme.pinnedText
        } else {
            iconView.tintColor = KeyboardTheme.secondaryText
            stateLabel.textColor = KeyboardTheme.secondaryText
        }
        titleLabel.textColor = KeyboardTheme.keyText
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let iconSize: CGFloat = compact ? 17 : 22
        let titleH: CGFloat = compact ? 15 : 17
        let stateH: CGFloat = compact ? 12 : 14
        let gap1: CGFloat = compact ? 2 : 5
        let gap2: CGFloat = compact ? 1 : 2
        let total = iconSize + gap1 + titleH + gap2 + stateH
        let top = floor((bounds.height - total) / 2)

        iconView.frame = CGRect(x: floor((bounds.width - iconSize) / 2), y: top, width: iconSize, height: iconSize)
        titleLabel.frame = CGRect(x: 4, y: iconView.frame.maxY + gap1, width: max(0, bounds.width - 8), height: titleH)
        stateLabel.frame = CGRect(x: 4, y: titleLabel.frame.maxY + gap2, width: max(0, bounds.width - 8), height: stateH)
    }
}

private final class OpenAppRowButton: UIControl {
    private let leadingIcon = UIImageView()
    private let titleLabel = UILabel()
    private let subtitleLabel = UILabel()
    private let trailingIcon = UIImageView()
    private var compact = false

    override var isHighlighted: Bool {
        didSet { updateBackground() }
    }

    var isLinkHighlighted = false {
        didSet {
            if isLinkHighlighted != oldValue { updateBackground() }
        }
    }

    private func updateBackground() {
        backgroundColor = (isHighlighted || isLinkHighlighted)
            ? KeyboardTheme.panelCardPressed
            : KeyboardTheme.panelCardActive
    }

    override init(frame: CGRect) {
        super.init(frame: frame)
        layer.cornerRadius = 11
        layer.cornerCurve = .continuous
        backgroundColor = KeyboardTheme.panelCardActive
        isAccessibilityElement = true
        accessibilityLabel = "打开 AI 拼音设置"
        accessibilityTraits = [.button]

        leadingIcon.contentMode = .scaleAspectFit
        leadingIcon.tintColor = KeyboardTheme.pinnedText
        addSubview(leadingIcon)

        titleLabel.textColor = KeyboardTheme.keyText
        addSubview(titleLabel)

        subtitleLabel.textColor = KeyboardTheme.secondaryText
        subtitleLabel.textAlignment = .right
        subtitleLabel.adjustsFontSizeToFitWidth = true
        subtitleLabel.minimumScaleFactor = 0.8
        addSubview(subtitleLabel)

        trailingIcon.contentMode = .scaleAspectFit
        trailingIcon.tintColor = KeyboardTheme.secondaryText
        addSubview(trailingIcon)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func configure(hasFullAccess: Bool, compact: Bool) {
        self.compact = compact
        let pt: CGFloat = compact ? 14 : 15.5
        leadingIcon.image = UIImage(systemName: "slider.horizontal.3", withConfiguration: UIImage.SymbolConfiguration(pointSize: pt, weight: .medium))
        trailingIcon.image = UIImage(systemName: "arrow.up.forward.app", withConfiguration: UIImage.SymbolConfiguration(pointSize: compact ? 13 : 14, weight: .medium))
        titleLabel.text = "打开 AI 拼音"
        titleLabel.font = .systemFont(ofSize: compact ? 13 : 14, weight: .medium)
        subtitleLabel.text = hasFullAccess ? "模糊音 · 词库与模型下载" : "模糊音 · 模型下载（需完全访问跳转）"
        subtitleLabel.font = .systemFont(ofSize: compact ? 11 : 12, weight: .regular)
        accessibilityValue = subtitleLabel.text
        setNeedsLayout()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let padX: CGFloat = 12
        let iconSize: CGFloat = compact ? 17 : 19
        let trailingSize: CGFloat = compact ? 15 : 16
        leadingIcon.frame = CGRect(x: padX, y: floor((bounds.height - iconSize) / 2), width: iconSize, height: iconSize)
        trailingIcon.frame = CGRect(x: bounds.width - padX - trailingSize, y: floor((bounds.height - trailingSize) / 2), width: trailingSize, height: trailingSize)

        let titleX = leadingIcon.frame.maxX + 8
        let titleWidth: CGFloat = 96
        titleLabel.frame = CGRect(x: titleX, y: 0, width: titleWidth, height: bounds.height)

        let subX = titleLabel.frame.maxX + 6
        let subW = max(0, trailingIcon.frame.minX - 6 - subX)
        subtitleLabel.frame = CGRect(x: subX, y: 0, width: subW, height: bounds.height)
    }
}
