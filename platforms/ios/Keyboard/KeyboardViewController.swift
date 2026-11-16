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
    private let pendingCompositions = PendingCompositionStore(fileURL: EngineFactory.pendingCompositionsURL)
    private var lifecycleObservers: [NSObjectProtocol] = []
    /// 宿主曾失活或收起过：下一次重连即使用新 documentIdentifier 也允许回退恢复原输入框的组合
    private var hostInterrupted = false
    private var rootView: KeyboardRootView!
    private var feedback: KeyFeedback!

    private var page: KeyboardPage = .letters
    private var symbolCategory: SymbolCategory = .common
    private var isSymbolPanelLocked = false
    private var shift = ShiftState()
    private var traits = FieldTraits()
    private var settings = IMESettings()
    private var documentID: UUID?
    private var aiStatus = ""
    private var panelNotice: String?
    /// 键盘当前是否处在显示周期内（viewDidAppear 到 viewWillDisappear 之间）
    private var isVisible = false
    /// 键盘正处在弹出动画或已可见状态（viewWillAppear 到 viewWillDisappear 之间）：
    /// 让后台本地模型加载在约 300ms 的键盘上滑动画期间就开始并行执行，而不必白等动画结束后的 viewDidAppear
    private var isAppearingOrVisible = false
    /// 本地模型上一次状态回调时是否已加载：只在「刚加载好」那一下请求按 AI 重排
    private var localModelWasLoaded = false
    /// 快捷设置面板打开的时刻。刚打开的一小段时间内忽略面板上的点按：
    /// 面板盖住了字母键，快速打字时紧跟着的几下不能误改 AI 模式或开关
    private var settingsOpenedAt: CFTimeInterval = 0
    private var settingsOpenGeneration: UInt64 = 0
    private static let panelTouchLockout: CFTimeInterval = 0.4

    private var panelAcceptsTouches: Bool {
        CACurrentMediaTime() - settingsOpenedAt >= Self.panelTouchLockout
    }

    private var effectiveHasFullAccess: Bool {
        #if DEBUG
        if let marker = AppGroup.debugSimulateFullAccessURL,
           FileManager.default.fileExists(atPath: marker.path) {
            return true
        }
        #endif
        return hasFullAccess
    }

    deinit {
        for observer in lifecycleObservers {
            NotificationCenter.default.removeObserver(observer)
        }
    }

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
        rootView.symbolPanel.delegate = self
        rootView.settingsPanel.delegate = self
        rootView.settingsPanel.installLinkHosts(in: self)
        rootView.toolbar.onToggleSettings = { [weak self] in
            guard let self else { return }
            self.feedback.keyDown()
            self.setSettingsExpanded(!self.rootView.isSettingsExpanded)
            self.render()
        }
        rootView.toolbar.onDismiss = { [weak self] in self?.dismissKeyboard() }

        input.output = ProxyTextOutput(controller: self)
        input.onChange = { [weak self] in self?.render() }
        input.onEngineCreated = { [weak self] engine in self?.ai.attach(engine) }
        input.onEngineDropped = { [weak self] in self?.ai.detachEngine() }
        ai.onStatusChange = { [weak self] status in
            guard let self else { return }
            let loadedNow = self.ai.localLoader.state == .loaded
            let localJustLoaded = loadedNow && !self.localModelWasLoaded
            self.localModelWasLoaded = loadedNow
            self.aiStatus = status
            if localJustLoaded, self.input.isComposing {
                self.input.requestAIRefresh()
            }
            self.render()
        }
        let center = NotificationCenter.default
        lifecycleObservers = [
            center.addObserver(forName: .NSExtensionHostWillResignActive, object: nil, queue: .main) { [weak self] note in
                guard let self, self.matchesExtensionContext(note) else { return }
                self.hostInterrupted = true
                self.saveCurrentComposition()
                if self.rootView.isSettingsExpanded {
                    self.setSettingsExpanded(false)
                    self.render()
                }
            },
            center.addObserver(forName: .NSExtensionHostDidBecomeActive, object: nil, queue: .main) { [weak self] note in
                guard let self, self.matchesExtensionContext(note) else { return }
                self.hostDidBecomeActive()
            }
        ]
        pendingCompositions.purgeExpired()

        // 键盘高度由根视图里的内容约束决定（见 KeyboardRootView.updateHeight）
        rootView.updateHeight(landscape: false, width: view.bounds.width)
        render()
    }

    /// 同一键盘扩展进程会同时驻留多个宿主应用的控制器，只响应属于自己宿主会话（extensionContext）的通知
    private func matchesExtensionContext(_ note: Notification) -> Bool {
        if let expected = extensionContext, let sender = note.object as AnyObject? {
            return sender === expected
        }
        return true
    }

    private func hostDidBecomeActive() {
        guard isViewLoaded else { return }
        if currentDocumentID != nil, currentDocumentID != documentID {
            syncField()
        } else {
            // 下拉控制中心等回到同一个输入框：这次中断已经结束，之后从 A 直接点到 B 时仍要排除 A
            hostInterrupted = false
        }
        // 切应用再切回时，若键盘原本开着，iOS 有时只发 DidBecomeActive 而不重走 viewWillAppear / viewDidAppear：
        // 重新同步设置并让 AI 控制器接回（或重试加载）本地模型，无需手动切走输入法再切回
        guard isVisible else { return }
        settings = IMESettings.loadForKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        input.apply(settings)
        feedback.configure(settings: settings, hasFullAccess: hasFullAccess)
        if !input.isComposing {
            input.keyboardWillAppear()
        }
        ai.apply(settings)
        input.ensureEngine()
        if settings.aiMode != .off, input.isComposing {
            input.requestAIRefresh()
        }
        render()
    }

    private func ensureEngineAndAI() {
        ai.apply(settings)
        input.ensureEngine()
        if settings.aiMode != .off, input.isComposing {
            input.requestAIRefresh()
        }
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        isAppearingOrVisible = true
        // settings.json 只有几百字节，读一次很快；未开完全访问但快捷面板改过开关时合并读取本地覆盖配置
        settings = IMESettings.loadForKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        setSettingsExpanded(false)
        input.apply(settings)
        feedback.configure(settings: settings, hasFullAccess: hasFullAccess)
        // 先同步词库和个人词库，再恢复或初始化当前输入框（否则词库变化丢掉旧引擎时会冲掉刚恢复的组合）
        input.keyboardWillAppear()
        syncField()
        // 首帧排版提交后立即在下一拍启动引擎与后台模型加载（与系统约 300ms 的键盘弹出动画并行）
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isAppearingOrVisible else { return }
            self.ensureEngineAndAI()
        }
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        isVisible = true
        isAppearingOrVisible = true

        if currentDocumentID != nil, currentDocumentID != documentID {
            syncField()
        }
        // 屏幕底边的系统手势会推迟最下一行键的 touchesBegan
        view.window?.gestureRecognizers?.forEach { $0.delaysTouchesBegan = false }
        DispatchQueue.main.async { [weak self] in
            guard let self, self.isVisible else { return }
            self.ensureEngineAndAI()
        }
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        isVisible = false
        isAppearingOrVisible = false
        hostInterrupted = true
        saveCurrentComposition()
        documentID = nil
        input.resetComposition()
        ai.keyboardWillHide()
        page = traits.startPage
        isSymbolPanelLocked = false
        setGridExpanded(false)
        setSettingsExpanded(false)
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
        guard let newID = currentDocumentID else {
            // 宿主切后台、截图或输入框收起瞬间 documentIdentifier 可能短暂为 nil：先保存快照，不清空屏幕上的未完成输入
            hostInterrupted = true
            saveCurrentComposition()
            return
        }
        if newID != documentID || currentTraits() != traits {
            // 换了输入框（或同一输入框在消失后重新获得焦点）
            syncField()
        } else {
            hostInterrupted = false
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

    private func saveCurrentComposition() {
        guard let docID = documentID else { return }
        if let snapshot = input.compositionSnapshot() {
            pendingCompositions.save(snapshot, for: docID, traits: traits, contextBefore: input.context.text)
        } else {
            pendingCompositions.remove(for: docID)
        }
    }

    private func syncField() {
        let previousDocID = documentID
        let wasInterrupted = hostInterrupted
        hostInterrupted = false
        saveCurrentComposition()

        let newTraits = currentTraits()
        let newID = currentDocumentID
        traits = newTraits
        // 先把 documentID 置空，避免 beginField 内部 resetComposition 触发 render 时误删即将接管的记录
        documentID = nil
        rootView.overrideUserInterfaceStyle = textDocumentProxy.keyboardAppearance == .dark ? .dark : .unspecified
        shift.reset()
        page = newTraits.startPage
        isSymbolPanelLocked = false
        switch newTraits.keyboardType {
        case .URL, .emailAddress:
            symbolCategory = .network
        case _ where newTraits.startsInEnglish:
            symbolCategory = .english
        default:
            symbolCategory = .common
        }
        setGridExpanded(false)
        setSettingsExpanded(false)
        let snapshot: String?
        if let newID, !newTraits.startsInEnglish {
            // 同一屏幕直接从输入框 A 点到另一个同类型输入框 B 时（previousDocID 非空且未经过收起 / 失活），
            // 排除刚存下的 A，不把 A 的未完成拼音带到 B；等稍后点回 A 时再恢复
            let excluded = (!wasInterrupted && previousDocID != newID) ? previousDocID : nil
            snapshot = pendingCompositions.take(
                for: newID,
                traits: newTraits,
                contextBefore: textDocumentProxy.documentContextBeforeInput,
                excludingFallbackID: excluded
            )
        } else {
            snapshot = nil
        }
        input.beginField(newTraits, restoring: snapshot)
        documentID = newID
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
            if rootView.isSettingsExpanded {
                setSettingsExpanded(false)
            }
            let pager = input.pager
            rootView.strip.show(CompositionModel(view: input.view), landscape: landscape)
            rootView.bar.show(pager.candidates, hasMore: pager.hasMore, landscape: landscape)
            if rootView.isGridExpanded {
                rootView.grid.show(pager.candidates, hasMore: pager.hasMore, landscape: landscape)
            }
        } else {
            if let docID = documentID {
                pendingCompositions.remove(for: docID)
            }
            setGridExpanded(false)
        }
        let mode = chinese ? "中文" : "英文"
        var status = aiStatus.isEmpty ? mode : "\(mode) · \(aiStatus)"
        if let note = lexiconNote { status = "\(mode) · \(note)" }
        #if DEBUG
        status += Self.memoryStatus
        #endif
        rootView.toolbar.setStatus(status)
        let showSettings = !composing && rootView.isSettingsExpanded
        rootView.toolbar.setPanelOpen(showSettings, notice: panelNotice)
        if showSettings {
            rootView.settingsPanel.apply(makeSettingsPanelState(landscape: landscape))
        }
        let showSymbols = page == .symbols
        rootView.isSymbolPanelVisible = showSymbols
        if showSymbols {
            rootView.symbolPanel.apply(.init(category: symbolCategory, isLocked: isSymbolPanelLocked, landscape: landscape))
        }
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

    private func setSettingsExpanded(_ expanded: Bool) {
        panelNotice = nil
        settingsOpenGeneration &+= 1
        if expanded && !rootView.isSettingsExpanded {
            settingsOpenedAt = CACurrentMediaTime()
            rootView.settingsPanel.setLinksInteractive(false)
            let generation = settingsOpenGeneration
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.panelTouchLockout) { [weak self] in
                guard let self,
                      self.settingsOpenGeneration == generation,
                      self.rootView.isSettingsExpanded else { return }
                self.rootView.settingsPanel.setLinksInteractive(true)
            }
        } else if !expanded {
            rootView.settingsPanel.setLinksInteractive(false)
        }
        rootView.isSettingsExpanded = expanded
    }

    private func makeSettingsPanelState(landscape: Bool) -> QuickSettingsPanelView.State {
        let downloaded = ModelCatalog.all.filter(\.isDownloaded)
        let selected = ModelCatalog.find(settings.localModelID)
        let active = (selected?.isDownloaded == true) ? selected : downloaded.first
        let subtitle: String
        if let model = active {
            let shortTitle = model.title.replacingOccurrences(of: "通义", with: "")
            subtitle = downloaded.count > 1 && settings.aiMode == .local
                ? "\(shortTitle) · 切换"
                : shortTitle
        } else {
            subtitle = "去 App 下载"
        }
        return QuickSettingsPanelView.State(
            settings: settings,
            hasFullAccess: effectiveHasFullAccess,
            appleAvailable: ai.chooser.isAvailable,
            localModelSubtitle: subtitle,
            hasDownloadedLocalModel: active != nil,
            landscape: landscape
        )
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
            if isVisible { ai.restoreLocalModelIfYielded() }
            let letter = shift.isUppercase ? Character(c.uppercased()) : c
            shift.didTypeLetter()
            input.typeLetter(letter)
        case .symbol(let s):
            input.typeSymbol(s)
        case .literal(let s):
            input.typeLiteral(s)
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

// MARK: - 快捷设置面板

extension KeyboardViewController: QuickSettingsPanelDelegate {
    func quickSettingsPanel(_ panel: QuickSettingsPanelView, didSelectAIMode mode: IMESettings.AIMode) {
        guard panelAcceptsTouches else { return }
        feedback.keyDown()
        panelNotice = nil
        // 先读最新的设置再改这一项：App 在前台改过的别的设置不能被键盘手里的旧副本覆盖
        settings = IMESettings.loadForKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        switch mode {
        case .local:
            let downloaded = ModelCatalog.all.filter(\.isDownloaded)
            guard !downloaded.isEmpty else {
                // 已开启完全访问时该卡片由原生 SwiftUI Link 直接跳转 aipinyin://ai；走到这里是未开启完全访问
                panelNotice = "尚未下载本地模型，请从主屏幕打开「AI 拼音」下载"
                render()
                return
            }
            if settings.aiMode == .local,
               downloaded.count > 1,
               let currentIdx = downloaded.firstIndex(where: { $0.id == settings.localModelID }) {
                let next = downloaded[(currentIdx + 1) % downloaded.count]
                settings.localModelID = next.id
                panelNotice = "已切换为\(next.title)"
            } else if ModelCatalog.find(settings.localModelID)?.isDownloaded != true {
                settings.localModelID = downloaded[0].id
            }
            settings.aiMode = .local

        case .apple:
            settings.aiMode = .apple
            if !ai.chooser.isAvailable {
                panelNotice = "当前设备或系统设置未启用 Apple 智能"
            }

        case .off:
            settings.aiMode = .off
        }

        settings.saveFromKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        input.apply(settings)
        input.ensureEngine()
        ai.apply(settings)
        render()
    }

    func quickSettingsPanel(_ panel: QuickSettingsPanelView, didToggle item: QuickSettingsToggle) {
        guard panelAcceptsTouches else { return }
        feedback.keyDown()
        panelNotice = nil
        settings = IMESettings.loadForKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        switch item {
        case .emoji:
            settings.emojiCandidates.toggle()
        case .typoCorrection:
            settings.fuzzy.typoCorrection.toggle()
        case .keyClickSound:
            settings.keyClickSound.toggle()
            if settings.keyClickSound && !hasFullAccess {
                panelNotice = "按键音需开启「允许完全访问」后生效"
            }
        case .haptics:
            settings.haptics.toggle()
            if settings.haptics && !hasFullAccess {
                panelNotice = "触感反馈需开启「允许完全访问」后生效"
            }
        }
        settings.saveFromKeyboard(overrideURL: EngineFactory.keyboardSettingsURL)
        input.apply(settings)
        feedback.configure(settings: settings, hasFullAccess: hasFullAccess)
        render()
    }

    func quickSettingsPanelDidTapOpenApp(_ panel: QuickSettingsPanelView) {
        guard panelAcceptsTouches else { return }
        feedback.keyDown()
        // 已开启完全访问时由面板上的原生 SwiftUI Link 直接跳转 aipinyin://settings；未开启时原位提示
        panelNotice = "跳转需开启「允许完全访问」，请从主屏幕打开 AI 拼音"
        render()
    }

    func quickSettingsPanelDidTouchDownLink(_ panel: QuickSettingsPanelView) {
        guard panelAcceptsTouches else { return }
        feedback.keyDown()
    }
}

// MARK: - 分类符号面板

extension KeyboardViewController: SymbolPanelDelegate {
    func symbolPanel(_ panel: SymbolPanelView, didSelectSymbol text: String) {
        input.typeLiteral(text)
        if !isSymbolPanelLocked {
            page = traits.startPage
        }
        render()
    }

    func symbolPanel(_ panel: SymbolPanelView, didSelectCategory category: SymbolCategory) {
        symbolCategory = category
        render()
    }

    func symbolPanelDidToggleLock(_ panel: SymbolPanelView) {
        isSymbolPanelLocked.toggle()
        render()
    }

    func symbolPanelDidTapBack(_ panel: SymbolPanelView) {
        page = traits.startPage
        isSymbolPanelLocked = false
        render()
    }

    func symbolPanelDidTriggerBackspace(_ panel: SymbolPanelView) {
        input.backspace()
    }

    func symbolPanelDidTouchDown(_ panel: SymbolPanelView) {
        feedback.keyDown()
    }
}
