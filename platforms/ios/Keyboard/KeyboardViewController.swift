import UIKit
import os

/// 键盘扩展的入口。
///
/// 启动顺序：loadView 只建轻量视图并画出首帧 → viewWillAppear 读设置、按输入框类型定模式；
/// 引擎已存在时，词库换了（App 刚下完完整词库）就丢掉旧引擎，否则重新读入被别的进程改过的个人词库 →
/// viewDidAppear 之后的下一拍才创建引擎、接 AI（Apple 模型预热或本地模型加载）。
/// 加载慢的键盘会被宿主杀掉，所以 viewDidLoad 里不做任何重活。
final class KeyboardViewController: UIInputViewController {
    private let input = InputController(makeEngine: { EngineFactory.makeEngine(source: $0) },
                                        currentLexicon: { EngineFactory.lexiconSource() })
    private let ai = AIController()
    private var rootView: KeyboardRootView!
    private var feedback: KeyFeedback!

    private var page: KeyboardPage = .letters
    private var shift = ShiftState()
    private var traits = FieldTraits()
    private var settings = IMESettings()
    private var documentID: UUID?
    private var aiStatus = ""

    // MARK: - 生命周期

    override func loadView() {
        let root = KeyboardRootView()
        rootView = root
        view = root
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        feedback = KeyFeedback(view: rootView)
        rootView.keyboard.delegate = self
        rootView.bar.delegate = self
        rootView.grid.delegate = self
        rootView.strip.delegate = self
        rootView.toolbar.onDismiss = { [weak self] in self?.dismissKeyboard() }

        input.output = ProxyTextOutput(controller: self)
        input.onChange = { [weak self] in self?.render() }
        input.onEngineCreated = { [weak self] engine in self?.ai.attach(engine) }
        input.onEngineDropped = { [weak self] in self?.ai.detachEngine() }
        ai.onStatusChange = { [weak self] status in
            self?.aiStatus = status
            self?.render()
        }

        // 键盘高度由根视图里的内容约束决定（见 KeyboardRootView.updateHeight）
        rootView.updateHeight(landscape: false, width: view.bounds.width)
        render()
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        // settings.json 只有几百字节，读一次很快；键盘没有写权限，只读
        settings = IMESettings.load()
        input.apply(settings)
        feedback.configure(settings: settings, hasFullAccess: hasFullAccess)
        startField()
        // 同一个控制器再次出现时，别的宿主里的键盘进程可能已经写过个人词库（通常只是一次 stat）
        input.keyboardWillAppear()
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)

        // 屏幕底边的系统手势会推迟最下一行键的 touchesBegan
        view.window?.gestureRecognizers?.forEach { $0.delaysTouchesBegan = false }
        // 首帧已经上屏，下一拍再建引擎、接 AI
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.ai.apply(self.settings)
            self.input.ensureEngine()
        }
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        input.resetComposition()
        ai.keyboardWillHide()
        setGridExpanded(false)
    }

    override func viewWillLayoutSubviews() {
        let landscape = traitCollection.verticalSizeClass == .compact
        rootView.landscape = landscape
        rootView.updateHeight(landscape: landscape, width: view.bounds.width)
        if rootView.keyboard.config.showsGlobe != needsInputModeSwitchKey
            || rootView.keyboard.config.landscape != landscape {
            render()
        }
        super.viewWillLayoutSubviews()
    }

    override func didReceiveMemoryWarning() {
        super.didReceiveMemoryWarning()
        ai.didReceiveMemoryWarning()
    }

    // MARK: - 输入框

    override func textDidChange(_ textInput: UITextInput?) {
        super.textDidChange(textInput)
        guard isViewLoaded else { return }
        if currentDocumentID != documentID || currentTraits() != traits {
            // 换了输入框
            startField()
        } else {
            input.hostTextDidChange()
            render()
        }
    }

    /// 当前输入框的标识。头文件里写的是非可选的 UUID，但输入框正在消失时系统会给 nil，
    /// 直接读 textDocumentProxy.documentIdentifier 会在桥接成 UUID 时崩溃（2026-11-12 在模拟器里遇到：
    /// 点「收起键盘」后再点输入框，textDidChange 里读到 nil）。所以按 Objective-C 的方式读出可选值
    private var currentDocumentID: UUID? {
        let getter = #selector(getter: UITextDocumentProxy.documentIdentifier)
        guard let proxy = textDocumentProxy as? NSObject, proxy.responds(to: getter) else { return nil }
        return proxy.perform(getter)?.takeUnretainedValue() as? UUID
    }

    private func currentTraits() -> FieldTraits {
        let proxy = textDocumentProxy
        return FieldTraits(keyboardType: proxy.keyboardType ?? .default,
                           returnKeyType: proxy.returnKeyType ?? .default)
    }

    private func startField() {
        traits = currentTraits()
        documentID = currentDocumentID
        rootView.overrideUserInterfaceStyle = textDocumentProxy.keyboardAppearance == .dark ? .dark : .unspecified
        shift.reset()
        page = traits.startPage
        setGridExpanded(false)
        input.beginField(traits)
    }

    // MARK: - 刷新界面

    private func render() {
        guard isViewLoaded else { return }
        let composing = input.isComposing
        let chinese = input.isChineseMode
        let landscape = rootView.landscape

        var config = KeyboardView.Config()
        config.page = page
        config.chineseMode = chinese
        config.shift = shift.mode
        config.returnTitle = traits.returnKeyTitle(isComposing: composing)
        config.returnProminent = traits.returnKeyIsProminent && !composing
        config.showsGlobe = needsInputModeSwitchKey
        config.landscape = landscape
        rootView.keyboard.apply(config)

        rootView.isComposing = composing
        if composing {
            let pager = input.pager
            rootView.strip.show(CompositionModel(view: input.view), landscape: landscape)
            rootView.bar.show(pager.candidates, hasMore: pager.hasMore, landscape: landscape)
            if rootView.isGridExpanded {
                rootView.grid.show(pager.candidates, hasMore: pager.hasMore, landscape: landscape)
            }
        } else {
            setGridExpanded(false)
        }
        let mode = chinese ? "中文" : "英文"
        var status = aiStatus.isEmpty ? mode : "\(mode) · \(aiStatus)"
        if let note = lexiconNote { status = "\(mode) · \(note)" }
        #if DEBUG
        status += Self.memoryStatus
        #endif
        rootView.toolbar.setStatus(status)
    }

    /// 还没有完整词库时，状态栏提示去 App 里下载（比 AI 选词的状态要紧）
    private var lexiconNote: String? {
        guard input.engine != nil else { return nil }
        switch input.engineLexicon {
        case .downloaded, .bundledFull: return nil
        case .fallback: return "基础词库，打开 AI 拼音 App 下载完整词库"
        case nil: return "还没有中文词库，打开 AI 拼音 App 下载"
        }
    }

    #if DEBUG
    /// 调试构建在状态栏末尾显示「已用 / 上限」（MB）。已用就是系统判定键盘扩展是否超限的 phys_footprint，
    /// 上限 = 已用 + os_proc_available_memory()。真机上一眼能看出离被系统杀掉还有多远；模拟器没有上限，只显示已用
    private static var memoryStatus: String {
        let megabyte = 1024.0 * 1024.0
        let used = Double(RustEngine.memoryFootprint) / megabyte
        let available = Double(max(0, os_proc_available_memory())) / megabyte
        return available > 0
            ? String(format: " · %.0f/%.0fMB", used, used + available)
            : String(format: " · %.0fMB", used)
    }
    #endif

    /// 展开 / 收起候选网格。InputController 要知道：网格开着时到达的 AI 结果先压着，收起后再刷新
    private func setGridExpanded(_ expanded: Bool) {
        rootView.isGridExpanded = expanded
        input.isGridExpanded = expanded
    }

    private func pickCandidate(at index: Int) {
        // 界面先收起，InputController 等选完词再知道：收起会放出压着的 AI 刷新，不能抢在选词之前改掉下标
        rootView.isGridExpanded = false
        input.selectCandidate(at: index)
        input.isGridExpanded = false
    }
}

// MARK: - 按键

extension KeyboardViewController: KeyboardViewDelegate {
    func keyboardView(_ keyboardView: KeyboardView, didTrigger action: KeyAction) {
        switch action {
        case .letter(let c):
            let letter = shift.isUppercase ? Character(c.uppercased()) : c
            shift.didTypeLetter()
            input.typeLetter(letter)
        case .symbol(let s):
            input.typeSymbol(s)
        case .shift:
            shift.tap(at: CACurrentMediaTime())
            render()
        case .backspace:
            input.backspace()
        case .space:
            input.space()
        case .returnKey:
            input.returnKey()
        case .toggleLanguage:
            input.toggleLanguage()
        case .page(let newPage):
            page = newPage
            shift.reset()
            render()
        case .globe:
            break
        }
    }

    func keyboardViewDidTouchDownKey(_ keyboardView: KeyboardView) {
        feedback.keyDown()
    }

    func keyboardView(_ keyboardView: KeyboardView, globeEvent event: UIEvent?) {
        guard let event else { return }
        handleInputModeList(from: keyboardView, with: event)
    }
}

// MARK: - 候选与音节

extension KeyboardViewController: CandidateBarDelegate, CandidateGridDelegate, CompositionStripDelegate {
    func candidateBar(_ bar: CandidateBarView, didSelect index: Int) {
        pickCandidate(at: index)
    }

    func candidateBar(_ bar: CandidateBarView, isTouching: Bool) {
        input.isTouchingCandidate = isTouching
    }

    func candidateBarNeedsMore(_ bar: CandidateBarView) {
        input.loadMoreCandidates()
    }

    func candidateBarDidToggleExpand(_ bar: CandidateBarView) {
        setGridExpanded(!rootView.isGridExpanded)
        render()
    }

    func candidateGrid(_ grid: CandidateGridView, didSelect index: Int) {
        pickCandidate(at: index)
    }

    func candidateGrid(_ grid: CandidateGridView, isTouching: Bool) {
        input.isTouchingCandidate = isTouching
    }

    func candidateGridNeedsMore(_ grid: CandidateGridView) {
        input.loadMoreCandidates()
    }

    func compositionStrip(_ strip: CompositionStripView, didTapSyllable index: Int) {
        input.tapSyllable(at: index)
    }
}
