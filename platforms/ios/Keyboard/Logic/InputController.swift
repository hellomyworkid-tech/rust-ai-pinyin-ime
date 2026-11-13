import Foundation

/// 键盘的按键规则，不含界面。界面把按键转成这里的方法调用，再按 onChange 重画。
///
/// 规则：
/// - 中文模式下字母交给引擎；.commit 用 insertText 上屏；.passThrough 时按键做它本来的事
///   （空格插空格，回车插换行，退格删一个字）。
/// - 输入中途按标点或数字：先 commitFullSentence 上屏整句，再插入标点或数字。引擎不处理标点。
/// - 一段输入开始时把上文（最多 64 字）交给 engine.setContext；之后本地维护，不每次按键都去问宿主。
/// - 引擎始终停在第 0 页：候选「加载更多」用 engine.candidates(from:count:)，不翻引擎的页，
///   所以空格上屏的总是用户看到的第一个候选。
/// - AI 结果到达时，手指正按着候选、已经往后加载了候选、或者展开了候选网格，就先压着，
///   等这些都结束（或下一次视图变化）时再刷新，不把用户正在看的列表抽走。
///
/// 所有方法都在主线程调用。
final class InputController {
    private let makeEngine: (LexiconSource?) -> IMEEngine
    /// 现在该用哪份词库（键盘里是 EngineFactory.lexiconSource）
    private let currentLexicon: () -> LexiconSource?
    private(set) var engine: IMEEngine?
    /// 建当前引擎时用的词库。键盘重新出现时词库变了（App 刚下完完整词库）就换引擎
    private(set) var engineLexicon: LexiconSource?
    /// 写入宿主的出口。键盘里是 ProxyTextOutput，测试里是记录型假实现
    var output: TextOutput?

    private(set) var view = IMEView.idle
    private(set) var pager = CandidatePager()
    private(set) var context = ContextTracker()

    /// 状态变了（视图、模式），界面据此重画
    var onChange: (() -> Void)?
    /// 引擎建好时通知外面（接 AI 选词）
    var onEngineCreated: ((IMEEngine) -> Void)?
    /// 旧引擎丢掉了（词库换了），外面据此把 AI 从它身上摘下来；新引擎建好时还会有 onEngineCreated
    var onEngineDropped: (() -> Void)?

    private var settings = IMESettings()
    /// 用户在普通输入框里选定的中 / 英文
    private var prefersChinese = true
    /// 网址、邮箱、数字等输入框强制从英文开始
    private var forcedEnglish = false
    private var pendingAIRefresh = false

    /// 手指正按在某个候选上。这时到达的 AI 结果先压着，松手后再刷新，免得候选在手指下面换掉
    var isTouchingCandidate = false {
        didSet { applyPendingAIRefreshIfPossible() }
    }

    /// 候选网格展开着：用户在浏览候选，AI 结果同样先压着，收起后再刷新
    var isGridExpanded = false {
        didSet { applyPendingAIRefreshIfPossible() }
    }

    init(makeEngine: @escaping (LexiconSource?) -> IMEEngine, currentLexicon: @escaping () -> LexiconSource?) {
        self.makeEngine = makeEngine
        self.currentLexicon = currentLexicon
    }

    /// 测试用：引擎和词库无关
    convenience init(makeEngine: @escaping () -> IMEEngine) {
        self.init(makeEngine: { _ in makeEngine() }, currentLexicon: { nil })
    }

    // MARK: - 状态

    var isChineseMode: Bool {
        if let engine { return !engine.isEnglishMode }
        return wantsChinese
    }

    var isComposing: Bool { engine?.isComposing ?? false }

    private var wantsChinese: Bool { !forcedEnglish && prefersChinese }

    /// 拿到引擎，没有就现建。键盘在首帧画完以后调用一次；在那之前就按了字母也会走到这里
    @discardableResult
    func ensureEngine() -> IMEEngine {
        if let engine { return engine }
        let lexicon = currentLexicon()
        let created = makeEngine(lexicon)
        engine = created
        engineLexicon = lexicon
        created.apply(settings)
        created.onAsyncUpdate = { [weak self] in self?.asyncUpdateArrived() }
        if created.isEnglishMode == wantsChinese {
            _ = created.handle(.toggleChineseEnglish)
        }
        onEngineCreated?(created)
        return created
    }

    func apply(_ newSettings: IMESettings) {
        settings = newSettings
        engine?.apply(newSettings)
    }

    /// 键盘重新出现：
    /// - 词库变了（App 刚下完完整词库，或换了版本）：丢掉旧引擎，下次用到时按新词库重建。
    ///   旧引擎释放时会先把个人词库写完，新引擎读到的是最新的；
    /// - 否则个人词库若被别的键盘进程改过就重新读入，要在用户能上屏之前做，
    ///   不然这个引擎下次写盘会把那边学到的词盖掉。
    /// 引擎还没建时不用管（新建的引擎本来就用最新的词库和个人词库）
    func keyboardWillAppear() {
        guard let engine else { return }
        if currentLexicon() != engineLexicon {
            dropEngine()
            return
        }
        engine.reloadUserDictionaryIfChanged()
    }

    private func dropEngine() {
        resetComposition()
        // 通知外面时旧引擎还活着：AI 那边要先把本地模型从它身上卸掉
        withExtendedLifetime(engine) {
            engine = nil
            engineLexicon = nil
            onEngineDropped?()
        }
    }

    // MARK: - 输入框变化

    /// 换了一个输入框（或键盘重新出现）：清掉输入，按输入框类型决定中英文
    func beginField(_ traits: FieldTraits) {
        resetComposition()
        context.invalidate()
        forcedEnglish = traits.startsInEnglish
        guard let engine else {
            onChange?()
            return
        }
        if engine.isEnglishMode == wantsChinese {
            apply(engine.handle(.toggleChineseEnglish))
        } else {
            onChange?()
        }
    }

    /// 宿主那边的文字或光标变了：本地上文作废，下次开始输入时重读
    func hostTextDidChange() {
        context.invalidate()
    }

    /// 放弃当前输入（键盘收起、换输入框）
    func resetComposition() {
        pendingAIRefresh = false
        if let engine, engine.isComposing { engine.reset() }
        setView(.idle)
    }

    // MARK: - 按键

    /// 字母。大小写已由界面按上档键处理好
    func typeLetter(_ letter: Character) {
        guard isChineseMode else {
            insert(String(letter))
            return
        }
        let engine = ensureEngine()
        if !engine.isComposing { prepareContext(for: engine) }
        apply(engine.handle(.character(letter))) { insert(String(letter)) }
    }

    /// 数字或标点（ASCII）。中文模式输出中文标点
    func typeSymbol(_ ascii: String) {
        let chinese = isChineseMode
        if let engine, engine.isComposing {
            apply(engine.commitFullSentence())
        }
        if chinese, ascii == "\"" || ascii == "'", !context.isFresh {
            // 引号要看上文配对；本地副本不可信时才去读宿主
            context.refresh(from: output?.documentContextBeforeInput)
        }
        insert(PunctuationMap.output(for: ascii, chineseMode: chinese, contextBefore: context.text))
    }

    func space() {
        send(.space) { insert(" ") }
    }

    func returnKey() {
        send(.enter) { insert("\n") }
    }

    func backspace() {
        send(.backspace) { deleteBackward() }
    }

    func toggleLanguage() {
        let engine = ensureEngine()
        let wasEnglish = engine.isEnglishMode
        apply(engine.handle(.toggleChineseEnglish))
        // 强制英文的输入框里临时切换，不改变用户平时的选择。引擎没切过去（还没有中文词库时只能打英文）也不改：
        // 否则词库下完、换了引擎之后会从英文开始
        if !forcedEnglish, engine.isEnglishMode != wasEnglish { prefersChinese = !engine.isEnglishMode }
    }

    /// 点选候选，globalIndex 是 pager.candidates 里的下标
    func selectCandidate(at globalIndex: Int) {
        guard let engine, engine.isComposing else { return }
        apply(engine.selectCandidate(at: globalIndex))
    }

    func tapSyllable(at index: Int) {
        guard let engine, engine.isComposing else { return }
        apply(engine.tapSyllable(at: index))
    }

    /// 往后再取一批候选（一页的量），接在列表后面。不翻引擎的页。取到新候选返回 true
    @discardableResult
    func loadMoreCandidates() -> Bool {
        guard let engine, engine.isComposing, pager.hasMore else { return false }
        let offset = pager.candidates.count
        let more = engine.candidates(from: offset, count: max(1, pager.pageSize))
        pager.append(more, at: offset)
        guard pager.candidates.count > offset else { return false }
        onChange?()
        return true
    }

    // MARK: - AI

    /// AI 结果到了：重排候选（引擎返回的总是第 0 页，往后加载过的候选一并丢掉）
    func refreshAfterAI() {
        pendingAIRefresh = false
        guard let engine, engine.isComposing else { return }
        setView(engine.refreshAfterAI())
    }

    /// 用户正在看候选列表：手指按着候选、往后加载过候选，或者展开了网格
    private var holdsAIRefresh: Bool {
        isTouchingCandidate || isGridExpanded || pager.hasLoadedMore
    }

    private func asyncUpdateArrived() {
        guard engine?.isComposing == true else { return }
        if holdsAIRefresh {
            pendingAIRefresh = true
        } else {
            refreshAfterAI()
        }
    }

    private func applyPendingAIRefreshIfPossible() {
        guard pendingAIRefresh, !holdsAIRefresh else { return }
        refreshAfterAI()
    }

    // MARK: - 内部

    private func send(_ key: IMEKey, passThrough: () -> Void) {
        guard let engine, isChineseMode || engine.isComposing else {
            passThrough()
            onChange?()
            return
        }
        apply(engine.handle(key), passThrough: passThrough)
    }

    private func prepareContext(for engine: IMEEngine) {
        if !context.isFresh {
            context.refresh(from: output?.documentContextBeforeInput)
        }
        // 刚问过宿主：给 nil（空输入框通常如此）就是没有上文。传 nil 会让引擎沿用之前输入框里的上屏历史
        engine.setContext(context.text ?? "")
    }

    private func apply(_ result: IMEResult, passThrough: () -> Void = {}) {
        switch result {
        case .update(let newView):
            setView(newView)
        case .commit(let text, let newView):
            insert(text)
            setView(newView)
        case .passThrough:
            passThrough()
            onChange?()
        }
    }

    private func setView(_ newView: IMEView) {
        view = newView
        pager.apply(newView)
        if pendingAIRefresh {
            if engine?.isComposing != true {
                // 输入已经结束，压着的结果没用了
                pendingAIRefresh = false
            } else if !holdsAIRefresh, let engine {
                // 新视图回到了第一页、用户也不在看列表了：把压着的 AI 结果补上
                pendingAIRefresh = false
                let refreshed = engine.refreshAfterAI()
                view = refreshed
                pager.apply(refreshed)
            }
        }
        onChange?()
    }

    private func insert(_ text: String) {
        guard !text.isEmpty else { return }
        output?.insertText(text)
        context.didInsert(text)
    }

    private func deleteBackward() {
        output?.deleteBackward()
        context.didDeleteBackward()
    }
}
