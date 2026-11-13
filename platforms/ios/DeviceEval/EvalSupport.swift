import Foundation
import XCTest
import CryptoKit

/// 一条评测用例：上文、期望的首选、拼音（与 data/eval/*.tsv 的三列一致）
struct EvalCase: Codable, Equatable {
    let context: String
    let target: String
    let pinyin: String
}

enum EvalCases {
    private final class Marker {}

    /// 读测试包里 eval 目录下的用例文件（project.yml 把仓库的 data/eval 作为文件夹资源拷进来）
    static func load(_ name: String) throws -> [EvalCase] {
        let bundle = Bundle(for: Marker.self)
        guard let url = bundle.url(forResource: name, withExtension: "tsv", subdirectory: "eval") else {
            throw XCTSkip("测试包里没有 eval/\(name).tsv")
        }
        let text = try String(contentsOf: url, encoding: .utf8)
        return text.split(separator: "\n", omittingEmptySubsequences: true).compactMap { line in
            guard !line.hasPrefix("#") else { return nil }
            let parts = line.split(separator: "\t", omittingEmptySubsequences: false)
            guard parts.count >= 3 else { return nil }
            return EvalCase(context: String(parts[0]),
                            target: parts[1].trimmingCharacters(in: .whitespaces),
                            pinyin: parts[2].trimmingCharacters(in: .whitespaces))
        }
    }
}

/// 一条用例的结果
struct EvalOutcome: Codable {
    let pinyin: String
    let context: String
    let target: String
    let top: String
    let tag: String
    let correct: Bool
    /// 目标在候选里的位置（全局下标，第 0 页没有就往后找）；nil 表示不在候选里
    let targetIndex: Int?
    /// 最后一个字母按下到 AI 结果落定的毫秒数（没有 AI 时为 0）
    let waitMs: Double
    /// 逐键处理耗时里最慢的一次（毫秒）
    let worstKeyMs: Double
}

/// 「教会的词」测试（EvalRunner.runTeach）的结果。每组按第 1 条的结果归类：
/// 首选不对、目标在候选里，用户点选了目标的组算教过；首选本来就对、空格上屏（没有主动改选）的组没教过；
/// 目标不在候选里的组教不了。各组其余各条照样像用户一样上屏，首选不对就再点选一次（用户再纠正），
/// 所以教过的组只有「换了上文第 1 次」和「其余各条全在首位」量的是只教一次之后的保持。
///
/// 其余各条里目标和第 1 条不同的是反向行（habit_reversal_cases.tsv 每组第 3 条：同音的另一个词，上文明确指向它，
/// 量的是教过之后记忆会不会太强）：不算进上面任何一项，按所在组的归类另放（与 ffi_session --teach 相同）
struct TeachResult {
    /// 每组第 1 条（教会之前）
    var first: [EvalOutcome] = []
    /// 教过的组：每组其余各条（不含反向行）
    var taught: [[EvalOutcome]] = []
    /// 没教过的组：每组其余各条（不含反向行）
    var neverTaught: [[EvalOutcome]] = []
    /// 教不了的组：每组其余各条（不含反向行）
    var unteachable: [[EvalOutcome]] = []
    /// 旧口径：所有组的其余各条（不含反向行，按输入顺序），只作对照
    var later: [EvalOutcome] = []
    /// 反向行，按所在组的归类
    var taughtReversal: [EvalOutcome] = []
    var neverTaughtReversal: [EvalOutcome] = []
    var unteachableReversal: [EvalOutcome] = []

    var hasReversal: Bool {
        !(taughtReversal.isEmpty && neverTaughtReversal.isEmpty && unteachableReversal.isEmpty)
    }

    /// 一组里最多有几条保持行（「其余 N 次全在首位」里的 N）
    var keptRows: Int {
        (taught + neverTaught + unteachable).map(\.count).max() ?? 0
    }

    /// 记下一组：first 是第 1 条，rest 是其余各条（按输入顺序）
    mutating func add(first: EvalOutcome, rest: [EvalOutcome]) {
        self.first.append(first)
        let kept = rest.filter { $0.target == first.target }
        let reversal = rest.filter { $0.target != first.target }
        later += kept
        switch first.targetIndex {
        case .some(0):
            neverTaught.append(kept)
            neverTaughtReversal += reversal
        case .some:
            taught.append(kept)
            taughtReversal += reversal
        case .none:
            unteachable.append(kept)
            unteachableReversal += reversal
        }
    }
}

/// 一次运行（一个引擎配一个 AIController）里的选词往来：引擎发出的选词请求、选词器交回的次数，
/// 和其中真正给了答案（不是 nil）的次数。包在 AIController 接好的两头外面（engine.onChoiceRequest 和
/// AppleModelChooser.deliver），原来的接线照旧。
/// 只数交回的次数不够：AppleModelChooser 对每个被新请求取代的请求、每次超过 1.5 秒都交回 nil，
/// 模型一次也答不上来时交回的次数照样不少
@MainActor
final class ChoiceTally {
    struct Counts: Equatable {
        var requests = 0
        var deliveries = 0
        var answers = 0

        /// 一个答案也没拿到时说明是哪一种情况；拿到过答案返回 nil
        var silenceProblem: String? {
            guard answers == 0 else { return nil }
            if requests == 0 {
                return "引擎一次也没有发出选词请求（选词器没有接上）"
            }
            if deliveries == 0 {
                return "引擎发出 \(requests) 个选词请求，选词器一次也没有交回（比如 AIController 提前释放了，请求没人接）"
            }
            return "引擎发出 \(requests) 个选词请求，选词器交回 \(deliveries) 次，全是空答案（模型一直出错、超时或答不出候选里的词）"
        }

        var line: String {
            "选词请求 \(requests) 次，选词器交回 \(deliveries) 次，其中答案 \(answers) 次"
        }
    }

    private(set) var counts = Counts()

    /// 在 AIController 的 attach 和 apply 之后调用：之后再 apply 会换掉 engine.onChoiceRequest，就不再数请求了。
    /// 引擎没接选词器（onChoiceRequest 为 nil）时不替它装一个，免得引擎平白多出一个不回答的选词器
    init(engine: RustEngine, chooser: AppleModelChooser) {
        if let ask = engine.onChoiceRequest {
            engine.onChoiceRequest = { [self] request in
                counts.requests += 1
                ask(request)
            }
        }
        let deliver = chooser.deliver
        chooser.deliver = { [self] id, text in
            counts.deliveries += 1
            if text != nil { counts.answers += 1 }
            deliver?(id, text)
        }
    }
}

/// 选词器有没有真的答过（testC、testD 用）。任何一项不过，那组数字就只是规则和个人记忆的成绩，不能拿来比较：
/// - 一次运行里选词器一个答案也没交回（ChoiceTally）；
/// - 某一组结果每一条都等到了 waitLimit：选词器一直不交回时，引擎在宿主选词时限
///   （EvalRunner.engineChooserTimeout，4 秒）一到就让 aiPending 转为假，每条都等到这里为止
enum ChooserGuard {
    /// 等待时间那一项的门槛：引擎的宿主选词时限，评测自己的时限（EvalRunner.aiTimeout）更短时取它
    static func waitLimit(runnerTimeout: TimeInterval) -> TimeInterval {
        min(runnerTimeout, EvalRunner.engineChooserTimeout)
    }

    /// 发现的问题（每条一句，空数组表示没有问题）
    static func problems(_ title: String, summaries: [EvalSummary], runs: [(name: String, counts: ChoiceTally.Counts)],
                         runnerTimeout: TimeInterval) -> [String] {
        var problems = runs.compactMap { run in
            run.counts.silenceProblem.map { "\(title) · \(run.name)：\($0)" }
        }
        let limit = waitLimit(runnerTimeout: runnerTimeout)
        let which = limit < EvalRunner.engineChooserTimeout ? "评测自己的时限" : "引擎的宿主选词时限"
        for summary in summaries where summary.total > 0 && summary.minWaitMs >= limit * 1000 - 50 {
            problems.append("\(title) · \(summary.name)：每一条都等到了 \(String(format: "%g", limit)) 秒（\(which)），"
                + "选词器没有在时限内交回过答案")
        }
        return problems
    }
}

/// 一组结果的汇总
struct EvalSummary: Codable {
    let name: String
    let total: Int
    let correct: Int
    /// 等 AI 最短的一条。不小于引擎的宿主选词时限（EvalRunner.engineChooserTimeout，4 秒）说明每一条都等到了时限，
    /// 选词器一次也没在时限内交回（见 ChooserGuard）
    let minWaitMs: Double
    let medianWaitMs: Double
    let p90WaitMs: Double
    let maxWaitMs: Double
    let medianKeyMs: Double
    let worstKeyMs: Double

    init(name: String, outcomes: [EvalOutcome]) {
        self.init(name: name, total: outcomes.count, correct: outcomes.filter(\.correct).count, outcomes: outcomes)
    }

    /// 按组计数：一组里每一条都对，这一组才算对。等待和逐键耗时按各组的全部各条统计
    init(name: String, groups: [[EvalOutcome]]) {
        self.init(name: name, total: groups.count,
                  correct: groups.filter { !$0.isEmpty && $0.allSatisfy(\.correct) }.count,
                  outcomes: groups.flatMap { $0 })
    }

    private init(name: String, total: Int, correct: Int, outcomes: [EvalOutcome]) {
        self.name = name
        self.total = total
        self.correct = correct
        let waits = outcomes.map(\.waitMs).sorted()
        minWaitMs = waits.first ?? 0
        medianWaitMs = Self.percentile(waits, 0.5)
        p90WaitMs = Self.percentile(waits, 0.9)
        maxWaitMs = waits.last ?? 0
        let keys = outcomes.map(\.worstKeyMs).sorted()
        medianKeyMs = Self.percentile(keys, 0.5)
        worstKeyMs = keys.last ?? 0
    }

    static func percentile(_ sorted: [Double], _ p: Double) -> Double {
        guard !sorted.isEmpty else { return 0 }
        let index = min(sorted.count - 1, max(0, Int((Double(sorted.count - 1) * p).rounded())))
        return sorted[index]
    }

    var line: String {
        String(format: "%@: %d/%d (%.1f%%)  等 AI 中位 %.0f ms, p90 %.0f ms, 最慢 %.0f ms；单键最慢 %.1f ms",
               name, correct, total, total == 0 ? 0 : 100.0 * Double(correct) / Double(total),
               medianWaitMs, p90WaitMs, maxWaitMs, worstKeyMs)
    }
}

/// 评测报告：打印到测试日志，同时写进宿主 App 容器的 Library/Caches/DeviceEval/，
/// 真机上用 xcrun devicectl device copy from --domain-type appDataContainer --domain-identifier com.zengji.aipinyin
/// --source Library/Caches/DeviceEval 取回。
final class EvalReport {
    struct Section: Codable {
        let title: String
        let summaries: [EvalSummary]
        let notes: [String]
        let outcomes: [String: [EvalOutcome]]
    }

    let name: String
    private(set) var sections: [Section] = []
    private var environment: [String: String] = [:]

    init(name: String) {
        self.name = name
        let device = ProcessInfo.processInfo
        environment["os"] = device.operatingSystemVersionString
        environment["physicalMemory"] = String(device.physicalMemory)
        environment["processors"] = String(device.activeProcessorCount)
        #if targetEnvironment(simulator)
        environment["simulator"] = "true"
        #endif
    }

    func note(_ key: String, _ value: String) {
        environment[key] = value
    }

    func add(_ title: String, summaries: [EvalSummary], notes: [String] = [], outcomes: [String: [EvalOutcome]] = [:]) {
        sections.append(Section(title: title, summaries: summaries, notes: notes, outcomes: outcomes))
        print("==== \(title)")
        summaries.forEach { print("  " + $0.line) }
        notes.forEach { print("  " + $0) }
    }

    /// 写 JSON 文件并作为测试附件保留
    @discardableResult
    func write(to testCase: XCTestCase) -> URL? {
        struct Payload: Codable {
            let name: String
            let environment: [String: String]
            let sections: [Section]
        }
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(Payload(name: name, environment: environment, sections: sections)) else {
            return nil
        }
        let attachment = XCTAttachment(data: data, uniformTypeIdentifier: "public.json")
        attachment.name = "\(name).json"
        attachment.lifetime = .keepAlways
        testCase.add(attachment)

        let directory = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("DeviceEval", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let stamp = ISO8601DateFormatter().string(from: Date()).replacingOccurrences(of: ":", with: "-")
        let url = directory.appendingPathComponent("\(name)-\(stamp).json")
        try? data.write(to: url, options: .atomic)
        print("报告：\(url.path)")
        return url
    }
}

/// 毫秒计时
@inline(__always)
func elapsedMs(since start: CFAbsoluteTime) -> Double {
    (CFAbsoluteTimeGetCurrent() - start) * 1000
}

func megabytes(_ bytes: UInt64) -> String {
    String(format: "%.1f MiB", Double(bytes) / 1_048_576)
}

/// 流式计算文件的 SHA-256（十六进制小写）；读不了文件时返回 nil
func sha256Hex(of url: URL) -> String? {
    guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
    defer { try? handle.close() }
    var hasher = SHA256()
    while true {
        // read(upToCount:) 读到文件末尾时返回 nil；只有抛错才算读失败
        let chunk: Data?
        do {
            chunk = try autoreleasepool { try handle.read(upToCount: 4 << 20) }
        } catch {
            return nil
        }
        guard let chunk, !chunk.isEmpty else { break }
        hasher.update(data: chunk)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

