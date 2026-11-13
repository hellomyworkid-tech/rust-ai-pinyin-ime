import Foundation

/// 记录型的假引擎，用来测键盘的按键规则。行为刻意简单、好预测：
/// - 字母进缓冲区；候选是 candidateCount 个 "c0" "c1"…，按 pageSize 分页；
/// - 「加载更多」用 candidates(from:count:)，不改变 pageIndex；
/// - 空格上屏 "<缓冲区>"，commitFullSentence 上屏 "[缓冲区]"，选候选上屏候选文字；
/// - 空闲时空格、回车、退格、其他字符都 passThrough。
final class RecordingEngine: IMEEngine {
    enum Call: Equatable {
        case handle(IMEKey)
        case select(Int)
        case candidates(Int, Int)
        case tapSyllable(Int)
        case commitFullSentence
        case refreshAfterAI
        case setContext(String?)
        case reset
        case deliverChoice(UInt64, String?)
        case apply
        case reloadUserDictionary
    }

    private(set) var calls: [Call] = []
    private(set) var buffer = ""
    private(set) var isEnglishMode = false
    /// 本地模型的加载 / 卸载。LocalModelLoader 在自己的队列上调用，所以单独加锁
    enum ModelEvent: Equatable {
        case load(URL)
        case unload
    }

    private let modelLock = NSLock()
    private var _loadedModel: URL?
    private var _modelEvents: [ModelEvent] = []
    private var _failingModels: Set<URL> = []

    var loadedModel: URL? { modelLock.withLock { _loadedModel } }
    var isLocalModelLoaded: Bool { loadedModel != nil }
    var modelEvents: [ModelEvent] { modelLock.withLock { _modelEvents } }
    var failingModels: Set<URL> {
        get { modelLock.withLock { _failingModels } }
        set { modelLock.withLock { _failingModels = newValue } }
    }
    var candidateCount = 3
    var pageSize = 10
    private var pageIndex = 0

    var onAsyncUpdate: (() -> Void)?
    var onChoiceRequest: ((IMEChoiceRequest) -> Void)?
    var onChoiceCancel: ((UInt64) -> Void)?

    var isComposing: Bool { !buffer.isEmpty }

    var contexts: [String?] {
        calls.compactMap {
            if case .setContext(let c) = $0 { return .some(c) }
            return nil
        }
    }

    var handledKeys: [IMEKey] {
        calls.compactMap {
            if case .handle(let k) = $0 { return k }
            return nil
        }
    }

    func count(_ call: Call) -> Int { calls.filter { $0 == call }.count }

    func handle(_ key: IMEKey) -> IMEResult {
        calls.append(.handle(key))
        if isEnglishMode {
            guard key == .toggleChineseEnglish else { return .passThrough }
            isEnglishMode = false
            return .update(.idle)
        }
        switch key {
        case .character(let c):
            guard c.isLetter, c.isASCII else { return isComposing ? .update(view()) : .passThrough }
            buffer.append(c)
            pageIndex = 0
            return .update(view())
        case .space:
            return isComposing ? commit("<\(buffer)>") : .passThrough
        case .enter:
            return isComposing ? commit(buffer) : .passThrough
        case .backspace:
            guard isComposing else { return .passThrough }
            buffer.removeLast()
            return .update(isComposing ? view() : .idle)
        case .toggleChineseEnglish:
            isEnglishMode = true
            return isComposing ? commit(buffer) : .update(.idle)
        case .escape:
            guard isComposing else { return .passThrough }
            buffer = ""
            return .update(.idle)
        case .pageDown:
            let pages = (candidateCount + pageSize - 1) / pageSize
            pageIndex = min(pageIndex + 1, max(0, pages - 1))
            return .update(view())
        case .pageUp:
            pageIndex = max(0, pageIndex - 1)
            return .update(view())
        case .cursorLeft, .cursorRight:
            return .update(view())
        }
    }

    func selectCandidate(at globalIndex: Int) -> IMEResult {
        calls.append(.select(globalIndex))
        return commit("c\(globalIndex)")
    }

    func candidates(from offset: Int, count: Int) -> [IMECandidate] {
        calls.append(.candidates(offset, count))
        guard isComposing, offset >= 0, count > 0, offset < candidateCount else { return [] }
        return (offset..<min(candidateCount, offset + count)).map(candidate)
    }

    func tapSyllable(at index: Int) -> IMEResult {
        calls.append(.tapSyllable(index))
        var v = view()
        v.activeSyllableIndex = index
        return .update(v)
    }

    func commitFullSentence() -> IMEResult {
        calls.append(.commitFullSentence)
        return isComposing ? commit("[\(buffer)]") : .passThrough
    }

    func currentView() -> IMEView { isComposing ? view() : .idle }

    func refreshAfterAI() -> IMEView {
        calls.append(.refreshAfterAI)
        // 与真引擎一样先回到第 0 页
        pageIndex = 0
        return currentView()
    }

    func setContext(_ textBeforeCursor: String?) {
        calls.append(.setContext(textBeforeCursor))
    }

    func reset() {
        calls.append(.reset)
        buffer = ""
        pageIndex = 0
    }

    func deliverChoice(id: UInt64, text: String?) {
        calls.append(.deliverChoice(id, text))
    }

    func apply(_ settings: IMESettings) {
        calls.append(.apply)
    }

    func reloadUserDictionaryIfChanged() -> Bool {
        calls.append(.reloadUserDictionary)
        return false
    }

    /// 与 Rust 引擎一样：已有模型先卸载；加载失败之后没有模型
    func loadLocalModel(at url: URL) throws {
        try modelLock.withLock {
            _modelEvents.append(.load(url))
            _loadedModel = nil
            if _failingModels.contains(url) { throw CocoaError(.fileReadCorruptFile) }
            _loadedModel = url
        }
    }

    func unloadLocalModel() {
        modelLock.withLock {
            _modelEvents.append(.unload)
            _loadedModel = nil
        }
    }

    /// 测试里模拟「加载失败之后旧模型还留在引擎里」这类本不该出现的状态
    func forceLoadedModel(_ url: URL) {
        modelLock.withLock { _loadedModel = url }
    }

    /// 模拟 AI 结果到达
    func fireAsyncUpdate() {
        onAsyncUpdate?()
    }

    private func commit(_ text: String) -> IMEResult {
        buffer = ""
        pageIndex = 0
        return .commit(text: text, view: .idle)
    }

    private func view() -> IMEView {
        var v = IMEView()
        v.visible = true
        v.preeditText = buffer
        v.pageIndex = pageIndex
        v.pageSize = pageSize
        v.totalCandidates = candidateCount
        let start = pageIndex * pageSize
        v.visibleCandidates = (start..<min(candidateCount, start + pageSize)).map(candidate)
        v.composedFullSentence = buffer.uppercased()
        return v
    }

    private func candidate(_ index: Int) -> IMECandidate {
        IMECandidate(text: "c\(index)", preedit: buffer, consumedLen: buffer.utf8.count, tag: "", aiBoosted: false)
    }
}
