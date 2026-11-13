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

    init(chooser: AppleModelChooser, localLoader: LocalModelLoader = LocalModelLoader(),
         localModelURL: @escaping (IMESettings) -> URL? = { ModelCatalog.find($0.localModelID)?.localURL }) {
        self.chooser = chooser
        self.localLoader = localLoader
        self.localModelURL = localModelURL
        localLoader.onStateChange = { [weak self] _ in self?.statusChanged() }
    }

    /// 看一次 Apple 端侧模型的可用性，定下 Apple 模式的路线
    private func chooseAppleRoute(_ settings: IMESettings) -> AppleRoute {
        if chooser.isAvailable { return .apple }
        if settings.localModelID != nil, let url = localModelURL(settings) { return .localFallback(url) }
        return .rules
    }

    convenience init() {
        self.init(chooser: AppleModelChooser(model: AppleChoiceModel(), debounce: AppleModelChooser.keyboardDebounce))
    }

    /// 引擎建好后接上
    func attach(_ engine: IMEEngine) {
        self.engine = engine
        chooser.deliver = { [weak engine] id, text in
            engine?.deliverChoice(id: id, text: text)
        }
        // 引擎作废了请求（输入结束等）：中止那次推理，不再交回结果
        engine.onChoiceCancel = { [weak self] id in self?.chooser.cancel(id: id) }
        apply(settings)
    }

    /// 引擎被丢掉（词库换了）：停掉进行中的请求，清掉选词器为旧引擎记下的状态（新引擎的请求编号从头算）；
    /// 先把本地模型从旧引擎上卸掉，加载记录作废，新引擎 attach 时按设置重新加载
    func detachEngine() {
        chooser.reset()
        localLoader.engineReplaced(old: engine)
        engine = nil
    }

    /// 键盘每次出现（首帧之后）调用
    func apply(_ newSettings: IMESettings) {
        settings = newSettings
        // 引擎还没建好时也先定下来：状态文字要用（引擎建好后 attach 会再调一次 apply）
        appleRoute = newSettings.aiMode == .apple ? chooseAppleRoute(newSettings) : .rules
        defer { statusChanged() }
        guard let engine else { return }
        switch newSettings.aiMode {
        case .apple:
            switch appleRoute {
            case .apple:
                localLoader.unload(into: engine)
                engine.onChoiceRequest = { [weak self] request in self?.chooser.request(request) }
                chooser.prewarm()
            case .localFallback(let url):
                engine.onChoiceRequest = nil
                chooser.cancel()
                localLoader.keyboardWillAppear()
                localLoader.load(url, into: engine)
            case .rules:
                localLoader.unload(into: engine)
                engine.onChoiceRequest = nil
            }
        case .local:
            engine.onChoiceRequest = nil
            chooser.cancel()
            localLoader.keyboardWillAppear()
            localLoader.load(ModelCatalog.find(newSettings.localModelID)?.localURL, into: engine)
        case .off:
            engine.onChoiceRequest = nil
            chooser.cancel()
            localLoader.unload(into: engine)
        }
    }

    func keyboardWillHide() {
        chooser.cancel()
    }

    func didReceiveMemoryWarning() {
        guard let engine else { return }
        localLoader.didReceiveMemoryWarning(engine: engine)
    }

    /// 给用户看的一句话状态。Apple 模式按 apply() 时定下的路线写，不实时读 Apple 端侧模型的可用性
    var statusText: String {
        switch settings.aiMode {
        case .off:
            return "AI 选词已关闭"
        case .apple:
            switch appleRoute {
            case .apple:
                return "AI 选词 · Apple 端侧模型"
            case .localFallback:
                switch localLoader.state {
                case .loaded: return "AI 选词 · 本地模型（Apple 端侧模型不可用）"
                case .loading: return "Apple 端侧模型不可用，正在加载本地模型…"
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
            case .idle: return "本地模型未加载"
            }
        }
    }

    private func statusChanged() {
        onStatusChange?(statusText)
    }
}
