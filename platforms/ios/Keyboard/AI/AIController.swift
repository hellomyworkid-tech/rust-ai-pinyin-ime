import Foundation

/// 按 IMESettings.aiMode 把 AI 选词接到引擎上：
/// - .apple：键盘每次出现时（apply）看一次 Apple 端侧模型能不能用。能用就由 AppleModelChooser 回答
///   engine.onChoiceRequest，并预热；用不了（设备或地区不支持、Apple 智能没开、模型没准备好）而设置里选了
///   本地模型时，改用本地模型，免得 AI 选词整个停掉；什么都没有才只按词频和规则。
///   键盘显示期间可用性变了，要到下次出现才换路线；状态文字按这次定下的路线写；
/// - .local：首帧之后按内存情况加载本地模型，引擎自己用它打分；
/// - .off：都不接。
/// 不是 .local 时，引擎里若还有本地模型（包括加载失败、过期的加载留下的）一律卸掉：引擎在两者都装上时只用本地模型。
@MainActor
final class AIController {
    let chooser: AppleModelChooser
    let localLoader: LocalModelLoader
    private weak var engine: IMEEngine?
    private(set) var settings = IMESettings()
    /// 状态变化（给空闲时的工具栏显示）
    var onStatusChange: ((String) -> Void)?
    /// 设置里选中的本地模型的位置（测试里换掉）
    private let localModelURL: (IMESettings) -> URL?

    /// Apple 模式下这次出现走哪条路
    private enum AppleRoute: Equatable {
        /// 后台正在探测 Apple 端侧模型是否可用（冷启动守护进程休眠时不阻塞主线程）
        case probing
        /// Apple 端侧模型回答选词请求
        case apple
        /// Apple 端侧模型用不了，改用设置里选中的本地模型
        case localFallback(URL)
        /// 都没有，只按词频和规则
        case rules
    }

    /// apply() 时定下的路线。接线和状态文字都按它，不在显示期间实时去读 Apple 端侧模型的可用性：
    /// 否则可用性中途变好后，本地模型一有状态变化（加载完、内存警告卸掉），状态就会说成在用 Apple 端侧模型，
    /// 而选词器其实没有接上
    private var appleRoute = AppleRoute.rules
    private var applyGeneration: UInt64 = 0
    private var probeTask: Task<Void, Never>?
    private var deferredLocalLoadWorkItem: DispatchWorkItem?
    /// `apply(..., deferColdLocalLoad: true)` 在引擎尚未创建前调用时暂存，等紧随其后的 `attach(_:)` 生效
    private var pendingDeferColdLocalLoad = false
    /// 冷启动或从后台重载本地模型时，等待键盘滑入动画与 InputUI 握手完成后再启动加载（秒）
    private let coldLocalLoadDelay: TimeInterval

    init(chooser: AppleModelChooser, localLoader: LocalModelLoader = LocalModelLoader(),
         localModelURL: @escaping (IMESettings) -> URL? = { ModelCatalog.find($0.localModelID)?.localURL },
         coldLocalLoadDelay: TimeInterval = 0.12) {
        self.chooser = chooser
        self.localLoader = localLoader
        self.localModelURL = localModelURL
        self.coldLocalLoadDelay = coldLocalLoadDelay
        localLoader.onStateChange = { [weak self] _ in self?.statusChanged() }
    }

    /// 当用户在 `.local` / `.off` 模式下打开快捷设置面板时，若尚未探测过 Apple 智能可用性，
    /// 在后台异步探测并在完成后通知刷新，绝不在主线程同步读取 `isAvailable`
    func probeAppleAvailabilityIfNeeded(onUpdate: (() -> Void)? = nil) {
        guard chooser.knownAvailability == nil else { return }
        let chooser = self.chooser
        Task { @MainActor in
            _ = await chooser.refreshAvailability()
            onUpdate?()
        }
    }

    /// 根据给定的可用性结果定下 Apple 模式的路线
    private func resolveAppleRoute(_ settings: IMESettings, available: Bool) -> AppleRoute {
        if available { return .apple }
        if settings.localModelID != nil, let url = localModelURL(settings) { return .localFallback(url) }
        return .rules
    }

    convenience init() {
        self.init(chooser: AppleModelChooser(model: AppleChoiceModel(), debounce: AppleModelChooser.keyboardDebounce),
                  localLoader: LocalModelLoader(coordinator: .shared))
    }

    /// 引擎建好后接上
    func attach(_ engine: IMEEngine) {
        self.engine = engine
        chooser.deliver = { [weak engine] id, text in
            engine?.deliverChoice(id: id, text: text)
        }
        // 引擎作废了请求（输入结束等）：中止那次推理，不再交回结果
        engine.onChoiceCancel = { [weak self] id in self?.chooser.cancel(id: id) }
        let deferLoad = pendingDeferColdLocalLoad
        pendingDeferColdLocalLoad = false
        apply(settings, deferColdLocalLoad: deferLoad)
    }

    /// 引擎被丢掉（词库换了）：停掉进行中的请求，清掉选词器为旧引擎记下的状态（新引擎的请求编号从头算）；
    /// 先把本地模型从旧引擎上卸掉，加载记录作废，新引擎 attach 时按设置重新加载
    func detachEngine() {
        pendingDeferColdLocalLoad = false
        deferredLocalLoadWorkItem?.cancel()
        deferredLocalLoadWorkItem = nil
        chooser.reset()
        localLoader.engineReplaced(old: engine)
        engine = nil
    }

    /// 键盘每次出现（首帧之后）调用。
    /// `deferColdLocalLoad == true` 时，若本地模型尚未常驻进程（需冷加载），等待键盘弹出动画完成（~300ms）后再启动后台加载，
    /// 避免刚呼出键盘瞬间抢占 CPU/I/O 与内存余量导致系统切回自带输入法
    func apply(_ newSettings: IMESettings, deferColdLocalLoad: Bool = false) {
        settings = newSettings
        applyGeneration &+= 1
        probeTask?.cancel()
        probeTask = nil
        deferredLocalLoadWorkItem?.cancel()
        deferredLocalLoadWorkItem = nil

        if newSettings.aiMode == .apple {
            if let known = chooser.knownAvailability {
                appleRoute = resolveAppleRoute(newSettings, available: known)
                // 顺便在后台静默刷新一次缓存，供下一次键盘出现使用（绝不改变本次已锁定的 appleRoute）
                let chooser = self.chooser
                Task { _ = await chooser.refreshAvailability() }
            } else {
                // 冷启动首次尚无可用性缓存：先走非阻塞 probing，等后台探测返回后再锁定本次路线
                appleRoute = .probing
                let generation = applyGeneration
                probeTask = Task { [weak self] in
                    guard let self else { return }
                    let available = await self.chooser.refreshAvailability()
                    guard !Task.isCancelled,
                          self.applyGeneration == generation,
                          self.settings.aiMode == .apple else { return }
                    self.appleRoute = self.resolveAppleRoute(self.settings, available: available)
                    if let engine = self.engine {
                        self.wireAppleRoute(into: engine, deferColdLocalLoad: deferColdLocalLoad)
                    }
                    self.statusChanged()
                }
            }
        } else {
            appleRoute = .rules
        }

        defer { statusChanged() }
        guard let engine else {
            pendingDeferColdLocalLoad = deferColdLocalLoad
            return
        }
        pendingDeferColdLocalLoad = false
        switch newSettings.aiMode {
        case .apple:
            wireAppleRoute(into: engine, deferColdLocalLoad: deferColdLocalLoad)
        case .local:
            engine.onChoiceRequest = nil
            chooser.cancel()
            localLoader.keyboardWillAppear()
            scheduleLocalLoad(localModelURL(newSettings), into: engine, deferIfCold: deferColdLocalLoad)
        case .off:
            engine.onChoiceRequest = nil
            chooser.cancel()
            localLoader.unload(into: engine)
        }
    }

    private func scheduleLocalLoad(_ url: URL?, into engine: IMEEngine, deferIfCold: Bool) {
        deferredLocalLoadWorkItem?.cancel()
        deferredLocalLoadWorkItem = nil
        guard deferIfCold,
              coldLocalLoadDelay > 0,
              let url,
              localLoader.hasModelFile(at: url),
              !localLoader.hasResidentModel(at: url, engine: engine) else {
            localLoader.load(url, into: engine)
            return
        }
        let generation = applyGeneration
        let item = DispatchWorkItem { [weak self, weak engine] in
            guard let self, let engine, self.applyGeneration == generation else { return }
            self.deferredLocalLoadWorkItem = nil
            self.localLoader.load(url, into: engine)
        }
        deferredLocalLoadWorkItem = item
        DispatchQueue.main.asyncAfter(deadline: .now() + coldLocalLoadDelay, execute: item)
    }

    private func wireAppleRoute(into engine: IMEEngine, deferColdLocalLoad: Bool) {
        switch appleRoute {
        case .probing:
            engine.onChoiceRequest = nil
        case .apple:
            localLoader.unload(into: engine)
            engine.onChoiceRequest = { [weak self] request in self?.chooser.request(request) }
            chooser.prewarm()
        case .localFallback(let url):
            engine.onChoiceRequest = nil
            chooser.cancel()
            localLoader.keyboardWillAppear()
            scheduleLocalLoad(url, into: engine, deferIfCold: deferColdLocalLoad)
        case .rules:
            localLoader.unload(into: engine)
            engine.onChoiceRequest = nil
        }
    }

    /// 键盘一直可见时，若同进程里的另一个键盘控制器（如弹出的系统搜索层）曾借走本地模型使本控制器回到 .idle，按需重新接回
    func restoreLocalModelIfYielded() {
        guard let engine, localLoader.state == .idle, deferredLocalLoadWorkItem == nil else { return }
        switch settings.aiMode {
        case .local:
            localLoader.load(localModelURL(settings), into: engine)
        case .apple:
            if case .localFallback(let url) = appleRoute {
                localLoader.load(url, into: engine)
            }
        case .off:
            break
        }
    }

    func keyboardWillHide() {
        applyGeneration &+= 1
        pendingDeferColdLocalLoad = false
        deferredLocalLoadWorkItem?.cancel()
        deferredLocalLoadWorkItem = nil
        chooser.cancel()
    }

    func didReceiveMemoryWarning() {
        pendingDeferColdLocalLoad = false
        deferredLocalLoadWorkItem?.cancel()
        deferredLocalLoadWorkItem = nil
        localLoader.didReceiveMemoryWarning(engine: engine)
    }

    /// 给用户看的一句话状态。Apple 模式按 apply() 时定下的路线写，不实时读 Apple 端侧模型的可用性
    var statusText: String {
        switch settings.aiMode {
        case .off:
            return "AI 选词已关闭"
        case .apple:
            switch appleRoute {
            case .probing:
                return "AI 选词准备中…"
            case .apple:
                return "AI 选词 · Apple 端侧模型"
            case .localFallback:
                switch localLoader.state {
                case .loaded: return "AI 选词 · 本地模型（Apple 端侧模型不可用）"
                case .loading: return "Apple 端侧模型不可用，正在加载本地模型…"
                case .idle where deferredLocalLoadWorkItem != nil:
                    return "Apple 端侧模型不可用，正在加载本地模型…"
                // 没下载、加载失败、内存不够没加载或被内存警告卸掉：实际只按词频和规则
                default: return "Apple 端侧模型不可用，按词频选词"
                }
            case .rules:
                return "Apple 端侧模型不可用，按词频选词"
            }
        case .local:
            switch localLoader.state {
            case .loaded: return "AI 选词 · 本地模型"
            case .loading: return "正在加载本地模型…"
            case .missing: return "本地模型还没下载，请在 App 里下载"
            case .failed: return "本地模型加载失败"
            case .skippedLowMemory: return "内存不足，本地模型没有加载"
            case .unloadedForMemory: return "内存紧张，本地模型已卸载"
            case .idle:
                return deferredLocalLoadWorkItem != nil ? "正在加载本地模型…" : "本地模型未加载"
            }
        }
    }

    private func statusChanged() {
        onStatusChange?(statusText)
    }
}
