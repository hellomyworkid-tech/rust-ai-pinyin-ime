import UIKit

protocol KeyboardViewDelegate: AnyObject {
    /// 键被触发（字母、字符、空格等在抬手时；上档、退格在按下时；退格按住会反复触发）
    func keyboardView(_ keyboardView: KeyboardView, didTrigger action: KeyAction)
    /// 有键被按下（用于按键音、触感）
    func keyboardViewDidTouchDownKey(_ keyboardView: KeyboardView)
    /// 地球键的触摸事件，原样交给 handleInputModeList(from:with:)
    func keyboardView(_ keyboardView: KeyboardView, globeEvent event: UIEvent?)
}

/// 按键区。所有键是一组复用的 KeyView，排布由 KeyboardGeometry 计算，触摸在这里统一处理：
/// 支持多指连打（第二指按下时第一指的字立即生效）、在字母键间滑动换键、退格长按连删。
final class KeyboardView: UIView {
    struct Config: Equatable {
        var page: KeyboardPage = .letters
        var chineseMode = true
        var shift: ShiftState.Mode = .off
        var returnTitle = "换行"
        var returnProminent = false
        var showsGlobe = false
        var landscape = false
        var split = false
    }

    weak var delegate: KeyboardViewDelegate?
    /// 放大气泡，由外层视图持有（要画到候选栏上面去）
    weak var preview: KeyPreviewView?
    /// 横屏时左右让出安全区
    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }
    /// 折叠屏展开态支持左右不对称安全区（如单侧屏下镜头或侧栏）
    var leftSafeInset: CGFloat = 0 {
        didSet { if leftSafeInset != oldValue { setNeedsLayout() } }
    }
    var rightSafeInset: CGFloat = 0 {
        didSet { if rightSafeInset != oldValue { setNeedsLayout() } }
    }

    private(set) var config = Config()
    private(set) var isConfigured = false
    private var rows: [[KeySpec]] = []
    private var splitRows: [SplitRowSpec] = []
    private var specs: [KeySpec] = []
    private var keyViews: [KeyView] = []
    private var keyFrames: [KeyboardGeometry.KeyFrame] = []
    private let leftBankPlate = UIView()
    private let rightBankPlate = UIView()

    private var touchTracker = KeyboardTouchTracker()
    private var repeatTimer: Timer?

    static let repeatDelay: TimeInterval = 0.45
    static let repeatInterval: TimeInterval = 0.08

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        for plate in [leftBankPlate, rightBankPlate] {
            plate.isUserInteractionEnabled = false
            plate.isHidden = true
            plate.backgroundColor = KeyboardTheme.splitBankPlate
            plate.layer.cornerRadius = 13
            plate.layer.cornerCurve = .continuous
            addSubview(plate)
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func apply(_ newConfig: Config, force: Bool = false) {
        let forceBuild = force || !isConfigured
        guard forceBuild || newConfig != config else { return }
        let rowsChanged = forceBuild
            || newConfig.page != config.page
            || newConfig.showsGlobe != config.showsGlobe
            || newConfig.split != config.split
            || (newConfig.page != .letters && newConfig.chineseMode != config.chineseMode)
        isConfigured = true
        config = newConfig
        if rowsChanged {
            cancelAllTouches()
            if newConfig.split {
                rows = []
                splitRows = KeyboardLayout.splitRows(for: newConfig.page, chineseMode: newConfig.chineseMode,
                                                     showsGlobe: newConfig.showsGlobe)
                specs = splitRows.flatMap { $0.left + $0.right }
            } else {
                splitRows = []
                rows = KeyboardLayout.rows(for: newConfig.page, chineseMode: newConfig.chineseMode,
                                           showsGlobe: newConfig.showsGlobe)
                specs = rows.flatMap { $0 }
            }
            while keyViews.count < specs.count {
                let view = KeyView()
                addSubview(view)
                keyViews.append(view)
            }
            for (i, view) in keyViews.enumerated() { view.isHidden = i >= specs.count }
            setNeedsLayout()
        }
        var spaceOrdinal = 0
        for (i, spec) in specs.enumerated() {
            keyViews[i].configure(face(for: spec),
                                  accessibilityLabel: accessibilityTitle(for: spec),
                                  accessibilityHint: accessibilityHint(for: spec))
            if newConfig.split, spec.action == .space {
                keyViews[i].accessibilityIdentifier = spaceOrdinal == 0 ? "spaceLeft" : "spaceRight"
                spaceOrdinal += 1
            } else {
                keyViews[i].accessibilityIdentifier = nil
            }
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if config.split {
            let rowCount = max(1, splitRows.count)
            var metrics = KeyboardGeometry.SplitMetrics()
            metrics.leftSafeInset = max(horizontalInset, leftSafeInset)
            metrics.rightSafeInset = max(horizontalInset, rightSafeInset)
            metrics.rowGap = max(4, (bounds.height / CGFloat(rowCount) * 0.19).rounded())
            let result = KeyboardGeometry.layoutSplit(rows: splitRows, in: bounds, metrics: metrics)
            leftBankPlate.isHidden = false
            rightBankPlate.isHidden = false
            leftBankPlate.frame = result.leftBankFrame
            rightBankPlate.frame = result.rightBankFrame
            keyFrames = result.frames
        } else {
            leftBankPlate.isHidden = true
            rightBankPlate.isHidden = true
            var metrics: KeyboardGeometry.Metrics = config.landscape ? .landscape : .portrait
            metrics.sideInset += horizontalInset
            // 行距随系统给的高度走：键高约占行高的八成
            if !rows.isEmpty {
                metrics.rowGap = max(4, (bounds.height / CGFloat(rows.count) * 0.2).rounded())
            }
            keyFrames = KeyboardGeometry.layout(rows: rows, in: bounds, metrics: metrics)
        }
        touchTracker.specs = specs
        touchTracker.frames = keyFrames
        for (i, frame) in keyFrames.enumerated() where i < keyViews.count {
            keyViews[i].frame = frame.frame
        }
    }

    // MARK: - 键面

    private func subLabelTitle(for action: KeyAction) -> String? {
        switch action {
        case .letter(let c): return String(c)
        case .symbol(let s): return PunctuationMap.subLabel(for: s, chineseMode: config.chineseMode)
        case .literal(let s): return s
        default: return nil
        }
    }

    private func face(for spec: KeySpec) -> KeyView.Face {
        let small: CGFloat = config.landscape ? 13 : 16
        let large: CGFloat = config.landscape ? 18 : 23
        let sub: CGFloat = config.landscape ? 9 : 10.5
        let icon: CGFloat = config.landscape ? 15 : 18
        let subTitle = spec.swipeUpAction.flatMap { subLabelTitle(for: $0) }
        var face = KeyView.Face(title: nil, subTitle: subTitle, symbolName: nil,
                                fontSize: small, subFontSize: sub, isFunction: spec.isFunctionKey)
        switch spec.action {
        case .letter(let c):
            face.title = config.shift == .off ? String(c) : c.uppercased()
            face.fontSize = subTitle != nil ? (config.landscape ? 16 : 22) : large
        case .symbol(let s):
            let text = PunctuationMap.label(for: s, chineseMode: config.chineseMode)
            face.title = text
            if text.count > 1 {
                face.fontSize = small + 2
            } else {
                face.fontSize = subTitle != nil ? (config.landscape ? 16 : 20) : large - 1
            }
        case .literal(let s):
            face.title = s
            face.fontSize = s.count > 1 ? small + 2 : large - 1
        case .shift:
            switch config.shift {
            case .off: face.symbolName = "shift"
            case .once: face.symbolName = "shift.fill"
            case .locked: face.symbolName = "capslock.fill"
            }
            face.fontSize = icon
            face.isOn = config.shift != .off
        case .backspace:
            face.symbolName = "delete.left"
            face.fontSize = icon
        case .globe:
            face.symbolName = "globe"
            face.fontSize = icon
        case .space:
            if KeyboardSymbolCache.hasSpaceSymbol {
                face.symbolName = "space"
                face.fontSize = icon
            } else {
                face.title = config.chineseMode ? "空格" : "space"
            }
        case .returnKey:
            face.title = config.returnTitle
            face.isProminent = config.returnProminent
        case .toggleLanguage:
            face.title = config.chineseMode ? "中" : "En"
            face.badgeSuffix = config.chineseMode ? "/En" : "/中"
            face.fontSize = config.landscape ? 13 : 15
            face.subFontSize = config.landscape ? 8.5 : 9.5
        case .page(let page):
            switch page {
            case .letters: face.title = config.chineseMode ? "拼音" : "ABC"
            case .numbers: face.title = "123"
            case .symbols: face.title = config.chineseMode ? "符号" : "#+="
            }
        }
        return face
    }

    private func accessibilityTitle(for spec: KeySpec) -> String {
        switch spec.action {
        case .letter(let c): return String(c)
        case .symbol(let s): return PunctuationMap.label(for: s, chineseMode: config.chineseMode)
        case .literal(let s): return s
        case .shift: return "上档"
        case .backspace: return "删除"
        case .globe: return "切换输入法"
        case .space: return "空格"
        case .returnKey: return config.returnTitle
        case .toggleLanguage: return config.chineseMode ? "中文，点按切换到英文" : "英文，点按切换到中文"
        case .page(let page):
            switch page {
            case .letters: return "字母"
            case .numbers: return "数字"
            case .symbols: return "符号"
            }
        }
    }

    private func accessibilityHint(for spec: KeySpec) -> String? {
        guard let swipeUp = spec.swipeUpAction, let text = actionTitle(for: swipeUp) else { return nil }
        return "上划输入 \(text)"
    }

    /// 键面上或上划对应的显示文字（给放大气泡用）
    private func actionTitle(for action: KeyAction) -> String? {
        switch action {
        case .letter(let c): return config.shift == .off ? String(c) : c.uppercased()
        case .symbol(let s): return PunctuationMap.label(for: s, chineseMode: config.chineseMode)
        case .literal(let s): return s
        default: return nil
        }
    }

    // MARK: - 触摸

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            applyOutcome(touchTracker.begin(id: ObjectIdentifier(touch), at: touch.location(in: self)), event: event)
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            applyOutcome(touchTracker.move(id: ObjectIdentifier(touch), to: touch.location(in: self)), event: event)
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            applyOutcome(touchTracker.end(id: ObjectIdentifier(touch), trigger: true), event: event)
        }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            applyOutcome(touchTracker.end(id: ObjectIdentifier(touch), trigger: false), event: event)
        }
    }

    private func applyOutcome(_ outcome: KeyboardTouchTracker.Outcome, event: UIEvent?) {
        if outcome.didTouchDown {
            delegate?.keyboardViewDidTouchDownKey(self)
        }
        if outcome.forwardGlobe {
            delegate?.keyboardView(self, globeEvent: event)
        }
        if outcome.startBackspaceRepeat {
            startRepeating()
        }
        if outcome.stopBackspaceRepeat {
            stopRepeating()
        }
        for action in outcome.triggered {
            delegate?.keyboardView(self, didTrigger: action)
        }
        syncPressedStates()
        if let p = outcome.preview {
            showPreview(for: p.keyIndex, action: p.action)
        } else if outcome.hidePreview {
            preview?.hide()
        }
    }

    private func syncPressedStates() {
        let pressed = touchTracker.pressedKeyIndices
        for i in 0..<min(specs.count, keyViews.count) {
            keyViews[i].isPressed = pressed.contains(i)
        }
    }

    private func cancelAllTouches() {
        stopRepeating()
        touchTracker.cancelAll()
        for i in 0..<keyViews.count {
            keyViews[i].isPressed = false
        }
        preview?.hide()
    }

    private func showPreview(for index: Int, action: KeyAction) {
        guard let preview, let container = preview.superview,
              specs[index].showsPreview, let title = actionTitle(for: action) else {
            preview?.hide()
            return
        }
        preview.show(title: title, above: convert(keyViews[index].frame, to: container))
    }

    // MARK: - 退格连删

    private func startRepeating() {
        stopRepeating()
        repeatTimer = Timer.scheduledTimer(withTimeInterval: Self.repeatDelay, repeats: false) { [weak self] _ in
            guard let self else { return }
            self.repeatTimer = Timer.scheduledTimer(withTimeInterval: Self.repeatInterval, repeats: true) { [weak self] _ in
                guard let self else { return }
                self.delegate?.keyboardView(self, didTrigger: .backspace)
            }
        }
    }

    private func stopRepeating() {
        repeatTimer?.invalidate()
        repeatTimer = nil
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil { cancelAllTouches() }
    }
}
