import Foundation

/// 界面开发和测试用的假引擎。行为尽量贴近 Rust 状态机（视图形状与 Tests/Fixtures 一致），
/// 词全部来自 MockLexicon：
/// - 拼音串按 MockSegmenter 切成音节，填 syllableSpans；
/// - 候选 = 剩余部分的整句（或词表整词）+ 前缀词，分页给出；空格和 AI 刷新先回到第 0 页；
/// - 选了只覆盖前半段的候选时把这几个音节「锁定」，继续选后面的，全部锁定后上屏；
/// - 点音节进入「这个音节的选词」；
/// - 模拟 AI：词条带 aiPick 时，约 250ms 后把它提到首位并回调 onAsyncUpdate；
/// - 设置了 onChoiceRequest 时，为整段候选发选词请求，deliverChoice 交回的答案提到首位。
final class MockEngine: IMEEngine {
    typealias Scheduler = (_ delay: TimeInterval, _ work: @escaping () -> Void) -> Void

    static let mainQueueScheduler: Scheduler = { delay, work in
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    /// 与 Rust 侧默认一致
    static let defaultPageSize = 30
    /// 发给选词请求的候选最多几个
    static let maxChoiceCandidates = 5

    let pageSize: Int
    /// 模拟 AI 结果晚到多久
    let aiDelay: TimeInterval
    private let schedule: Scheduler

    var onAsyncUpdate: (() -> Void)?
    var onChoiceRequest: ((IMEChoiceRequest) -> Void)?
    var onChoiceCancel: ((UInt64) -> Void)?

    private(set) var isEnglishMode = false
    private(set) var isLocalModelLoaded = false
    private(set) var context: String?
    private(set) var settings = IMESettings()

    private struct ByteRange: Hashable {
        let start: Int
        let end: Int
    }

    /// 一个候选和它覆盖的音节下标
    private struct Ranked {
        var candidate: IMECandidate
        let range: Range<Int>
    }

    private var buffer = ""
    private var pieces: [MockSegmenter.Piece] = []
    /// 用户已选定的音节文字。多字词选定时按字分到各音节
    private var pins: [ByteRange: String] = [:]
    private var activeSyllable: Int?
    private var pageIndex = 0
    /// 每次输入变化加一，过期的异步结果据此丢弃
    private var generation = 0
    private var aiReady = false
    private var aiChoice: String?
    private var pendingRequestID: UInt64?
    private var lastRequestID: UInt64 = 0
    private var cachedCandidates: [Ranked]?

    init(pageSize: Int = MockEngine.defaultPageSize,
         aiDelay: TimeInterval = 0.25,
         schedule: @escaping Scheduler = MockEngine.mainQueueScheduler) {
        self.pageSize = max(1, pageSize)
        self.aiDelay = aiDelay
        self.schedule = schedule
    }

    var isComposing: Bool { !buffer.isEmpty }

    // MARK: - IMEEngine

    func handle(_ key: IMEKey) -> IMEResult {
        if isEnglishMode {
            guard key == .toggleChineseEnglish else { return .passThrough }
            isEnglishMode = false
            return .update(.idle)
        }
        switch key {
        case .character(let c):
            return type(c)
        case .space:
            guard isComposing else { return .passThrough }
            // 与 Rust 引擎一样：先回到第 0 页，上屏全部候选里的第 0 个（之前翻没翻页都一样）；
            // 没有任何候选时与回车相同
            pageIndex = 0
            guard !rankedCandidates().isEmpty else { return handle(.enter) }
            return selectCandidate(at: 0)
        case .enter:
            guard isComposing else { return .passThrough }
            let raw = buffer
            clearInput()
            return .commit(text: raw, view: .idle)
        case .backspace:
            guard isComposing else { return .passThrough }
            return backspace()
        case .toggleChineseEnglish:
            isEnglishMode = true
            guard isComposing else { return .update(.idle) }
            let raw = buffer
            clearInput()
            return .commit(text: raw, view: .idle)
        case .escape:
            guard isComposing else { return .passThrough }
            clearInput()
            return .update(.idle)
        case .pageUp, .pageDown:
            guard isComposing else { return .passThrough }
            let pages = max(1, (rankedCandidates().count + pageSize - 1) / pageSize)
            pageIndex = key == .pageDown ? min(pageIndex + 1, pages - 1) : max(pageIndex - 1, 0)
            return .update(currentView())
        case .cursorLeft, .cursorRight:
            guard isComposing else { return .passThrough }
            moveActiveSyllable(left: key == .cursorLeft)
            return .update(currentView())
        }
    }

    func selectCandidate(at globalIndex: Int) -> IMEResult {
        guard isComposing else { return .passThrough }
        let ranked = rankedCandidates()
        guard ranked.indices.contains(globalIndex) else { return .update(currentView()) }
        let pick = ranked[globalIndex]
        let chars = Array(pick.candidate.text)
        for (offset, i) in pick.range.enumerated() {
            if chars.count == pick.range.count {
                pin(i, String(chars[offset]))
            } else {
                // 字数和音节数对不上（不应发生）：整个词挂在第一个音节上
                pin(i, offset == 0 ? pick.candidate.text : "")
            }
        }
        activeSyllable = nil
        if firstUnpinned == nil {
            let text = composeTexts().joined()
            clearInput()
            return .commit(text: text, view: .idle)
        }
        inputChanged()
        return .update(currentView())
    }

    func candidates(from offset: Int, count: Int) -> [IMECandidate] {
        let ranked = rankedCandidates()
        guard offset >= 0, count > 0, offset < ranked.count else { return [] }
        return ranked[offset..<min(ranked.count, offset + count)].map(\.candidate)
    }

    func tapSyllable(at index: Int) -> IMEResult {
        guard isComposing, pieces.indices.contains(index) else { return .update(currentView()) }
        // 再点一次当前音节就退出单音节选词
        activeSyllable = activeSyllable == index ? nil : index
        candidatesChanged()
        return .update(currentView())
    }

    func commitFullSentence() -> IMEResult {
        guard isComposing else { return .passThrough }
        let text = composeTexts().joined()
        clearInput()
        return .commit(text: text, view: .idle)
    }

    func currentView() -> IMEView {
        guard isComposing else { return .idle }
        let ranked = rankedCandidates()
        let pages = max(1, (ranked.count + pageSize - 1) / pageSize)
        pageIndex = min(pageIndex, pages - 1)
        let start = pageIndex * pageSize
        let texts = composeTexts()

        var view = IMEView()
        view.visible = true
        view.preeditText = pieces.map(\.code).joined(separator: "'")
        view.pageIndex = pageIndex
        view.pageSize = pageSize
        view.totalCandidates = ranked.count
        view.visibleCandidates = ranked[start..<min(ranked.count, start + pageSize)].map(\.candidate)
        view.selectedCandidateIndex = 0
        view.composedFullSentence = texts.joined()
        view.fullSentenceParts = sentenceParts(texts)
        view.syllableSpans = pieces.enumerated().map { i, piece in
            IMESpan(start: piece.start, end: piece.end, text: texts[i], codeKey: piece.code)
        }
        view.activeSyllableIndex = activeSyllable
        view.cursorBytePos = activeSyllable.map { pieces[$0].start } ?? buffer.utf8.count
        if let k = activeSyllable {
            view.aiSemanticHint = "光标位于音节 '\(pieces[k].code)' (第 \(k + 1)/\(pieces.count) 音节)"
        } else {
            view.aiSemanticHint = aiReady || aiChoice != nil ? "AI 已重排" : "通用语境"
        }
        return view
    }

    func refreshAfterAI() -> IMEView {
        // 与 Rust 引擎一样先回到第 0 页再重排
        pageIndex = 0
        return currentView()
    }

    func setContext(_ textBeforeCursor: String?) {
        context = textBeforeCursor.map { String($0.suffix(ContextTracker.maxLength)) }
    }

    func reset() {
        clearInput()
    }

    func deliverChoice(id: UInt64, text: String?) {
        guard id == pendingRequestID else { return }
        pendingRequestID = nil
        guard let text, rankedCandidates().contains(where: { $0.candidate.text == text }) else { return }
        aiChoice = text
        cachedCandidates = nil
        let current = generation
        schedule(0) { [weak self] in
            guard let self, self.generation == current else { return }
            self.onAsyncUpdate?()
        }
    }

    func apply(_ settings: IMESettings) {
        self.settings = settings
    }

    /// 假引擎没有个人词库文件
    func reloadUserDictionaryIfChanged() -> Bool { false }

    func loadLocalModel(at url: URL) throws {
        // 与 Rust 引擎一样：先卸载已有的，加载失败之后没有模型
        isLocalModelLoaded = false
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw CocoaError(.fileNoSuchFile)
        }
        isLocalModelLoaded = true
    }

    func unloadLocalModel() {
        isLocalModelLoaded = false
    }

    // MARK: - 按键

    private func type(_ c: Character) -> IMEResult {
        if c.isASCII, c.isLetter {
            // 空闲时的大写字母直接上屏（打英文缩写、人名首字母）
            if c.isUppercase, !isComposing { return .passThrough }
            buffer.append(Character(c.lowercased()))
        } else {
            guard isComposing else { return .passThrough }
            // 和 Rust 引擎一样：输入中途的其他字符并进拼音串（见 commit_with_punctuation.json）
            buffer.append(c)
        }
        resegment()
        activeSyllable = nil
        inputChanged()
        return .update(currentView())
    }

    private func backspace() -> IMEResult {
        if let last = pins.keys.max(by: { $0.start < $1.start }) {
            // 先撤销最近的选定，再删字母
            pins[last] = nil
            activeSyllable = nil
            inputChanged()
            return .update(currentView())
        }
        buffer.removeLast()
        guard isComposing else {
            clearInput()
            return .update(.idle)
        }
        resegment()
        activeSyllable = nil
        inputChanged()
        return .update(currentView())
    }

    private func moveActiveSyllable(left: Bool) {
        guard !pieces.isEmpty else { return }
        if left {
            activeSyllable = max(0, (activeSyllable ?? pieces.count) - 1)
        } else if let k = activeSyllable {
            activeSyllable = k + 1 < pieces.count ? k + 1 : nil
        }
        candidatesChanged()
    }

    // MARK: - 状态维护

    private func resegment() {
        pieces = MockSegmenter.segment(buffer)
        let alive = Set(pieces.map { ByteRange(start: $0.start, end: $0.end) })
        pins = pins.filter { alive.contains($0.key) }
    }

    private func clearInput() {
        buffer = ""
        pieces = []
        pins = [:]
        activeSyllable = nil
        pageIndex = 0
        cachedCandidates = nil
        aiReady = false
        aiChoice = nil
        pendingRequestID = nil
        generation += 1
    }

    /// 只换了候选列表（点音节、移光标），AI 状态不变
    private func candidatesChanged() {
        pageIndex = 0
        cachedCandidates = nil
    }

    /// 输入内容变了：作废旧的 AI 结果，重新安排异步工作
    private func inputChanged() {
        generation += 1
        pageIndex = 0
        cachedCandidates = nil
        aiReady = false
        aiChoice = nil
        pendingRequestID = nil
        scheduleAsyncWork()
    }

    private func scheduleAsyncWork() {
        guard isComposing, activeSyllable == nil else { return }
        let current = generation
        if hasAIPick() {
            schedule(aiDelay) { [weak self] in
                guard let self, self.generation == current else { return }
                self.aiReady = true
                self.cachedCandidates = nil
                self.onAsyncUpdate?()
            }
        }
        guard onChoiceRequest != nil else { return }
        schedule(0) { [weak self] in
            guard let self, self.generation == current, let request = self.makeChoiceRequest() else { return }
            self.pendingRequestID = request.id
            self.onChoiceRequest?(request)
        }
    }

    private func makeChoiceRequest() -> IMEChoiceRequest? {
        guard let run = unpinnedRun() else { return nil }
        // 只拿覆盖整段的候选去问，答案才能直接当首选
        let texts = rankedCandidates().filter { $0.range == run }.map(\.candidate.text)
        guard texts.count >= 2 else { return nil }
        lastRequestID += 1
        return IMEChoiceRequest(
            id: lastRequestID,
            context: context ?? "",
            pinyin: pieces[run].map(\.code).joined(separator: "'"),
            candidates: Array(texts.prefix(Self.maxChoiceCandidates))
        )
    }

    // MARK: - 候选

    private func pinned(_ i: Int) -> String? {
        pins[ByteRange(start: pieces[i].start, end: pieces[i].end)]
    }

    private func pin(_ i: Int, _ text: String) {
        pins[ByteRange(start: pieces[i].start, end: pieces[i].end)] = text
    }

    private var firstUnpinned: Int? {
        pieces.indices.first { pinned($0) == nil }
    }

    /// 从第一个没选定的音节开始、连续没选定的一段
    private func unpinnedRun() -> Range<Int>? {
        guard let s = firstUnpinned else { return nil }
        var end = s
        while end < pieces.count, pinned(end) == nil { end += 1 }
        return s..<end
    }

    private func entry(_ range: Range<Int>) -> MockLexicon.Entry? {
        let slice = pieces[range]
        guard slice.allSatisfy(\.isPinyin) else { return nil }
        return MockLexicon.entry(for: slice.map(\.code))
    }

    /// 词表外的单个音节
    private func unitText(_ i: Int) -> String {
        let piece = pieces[i]
        return piece.isPinyin ? MockLexicon.fallbackCandidates(for: piece.code)[0] : piece.code
    }

    /// 每个音节当前对应的文字：选定的用选定的，其余按最长词匹配
    private func composeTexts(useAI: Bool? = nil) -> [String] {
        let ai = useAI ?? aiReady
        var texts = Array(repeating: "", count: pieces.count)
        var i = 0
        while i < pieces.count {
            if let p = pinned(i) {
                texts[i] = p
                i += 1
                continue
            }
            var end = i
            while end < pieces.count, pinned(end) == nil { end += 1 }
            var matched: (Int, MockLexicon.Entry)?
            for j in stride(from: end, to: i, by: -1) {
                if let e = entry(i..<j) {
                    matched = (j, e)
                    break
                }
            }
            if let (j, e) = matched {
                let word = ai ? (e.aiPick ?? e.candidates[0]) : e.candidates[0]
                distribute(word, over: i..<j, into: &texts)
                i = j
            } else {
                texts[i] = unitText(i)
                i += 1
            }
        }
        // 外部选词结果覆盖整段时，整句预览跟着它
        if useAI == nil, activeSyllable == nil, let choice = aiChoice, let run = unpinnedRun(),
           choice.count == run.count {
            distribute(choice, over: run, into: &texts)
        }
        return texts
    }

    private func distribute(_ word: String, over range: Range<Int>, into texts: inout [String]) {
        let chars = Array(word)
        for (offset, i) in range.enumerated() {
            if chars.count == range.count {
                texts[i] = String(chars[offset])
            } else {
                texts[i] = offset == 0 ? word : ""
            }
        }
    }

    private func sentenceParts(_ texts: [String]) -> [IMESentencePart] {
        var parts: [IMESentencePart] = []
        for i in pieces.indices {
            let isPinned = pinned(i) != nil
            if let last = parts.last, last.pinned == isPinned {
                parts[parts.count - 1] = IMESentencePart(text: last.text + texts[i], pinned: isPinned)
            } else {
                parts.append(IMESentencePart(text: texts[i], pinned: isPinned))
            }
        }
        return parts
    }

    private func makeCandidate(_ text: String, _ range: Range<Int>, tag: String) -> Ranked {
        let slice = pieces[range]
        let candidate = IMECandidate(
            text: text,
            preedit: slice.map(\.code).joined(separator: "'"),
            consumedLen: slice.last?.end ?? 0,
            tag: tag,
            aiBoosted: false
        )
        return Ranked(candidate: candidate, range: range)
    }

    private func rankedCandidates() -> [Ranked] {
        if let cachedCandidates { return cachedCandidates }
        let built = buildCandidates()
        cachedCandidates = built
        return built
    }

    private func buildCandidates() -> [Ranked] {
        guard isComposing else { return [] }
        if let k = activeSyllable {
            let piece = pieces[k]
            let words = piece.isPinyin ? singleSyllableWords(piece.code) : [piece.code]
            // 单音节选词：consumedLen 是这个音节的长度，标签是 "(音节)"，与 update_syllable_cursor.json 一致
            return words.map { word in
                let candidate = IMECandidate(text: word, preedit: piece.code, consumedLen: piece.end - piece.start,
                                             tag: "(\(piece.code))", aiBoosted: false)
                return Ranked(candidate: candidate, range: k..<(k + 1))
            }
        }
        guard let run = unpinnedRun() else { return [] }
        var list: [Ranked] = []
        var boosted: String?
        /// 拼出来的整句本来就排第一，AI 改了它也要标记
        var sentenceChangedByAI = false

        // 1. 整段：词表里有整词就列出整词，否则给一个拼出来的整句
        if let e = entry(run) {
            list += e.candidates.map { makeCandidate($0, run, tag: "[词库]") }
            if aiReady, let pick = e.aiPick { boosted = pick }
        } else {
            let sentence = composeTexts()[run].joined()
            list.append(makeCandidate(sentence, run, tag: run.count > 1 ? "[整句]" : "[单字]"))
            if aiReady, sentence != composeTexts(useAI: false)[run].joined() {
                boosted = sentence
                sentenceChangedByAI = true
            }
        }
        // 2. 前缀词，由长到短
        for j in stride(from: run.upperBound - 1, to: run.lowerBound, by: -1) {
            if let e = entry(run.lowerBound..<j) {
                list += e.candidates.map { makeCandidate($0, run.lowerBound..<j, tag: "[前缀选词]") }
            }
        }
        // 去重，保留先出现的
        var seen = Set<String>()
        list = list.filter { seen.insert($0.candidate.text).inserted }

        // 3. AI 结果提到首位并做标记；外部选词结果优先
        if let choice = aiChoice {
            boosted = choice
            sentenceChangedByAI = false
        }
        if let boosted, let index = list.firstIndex(where: { $0.candidate.text == boosted }),
           index > 0 || sentenceChangedByAI {
            var top = list.remove(at: index)
            let c = top.candidate
            top.candidate = IMECandidate(text: c.text, preedit: c.preedit, consumedLen: c.consumedLen,
                                         tag: "[AI 优选]", aiBoosted: true)
            list.insert(top, at: 0)
        }
        return list
    }

    /// 单音节选词的候选：词表里的单字条目，再加上各词条里这个音节位置上的字（如 tian → 天、田）
    private func singleSyllableWords(_ code: String) -> [String] {
        var words = MockLexicon.entry(for: [code])?.candidates ?? []
        for e in MockLexicon.entries where e.syllables.count > 1 {
            for candidate in e.candidates {
                let chars = Array(candidate)
                guard chars.count == e.syllables.count else { continue }
                for (i, syllable) in e.syllables.enumerated() where syllable == code {
                    let char = String(chars[i])
                    if !words.contains(char) { words.append(char) }
                }
            }
        }
        return words.isEmpty ? MockLexicon.fallbackCandidates(for: code) : words
    }

    /// 当前剩余部分里有没有带 aiPick 的词条（决定要不要模拟 AI 晚到）
    private func hasAIPick() -> Bool {
        guard let run = unpinnedRun() else { return false }
        for i in run {
            for j in (i + 1)...run.upperBound where entry(i..<j)?.aiPick != nil {
                return true
            }
        }
        return false
    }
}
