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
    }

    weak var delegate: KeyboardViewDelegate?
    /// 放大气泡，由外层视图持有（要画到候选栏上面去）
    weak var preview: KeyPreviewView?
    /// 横屏时左右让出安全区
    var horizontalInset: CGFloat = 0 {
        didSet { if horizontalInset != oldValue { setNeedsLayout() } }
    }

    private(set) var config = Config()
    private var rows: [[KeySpec]] = []
    private var specs: [KeySpec] = []
    private var keyViews: [KeyView] = []
    private var keyFrames: [KeyboardGeometry.KeyFrame] = []

    private struct TrackedTouch {
        var keyIndex: Int
        /// 已经触发过（多指连打时提前触发），抬手时不再触发
        var consumed: Bool
    }

    private var tracked: [ObjectIdentifier: TrackedTouch] = [:]
    private var repeatTimer: Timer?

    static let repeatDelay: TimeInterval = 0.45
    static let repeatInterval: TimeInterval = 0.08

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        apply(config, force: true)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) has not been implemented")
    }

    func apply(_ newConfig: Config, force: Bool = false) {
        guard force || newConfig != config else { return }
        let rowsChanged = force || newConfig.page != config.page || newConfig.showsGlobe != config.showsGlobe
            || (newConfig.page != .letters && newConfig.chineseMode != config.chineseMode)
        config = newConfig
        if rowsChanged {
            cancelAllTouches()
            rows = KeyboardLayout.rows(for: newConfig.page, chineseMode: newConfig.chineseMode,
                                       showsGlobe: newConfig.showsGlobe)
            specs = rows.flatMap { $0 }
            while keyViews.count < specs.count {
                let view = KeyView()
                addSubview(view)
                keyViews.append(view)
            }
            for (i, view) in keyViews.enumerated() { view.isHidden = i >= specs.count }
            setNeedsLayout()
        }
        for (i, spec) in specs.enumerated() {
            keyViews[i].configure(face(for: spec), accessibilityLabel: accessibilityTitle(for: spec))
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        var metrics: KeyboardGeometry.Metrics = config.landscape ? .landscape : .portrait
        metrics.sideInset += horizontalInset
        // 行距随系统给的高度走：键高约占行高的八成
        if !rows.isEmpty {
            metrics.rowGap = max(4, (bounds.height / CGFloat(rows.count) * 0.2).rounded())
        }
        keyFrames = KeyboardGeometry.layout(rows: rows, in: bounds, metrics: metrics)
        for (i, frame) in keyFrames.enumerated() where i < keyViews.count {
            keyViews[i].frame = frame.frame
        }
    }

    // MARK: - 键面

    private func face(for spec: KeySpec) -> KeyView.Face {
        let small: CGFloat = config.landscape ? 13 : 16
        let large: CGFloat = config.landscape ? 18 : 23
        let icon: CGFloat = config.landscape ? 15 : 18
        var face = KeyView.Face(title: nil, symbolName: nil, fontSize: small, isFunction: spec.isFunctionKey)
        switch spec.action {
        case .letter(let c):
            face.title = config.shift == .off ? String(c) : c.uppercased()
            face.fontSize = large
        case .symbol(let s):
            face.title = PunctuationMap.label(for: s, chineseMode: config.chineseMode)
            face.fontSize = (face.title?.count ?? 0) > 1 ? small + 2 : large - 1
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
            face.title = config.chineseMode ? "空格" : "space"
        case .returnKey:
            face.title = config.returnTitle
            face.isProminent = config.returnProminent
        case .toggleLanguage:
            face.title = config.chineseMode ? "中" : "英"
            face.fontSize = small + 2
        case .page(let page):
            switch page {
            case .letters: face.title = config.chineseMode ? "拼音" : "ABC"
            case .numbers: face.title = "123"
            case .symbols: face.title = "#+="
            }
        }
        return face
    }

    private func accessibilityTitle(for spec: KeySpec) -> String {
        switch spec.action {
        case .letter(let c): return String(c)
        case .symbol(let s): return PunctuationMap.label(for: s, chineseMode: config.chineseMode)
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

    /// 键面上显示的字（给气泡用）
    private func previewTitle(for spec: KeySpec) -> String? {
        switch spec.action {
        case .letter(let c): return config.shift == .off ? String(c) : c.uppercased()
        case .symbol(let s): return PunctuationMap.label(for: s, chineseMode: config.chineseMode)
        default: return nil
        }
    }

    // MARK: - 触摸

    private func keyIndex(at point: CGPoint) -> Int? {
        if let hit = keyFrames.firstIndex(where: { $0.touchFrame.contains(point) }) { return hit }
        // 落在区域外（比如滑出了上边）：取最近的键
        return keyFrames.indices.min { distance(point, keyFrames[$0].frame) < distance(point, keyFrames[$1].frame) }
    }

    private func distance(_ p: CGPoint, _ r: CGRect) -> CGFloat {
        let dx = max(r.minX - p.x, 0, p.x - r.maxX)
        let dy = max(r.minY - p.y, 0, p.y - r.maxY)
        return dx * dx + dy * dy
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            guard let index = keyIndex(at: touch.location(in: self)), index < specs.count else { continue }
            let spec = specs[index]
            // 多指连打：新的手指按下时，先让还按着的字符键生效
            flushPendingCharacterTouches()
            tracked[ObjectIdentifier(touch)] = TrackedTouch(keyIndex: index, consumed: false)
            keyViews[index].isPressed = true
            delegate?.keyboardViewDidTouchDownKey(self)
            switch spec.action {
            case .globe:
                delegate?.keyboardView(self, globeEvent: event)
            case .shift:
                markConsumed(touch)
                delegate?.keyboardView(self, didTrigger: .shift)
            case .backspace:
                markConsumed(touch)
                delegate?.keyboardView(self, didTrigger: .backspace)
                startRepeating()
            default:
                showPreview(for: index)
            }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for touch in touches {
            let id = ObjectIdentifier(touch)
            guard var state = tracked[id], !state.consumed else { continue }
            let spec = specs[state.keyIndex]
            if spec.action == .globe {
                delegate?.keyboardView(self, globeEvent: event)
                continue
            }
            // 只有字符键之间可以滑动换键
            guard spec.showsPreview,
                  let index = keyIndex(at: touch.location(in: self)), index != state.keyIndex,
                  specs[index].showsPreview else { continue }
            keyViews[state.keyIndex].isPressed = false
            state.keyIndex = index
            tracked[id] = state
            keyViews[index].isPressed = true
            showPreview(for: index)
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(touches, event: event, trigger: true)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        finish(touches, event: event, trigger: false)
    }

    private func finish(_ touches: Set<UITouch>, event: UIEvent?, trigger: Bool) {
        for touch in touches {
            guard let state = tracked.removeValue(forKey: ObjectIdentifier(touch)) else { continue }
            let spec = specs[state.keyIndex]
            if !tracked.values.contains(where: { $0.keyIndex == state.keyIndex }) {
                keyViews[state.keyIndex].isPressed = false
            }
            if spec.action == .backspace { stopRepeating() }
            if spec.action == .globe {
                delegate?.keyboardView(self, globeEvent: event)
                continue
            }
            if trigger, !state.consumed { delegate?.keyboardView(self, didTrigger: spec.action) }
        }
        if !tracked.values.contains(where: { specs[$0.keyIndex].showsPreview && !$0.consumed }) {
            preview?.hide()
        }
    }

    private func flushPendingCharacterTouches() {
        for (id, state) in tracked where !state.consumed && specs[state.keyIndex].showsPreview {
            tracked[id]?.consumed = true
            keyViews[state.keyIndex].isPressed = false
            delegate?.keyboardView(self, didTrigger: specs[state.keyIndex].action)
        }
        preview?.hide()
    }

    private func markConsumed(_ touch: UITouch) {
        tracked[ObjectIdentifier(touch)]?.consumed = true
    }

    private func cancelAllTouches() {
        stopRepeating()
        for state in tracked.values where state.keyIndex < keyViews.count {
            keyViews[state.keyIndex].isPressed = false
        }
        tracked.removeAll()
        preview?.hide()
    }

    private func showPreview(for index: Int) {
        guard let preview, let container = preview.superview,
              specs[index].showsPreview, let title = previewTitle(for: specs[index]) else {
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
