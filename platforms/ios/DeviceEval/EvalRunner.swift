import Foundation

/// 用真引擎（RustEngine）逐条输入用例：设上文、逐个字母输入、等 AI 落定、读首选。
/// 所有调用都在主线程：引擎的回调（选词请求、结果通知）经主队列转发，等待期间要让出主线程。
@MainActor
final class EvalRunner {
    let engine: RustEngine
    /// 这一轮有没有接 AI（没有就不等）
    var hasAI: Bool
    /// 引擎对宿主选词的时限，与 crates/ime-ai/src/inprocess.rs 的 HostChooser::DEFAULT_TIMEOUT 相同：
    /// 请求发出后超过它还没有答复，engine.aiPending 就转为假，之后送到的答复被丢弃
    nonisolated static let engineChooserTimeout: TimeInterval = 4
    /// AI 结果最多等多久。Apple 选词器 1.5 秒超时（超时也会交回一个空答案）；选词器连空答案都不交回时
    /// （比如请求没人接），每条都等到引擎的宿主选词时限（engineChooserTimeout，4 秒），aiPending 转为假才算完。
    /// 这里多留 2 秒，只防引擎不按时限转为假（较早的引擎是这样，那时每条都等满这里的 6 秒）
    var aiTimeout: TimeInterval = 6

    init(engine: RustEngine, hasAI: Bool) {
        self.engine = engine
        self.hasAI = hasAI
    }

    /// 输入一条用例并读出首选。不上屏
    func run(_ item: EvalCase) async -> EvalOutcome {
        engine.reset()
        engine.setContext(item.context)
        var worstKey = 0.0
        for letter in item.pinyin {
            let start = CFAbsoluteTimeGetCurrent()
            _ = engine.handle(.character(letter))
            worstKey = max(worstKey, elapsedMs(since: start))
            // 让主队列上排着的回调先跑，和键盘里一次按键一个 run loop 周期的节奏一致
            await Task.yield()
        }
        let start = CFAbsoluteTimeGetCurrent()
        var waited = 0.0
        if hasAI {
            let deadline = start + aiTimeout
            // 先等一个 run loop 周期：选词请求要经后台线程再转到主队列
            try? await Task.sleep(for: .milliseconds(1))
            while engine.aiPending, CFAbsoluteTimeGetCurrent() < deadline {
                try? await Task.sleep(for: .milliseconds(2))
            }
            waited = elapsedMs(since: start)
        }
        let view = engine.refreshAfterAI()
        let target = Self.strip(item.target)
        let first = view.visibleCandidates.first
        var targetIndex = view.visibleCandidates.firstIndex { Self.strip($0.text) == target }
        // 第 0 页里没有，就像候选栏展开那样往后取（与 ffi_session 相同），引擎一直停在第 0 页
        let pageSize = max(1, view.pageSize)
        var offset = pageSize
        while targetIndex == nil, offset < view.totalCandidates {
            let more = engine.candidates(from: offset, count: pageSize)
            guard !more.isEmpty else { break }
            if let found = more.firstIndex(where: { Self.strip($0.text) == target }) {
                targetIndex = offset + found
            }
            offset += more.count
        }
        return EvalOutcome(
            pinyin: item.pinyin,
            context: item.context,
            target: item.target,
            top: first?.text ?? "",
            tag: first?.tag ?? "",
            correct: first.map { Self.strip($0.text) == target } ?? false,
            targetIndex: targetIndex,
            waitMs: waited,
            worstKeyMs: worstKey
        )
    }

    /// 像用户一样上屏：首选对就按空格；不对就点候选里的目标（主动改选）；找不到就取消输入
    func commitLikeUser(_ outcome: EvalOutcome) {
        switch outcome.targetIndex {
        case 0?:
            _ = engine.handle(.space)
        case let index?:
            _ = engine.selectCandidate(at: index)
            if engine.isComposing { _ = engine.commitFullSentence() }
        case nil:
            engine.reset()
        }
    }

    /// 逐条跑一组用例，不上屏（每条都是「第一次见到」）
    func runAll(_ cases: [EvalCase]) async -> [EvalOutcome] {
        var out: [EvalOutcome] = []
        out.reserveCapacity(cases.count)
        for item in cases {
            out.append(await run(item))
        }
        return out
    }

    /// 会话模拟：同一个引擎连续输入并像用户一样上屏，个人记忆逐渐形成。
    /// 每一轮把全部用例换个顺序输入一遍，返回每轮的结果
    func runSession(_ cases: [EvalCase], rounds: Int) async -> [[EvalOutcome]] {
        var all: [[EvalOutcome]] = []
        for round in 0..<rounds {
            var outcomes: [EvalOutcome] = []
            for index in Self.order(cases.count, round: round) {
                let outcome = await run(cases[index])
                commitLikeUser(outcome)
                outcomes.append(outcome)
            }
            all.append(outcomes)
        }
        return all
    }

    /// 「习惯词」压力测试（与 ffi_session --skew 相同）：同一拼音的各用例里，先把第一个连续输入 repeats 遍并上屏
    /// （养成个人记忆），再在各自的上文里输入其余的词，最后回到第一个。
    /// 返回：习惯词第 2 遍起的结果、其余词的结果、回到习惯词的结果
    func runSkewed(_ cases: [EvalCase], repeats: Int) async
        -> (habit: [EvalOutcome], rare: [EvalOutcome], back: [EvalOutcome]) {
        var groups: [[EvalCase]] = []
        for item in cases {
            if let index = groups.firstIndex(where: { $0[0].pinyin == item.pinyin }) {
                groups[index].append(item)
            } else {
                groups.append([item])
            }
        }
        var habit: [EvalOutcome] = []
        var rare: [EvalOutcome] = []
        var back: [EvalOutcome] = []
        for group in groups where group.count >= 2 {
            for repeat_ in 0..<repeats {
                let outcome = await run(group[0])
                commitLikeUser(outcome)
                if repeat_ > 0 { habit.append(outcome) }
            }
            for item in group.dropFirst() {
                let outcome = await run(item)
                commitLikeUser(outcome)
                rare.append(outcome)
            }
            let outcome = await run(group[0])
            commitLikeUser(outcome)
            back.append(outcome)
        }
        return (habit, rare, back)
    }

    /// 「教会的词」测试（与 ffi_session --teach 相同，用 data/eval/habit_neutral_cases.tsv，反向检验用
    /// habit_reversal_cases.tsv）：按文件顺序输入并像用户一样上屏，每 groupSize 条一组（同一拼音，
    /// 同一个目标词、不同的中性上文；反向检验每组最后一条换成同音的另一个词），按每组第 1 条的结果归类，
    /// 目标和第 1 条不同的是反向行，另放，见 TeachResult
    func runTeach(_ cases: [EvalCase], groupSize: Int) async -> TeachResult {
        let size = max(1, groupSize)
        var result = TeachResult()
        for start in stride(from: 0, to: cases.count, by: size) {
            let group = cases[start..<min(start + size, cases.count)]
            let first = await run(group[start])
            commitLikeUser(first)
            var rest: [EvalOutcome] = []
            for item in group.dropFirst() {
                let outcome = await run(item)
                // 首选不对时照样点选目标（用户再纠正一次），会话和真实使用一样；反向行也一样
                commitLikeUser(outcome)
                rest.append(outcome)
            }
            result.add(first: first, rest: rest)
        }
        return result
    }

    /// 第 round 轮的输入顺序：与 crates/ime-ffi/examples/ffi_session.rs 相同的确定性排列
    static func order(_ count: Int, round: Int) -> [Int] {
        guard count > 0 else { return [] }
        let steps = [1, 37, 53, 71, 89, 101, 113, 127]
        var step = steps[round % steps.count]
        while gcd(step, count) != 1 { step += 1 }
        return (0..<count).map { ($0 * step + round * 17) % count }
    }

    private static func gcd(_ a: Int, _ b: Int) -> Int {
        b == 0 ? a : gcd(b, a % b)
    }

    static func strip(_ text: String) -> String {
        text.filter { !$0.isWhitespace }
    }
}
