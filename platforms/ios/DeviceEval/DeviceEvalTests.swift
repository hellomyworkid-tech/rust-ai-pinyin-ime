import XCTest
import FoundationModels

/// 真机评测（也能在模拟器上跑，Apple 端侧模型那几项在 Mac 上会跳过）。
///
/// 这个测试包挂在宿主 App 里运行，所以能读 App Group 里下载好的模型，词库取自 App 包里的键盘扩展。
/// 跑法（手机连上、解锁）：
///
///     xcodebuild -project AIPinyin.xcodeproj -scheme AIPinyinDeviceEval \
///       -destination 'platform=iOS,id=<UDID>' -allowProvisioningUpdates test
///
/// 模型从哪来（App Group 里已经有的直接用）：
/// - 真机：先用 devicectl 把 GGUF 拷进 App 容器的 Library/Caches/ImportedModels/（见 README），
///   评测开始时核对大小和 SHA-256，再挪进 App Group 的模型目录（键盘也就能用了）；
/// - 也可以设 DEVICE_EVAL_DOWNLOAD=1，从 ModelCatalog 的地址下载（每个 400–500MB）；
/// - 模拟器：DEVICE_EVAL_MODEL=<路径[:路径…]> 直接把 Mac 上的 GGUF 拷进 App Group。
///
/// 可选的环境变量（交给 xcodebuild 时加 TEST_RUNNER_ 前缀）：
/// - DEVICE_EVAL_DOWNLOAD=1、DEVICE_EVAL_MODEL=…：见上
/// - DEVICE_EVAL_ROUNDS=<n>：会话模拟的轮数（默认 3）
/// - DEVICE_EVAL_POLICIES=<逗号分隔>：testD 量哪几种 chooser_memory（默认 blocked,contested,always,taught；
///   Apple 模型每次要 0.4 秒多，四种一起跑接近一个测试的 60 分钟上限，可以分几次跑）
/// - DEVICE_EVAL_SESSION_MODEL=<ModelCatalog 的 id>：本地模型会话模拟用哪个模型（默认 qwen2.5-0.5b-q4km）
///
/// 结果打印在测试日志里，同时写进 App 容器的 Library/Caches/DeviceEval/*.json（作为测试附件也会保留）。
/// testF、testG 是评测本身的自检（用假模型或拼出来的结果），不需要下载模型，模拟器上也能跑。
@MainActor
final class DeviceEvalTests: XCTestCase {
    static let caseFiles = [
        "context_cases", "alternating_cases", "alternating_cases_b", "english_cases",
        "long_context_cases", "long_context_mixed_cases",
    ]

    private var environment: [String: String] { ProcessInfo.processInfo.environment }
    private var rounds: Int { Int(environment["DEVICE_EVAL_ROUNDS"] ?? "") ?? 3 }

    override func setUp() async throws {
        executionTimeAllowance = 60 * 60
    }

    // MARK: - 准备

    private func lexiconURL() throws -> URL {
        if let plugins = Bundle.main.builtInPlugInsURL {
            let url = plugins.appendingPathComponent("AIPinyinKeyboard.appex/merged_lexicon.bin")
            if FileManager.default.fileExists(atPath: url.path) { return url }
        }
        throw XCTSkip("宿主 App 里找不到键盘扩展的词库（先跑 scripts/build-rust.sh）")
    }

    /// 个人词库只放在内存里，评测之间互不影响
    private func makeEngine() throws -> RustEngine {
        try RustEngine(lexiconURL: lexiconURL(), userDictionaryURL: nil)
    }

    private func caseSets() -> [(name: String, cases: [EvalCase])] {
        Self.caseFiles.compactMap { name in (try? EvalCases.load(name)).map { (name, $0) } }
    }

    private func appleAvailability() -> String {
        String(describing: SystemLanguageModel.default.availability)
    }

    /// 把模型放进 App Group：先收下 devicectl 拷进来的文件，模拟器从 Mac 上拷，真机按需下载。完成后返回已就位的目录模型
    private func prepareModels() async throws -> [LocalModel] {
        let files = FileManager.default
        let imported = files.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("ImportedModels", isDirectory: true)
        for model in ModelCatalog.all where !model.isDownloaded {
            let source = imported.appendingPathComponent(model.fileName)
            guard files.fileExists(atPath: source.path), let destination = model.localURL else { continue }
            let size = (try? source.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(Int64.init) ?? -1
            let start = CFAbsoluteTimeGetCurrent()
            let hash = sha256Hex(of: source)
            print(String(format: "导入 %@：%lld 字节，SHA-256 用时 %.1f 秒", model.id, size, CFAbsoluteTimeGetCurrent() - start))
            guard size == model.bytes, hash == model.sha256 else {
                XCTFail("\(model.id) 拷进来的文件不对：\(size) 字节，SHA-256 \(hash ?? "读不了")")
                continue
            }
            try files.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? files.removeItem(at: destination)
            try files.moveItem(at: source, to: destination)
        }
        for path in (environment["DEVICE_EVAL_MODEL"] ?? "").split(separator: ":").map(String.init) where !path.isEmpty {
            let source = URL(fileURLWithPath: path)
            guard let model = ModelCatalog.all.first(where: { $0.fileName == source.lastPathComponent }),
                  !model.isDownloaded, let destination = model.localURL else { continue }
            try files.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? files.removeItem(at: destination)
            try files.copyItem(at: source, to: destination)
        }
        if environment["DEVICE_EVAL_DOWNLOAD"] == "1" {
            for model in ModelCatalog.all where !model.isDownloaded {
                guard let destination = model.localURL else { continue }
                print("下载 \(model.title)（\(model.bytes) 字节）…")
                let start = CFAbsoluteTimeGetCurrent()
                let (temporary, response) = try await URLSession.shared.download(from: model.downloadURL)
                let size = (try? temporary.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(Int64.init) ?? -1
                guard (response as? HTTPURLResponse)?.statusCode == 200, size == model.bytes else {
                    XCTFail("\(model.id) 下载结果不对：HTTP \(String(describing: (response as? HTTPURLResponse)?.statusCode))，\(size) 字节")
                    continue
                }
                try files.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
                let part = destination.appendingPathExtension("part")
                try? files.removeItem(at: part)
                try files.moveItem(at: temporary, to: part)
                try? files.removeItem(at: destination)
                try files.moveItem(at: part, to: destination)
                print(String(format: "  完成，用时 %.0f 秒", (CFAbsoluteTimeGetCurrent() - start)))
            }
        }
        return ModelCatalog.all.filter(\.isDownloaded)
    }

    /// 按 AIController 的方式接上本地模型，等它加载完
    private func attachLocalModel(_ model: LocalModel, to engine: RustEngine) async throws -> (AIController, Double) {
        let ai = AIController(chooser: AppleModelChooser(model: AppleChoiceModel()), localLoader: LocalModelLoader())
        ai.attach(engine)
        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = model.id
        let start = CFAbsoluteTimeGetCurrent()
        ai.apply(settings)
        let deadline = start + 120
        while ai.localLoader.state == .loading, CFAbsoluteTimeGetCurrent() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        let loadMs = elapsedMs(since: start)
        guard ai.localLoader.state == .loaded, engine.isLocalModelLoaded else {
            throw XCTSkip("\(model.id) 没有加载：\(ai.localLoader.state)，\(ai.statusText)")
        }
        return (ai, loadMs)
    }

    /// 按 AIController 的方式把选词模型（Apple 端侧模型，或自检用的假模型）接成宿主选词器，
    /// 并在两头数选词往来（ChoiceTally）
    private func attachChooser(_ model: ChoiceModel, debounce: TimeInterval,
                               to engine: RustEngine) -> (AIController, ChoiceTally) {
        let chooser = AppleModelChooser(model: model, debounce: debounce)
        let ai = AIController(chooser: chooser, localLoader: LocalModelLoader(availableMemory: { 0 }))
        ai.attach(engine)
        var settings = IMESettings()
        settings.aiMode = .apple
        ai.apply(settings)
        return (ai, ChoiceTally(engine: engine, chooser: chooser))
    }

    /// 按 AIController 的方式接上 Apple 端侧模型，预热好
    private func attachAppleModel(to engine: RustEngine) async throws -> (AIController, ChoiceTally) {
        let model = AppleChoiceModel()
        guard model.isAvailable else {
            throw XCTSkip("Apple 端侧模型不可用：\(appleAvailability())")
        }
        let attached = attachChooser(model, debounce: AppleModelChooser.keyboardDebounce, to: engine)
        // 预热：键盘出现时就开始，第一次真正用到通常在一两秒之后
        try await Task.sleep(for: .seconds(2))
        return attached
    }

    private func sessionSummaries(_ prefix: String, _ rounds: [[EvalOutcome]]) -> [EvalSummary] {
        rounds.enumerated().map { EvalSummary(name: "\(prefix) 第 \($0.offset + 1) 轮", outcomes: $0.element) }
    }

    private func skewSummaries(_ result: (habit: [EvalOutcome], rare: [EvalOutcome], back: [EvalOutcome])) -> [EvalSummary] {
        [EvalSummary(name: "习惯词连打 8 遍（第 2 遍起）", outcomes: result.habit),
         EvalSummary(name: "随后各自上文里的其余同音词", outcomes: result.rare),
         EvalSummary(name: "再回到习惯词", outcomes: result.back)]
    }

    /// 「教会的词」：44 组，每组同一个目标词、5 个中性上文（与 ffi_session --teach 5 相同）。
    /// 按每组第 1 次的结果分开统计（见 TeachResult）：第 1 次首选不对、点选了目标的组才算教过，
    /// 看换上文第 1 次还在不在首位、其余 4 次是不是全在首位（中途没再纠正）；第 1 次本来就对的组没教过，单列；
    /// 所有组其余各次的合计是旧口径，只作对照
    private func teach(_ engine: RustEngine) async throws -> [EvalSummary] {
        let cases = try EvalCases.load("habit_neutral_cases")
        let result = await EvalRunner(engine: engine, hasAI: true).runTeach(cases, groupSize: 5)
        return teachSummaries(result)
    }

    /// 反向检验：44 组，每组 3 次（与 ffi_session --teach 3 相同）。前 2 次和「教会的词」一样在中性上文里教一个词、
    /// 换上文再看一次；第 3 次换成同音的另一个词，上文明确指向它：教过之后还选不选得回来，记忆太强在这里会错
    private func reversal(_ engine: RustEngine) async throws -> [EvalSummary] {
        let cases = try EvalCases.load("habit_reversal_cases")
        let result = await EvalRunner(engine: engine, hasAI: true).runTeach(cases, groupSize: 3)
        return teachSummaries(result)
    }

    /// 只空格接受过的词（与 ffi_session --teach 5 跑 habit_space_cases 相同）：每组第 1 次是这个词自己明确的上文，
    /// 通常本来就是首选、按空格上屏；之后 4 次换成中性上文。要看的是「第 1 次本来就对、没改选过的组，其余各次」：
    /// 只被接受过、没被主动改选过的词，到了中性上文还在不在首位
    private func spaceAccepted(_ engine: RustEngine) async throws -> [EvalSummary] {
        let cases = try EvalCases.load("habit_space_cases")
        let result = await EvalRunner(engine: engine, hasAI: true).runTeach(cases, groupSize: 5)
        return teachSummaries(result, label: "只空格接受过的词")
    }

    /// 「教会的词」和反向检验的各项汇总。没有反向行时与以前相同；有反向行时名字前加「反向检验 · 」，
    /// 不报旧口径，另报反向行（按所在组的归类）。只有一条保持行时不报「其余全在首位」（就是「换上文第 1 次」）。
    /// 给了 `label` 时名字前加它，也不报旧口径
    private func teachSummaries(_ result: TeachResult, label: String? = nil) -> [EvalSummary] {
        let reversed = result.hasReversal
        let prefix = label.map { $0 + " · " } ?? (reversed ? "反向检验 · " : "")
        var summaries = [
            EvalSummary(name: label != nil ? prefix + "每组第 1 次（上文明确指向它）"
                            : reversed ? prefix + "每组第 1 次（教之前）" : "教会之前（每组第 1 次）", outcomes: result.first),
            EvalSummary(name: prefix + "第 1 次改选过的 \(result.taught.count) 组，换上文第 1 次仍在首位",
                        outcomes: result.taught.compactMap(\.first)),
        ]
        if result.keptRows > 1 {
            summaries.append(EvalSummary(
                name: prefix + "第 1 次改选过的组，其余 \(result.keptRows) 次全在首位（中途没再纠正，按组计）",
                groups: result.taught))
        }
        summaries.append(EvalSummary(name: prefix + "第 1 次本来就对、没改选过的 \(result.neverTaught.count) 组，其余各次",
                                     outcomes: result.neverTaught.flatMap { $0 }))
        if !result.unteachable.isEmpty {
            summaries.append(EvalSummary(name: prefix + "目标不在候选里的 \(result.unteachable.count) 组，其余各次",
                                         outcomes: result.unteachable.flatMap { $0 }))
        }
        guard reversed else {
            if label == nil {
                summaries.append(EvalSummary(name: "所有组的其余各次合计（旧口径）", outcomes: result.later))
            }
            return summaries
        }
        summaries.append(EvalSummary(name: prefix + "改选过的组，换成同音的另一个词、上文明确指向它",
                                     outcomes: result.taughtReversal))
        summaries.append(EvalSummary(name: prefix + "没改选过的组，换成同音的另一个词、上文明确指向它",
                                     outcomes: result.neverTaughtReversal))
        if !result.unteachableReversal.isEmpty {
            summaries.append(EvalSummary(name: prefix + "目标不在候选里的组，换成同音的另一个词、上文明确指向它",
                                         outcomes: result.unteachableReversal))
        }
        return summaries
    }

    /// 选词器没有真的答过（见 ChooserGuard）：某次运行一个答案也没拿到（比如 AIController 提前释放了，
    /// 选词请求没人接），或者某一组结果每一条都等到了引擎的宿主选词时限。这些数字只是规则和个人记忆的成绩，
    /// 不能拿来比较
    private func failIfAINeverAnswered(_ title: String, _ summaries: [EvalSummary],
                                       runs: [(name: String, counts: ChoiceTally.Counts)], timeout: TimeInterval) {
        for problem in ChooserGuard.problems(title, summaries: summaries, runs: runs, runnerTimeout: timeout) {
            XCTFail(problem)
        }
    }

    // MARK: - 评测

    /// 只有规则重排：基线，也量逐键耗时
    func testA_RulesOnly() async throws {
        let report = EvalReport(name: "rules")
        let runner = EvalRunner(engine: try makeEngine(), hasAI: false)
        var summaries: [EvalSummary] = []
        var outcomes: [String: [EvalOutcome]] = [:]
        for set in caseSets() {
            let result = await runner.runAll(set.cases)
            summaries.append(EvalSummary(name: set.name, outcomes: result))
            outcomes[set.name] = result
        }
        report.add("只有规则重排", summaries: summaries,
                   notes: ["进程内存 " + megabytes(RustEngine.memoryFootprint)], outcomes: outcomes)
        report.write(to: self)
    }

    /// 本地模型：每个已下载的目录模型各跑一遍（第一次见到每条用例，没有个人记忆）
    func testB_LocalModels() async throws {
        guard RustEngine.supportsLocalModel else { throw XCTSkip("这个构建不含本地模型") }
        let models = try await prepareModels()
        guard !models.isEmpty else {
            throw XCTSkip("App Group 里没有下载好的模型：先在 App 里下载，或设置 DEVICE_EVAL_DOWNLOAD=1 / DEVICE_EVAL_MODEL")
        }
        let report = EvalReport(name: "local-models")
        for model in models {
            let engine = try makeEngine()
            let before = RustEngine.memoryFootprint
            let (ai, loadMs) = try await attachLocalModel(model, to: engine)
            let afterLoad = RustEngine.memoryFootprint
            var peak = afterLoad
            let runner = EvalRunner(engine: engine, hasAI: true)
            var summaries: [EvalSummary] = []
            var outcomes: [String: [EvalOutcome]] = [:]
            for set in caseSets() {
                let result = await runner.runAll(set.cases)
                peak = max(peak, RustEngine.memoryFootprint)
                summaries.append(EvalSummary(name: set.name, outcomes: result))
                outcomes[set.name] = result
            }
            report.add("本地模型 \(model.id)", summaries: summaries, notes: [
                String(format: "加载 %.0f ms", loadMs),
                "进程内存：加载前 \(megabytes(before))，加载后 \(megabytes(afterLoad))，评测中最高 \(megabytes(peak))",
            ], outcomes: outcomes)
            var off = IMESettings()
            off.aiMode = .off
            ai.apply(off)
        }
        report.write(to: self)
    }

    /// Apple 端侧模型（经 AppleModelChooser，和键盘里一样带 120ms 防抖）
    func testC_AppleModel() async throws {
        let engine = try makeEngine()
        let report = EvalReport(name: "apple-model")
        report.note("availability", appleAvailability())
        let before = RustEngine.memoryFootprint
        let (ai, tally) = try await attachAppleModel(to: engine)
        let runner = EvalRunner(engine: engine, hasAI: true)
        var summaries: [EvalSummary] = []
        var outcomes: [String: [EvalOutcome]] = [:]
        for set in caseSets() {
            let result = await runner.runAll(set.cases)
            summaries.append(EvalSummary(name: set.name, outcomes: result))
            outcomes[set.name] = result
        }
        let counts = tally.counts
        report.add("Apple 端侧模型", summaries: summaries, notes: [
            "进程内存：接模型前 \(megabytes(before))，评测后 \(megabytes(RustEngine.memoryFootprint))",
            "状态：\(ai.statusText)",
            counts.line,
        ], outcomes: outcomes)
        failIfAINeverAnswered("Apple 端侧模型", summaries, runs: [("全部用例", counts)], timeout: runner.aiTimeout)
        report.write(to: self)
    }

    /// Apple 端侧模型给不出分数，个人记忆首选遇到它时怎么办（chooser_memory 四种取值）。
    /// 会话模拟：先在 alternating_cases 上连续输入几轮，再换一批上文（alternating_cases_b），最后做习惯词压力测试、
    /// 教会的词测试、反向检验和只空格接受过的词（各用一个新引擎）。DEVICE_EVAL_POLICIES 可以只量其中几种。
    /// 每个引擎的 AIController 都要留到这种取值测完：引擎的回调只弱引用它，提前释放了，选词请求就没人接，
    /// 四种取值量出来都只是规则和个人记忆的成绩。每个引擎各数一份选词往来，一个答案也没拿到就判失败
    func testD_AppleMemoryPolicies() async throws {
        let a = try EvalCases.load("alternating_cases")
        let b = try EvalCases.load("alternating_cases_b")
        let report = EvalReport(name: "apple-memory-policies")
        report.note("availability", appleAvailability())
        let policies = (environment["DEVICE_EVAL_POLICIES"] ?? "blocked,contested,always,taught")
            .split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        for policy in policies {
            let engine = try makeEngine()
            XCTAssertTrue(engine.configure(#"{"chooser_memory":"\#(policy)"}"#), "引擎不接受 chooser_memory=\(policy)")
            let (ai, tally) = try await attachAppleModel(to: engine)
            let runner = EvalRunner(engine: engine, hasAI: true)
            let roundsA = await runner.runSession(a, rounds: rounds)
            let roundsB = await runner.runSession(b, rounds: 2)
            let skewEngine = try makeEngine()
            XCTAssertTrue(skewEngine.configure(#"{"chooser_memory":"\#(policy)"}"#))
            let (skewAI, skewTally) = try await attachAppleModel(to: skewEngine)
            let skew = await EvalRunner(engine: skewEngine, hasAI: true).runSkewed(a, repeats: 8)
            let teachEngine = try makeEngine()
            XCTAssertTrue(teachEngine.configure(#"{"chooser_memory":"\#(policy)"}"#))
            let (teachAI, teachTally) = try await attachAppleModel(to: teachEngine)
            let taught = try await teach(teachEngine)
            let reversalEngine = try makeEngine()
            XCTAssertTrue(reversalEngine.configure(#"{"chooser_memory":"\#(policy)"}"#))
            let (reversalAI, reversalTally) = try await attachAppleModel(to: reversalEngine)
            let reversed = try await reversal(reversalEngine)
            let spaceEngine = try makeEngine()
            XCTAssertTrue(spaceEngine.configure(#"{"chooser_memory":"\#(policy)"}"#))
            let (spaceAI, spaceTally) = try await attachAppleModel(to: spaceEngine)
            let accepted = try await spaceAccepted(spaceEngine)
            let summaries = sessionSummaries("同一批上文", roundsA) + sessionSummaries("换一批上文", roundsB)
                + skewSummaries(skew) + taught + reversed + accepted
            let runs = [(name: "会话模拟", counts: tally.counts), (name: "习惯词", counts: skewTally.counts),
                        (name: "教会的词", counts: teachTally.counts), (name: "反向检验", counts: reversalTally.counts),
                        (name: "只空格接受过的词", counts: spaceTally.counts)]
            report.add("策略 \(policy)", summaries: summaries, notes: runs.map { "\($0.name)：\($0.counts.line)" })
            failIfAINeverAnswered("策略 \(policy)", summaries, runs: runs, timeout: runner.aiTimeout)
            // 和 testE 一样最后关掉：既让四个 AIController 活到这里，也把选词器从引擎上摘下来
            var off = IMESettings()
            off.aiMode = .off
            for controller in [ai, skewAI, teachAI, reversalAI, spaceAI] {
                controller.apply(off)
            }
        }
        report.write(to: self)
    }

    /// 本地模型的会话模拟（个人记忆作先验，默认策略）
    func testE_LocalModelSession() async throws {
        guard RustEngine.supportsLocalModel else { throw XCTSkip("这个构建不含本地模型") }
        let models = try await prepareModels()
        // DEVICE_EVAL_SESSION_MODEL 选模型（ModelCatalog 的 id），默认 Qwen2.5
        let wanted = environment["DEVICE_EVAL_SESSION_MODEL"] ?? "qwen2.5-0.5b-q4km"
        guard let model = models.first(where: { $0.id == wanted }) ?? models.first else {
            throw XCTSkip("App Group 里没有下载好的模型")
        }
        let a = try EvalCases.load("alternating_cases")
        let b = try EvalCases.load("alternating_cases_b")
        let report = EvalReport(name: "local-model-session")
        let engine = try makeEngine()
        let (ai, _) = try await attachLocalModel(model, to: engine)
        let runner = EvalRunner(engine: engine, hasAI: true)
        let roundsA = await runner.runSession(a, rounds: rounds)
        let roundsB = await runner.runSession(b, rounds: 2)
        let skewEngine = try makeEngine()
        let (skewAI, _) = try await attachLocalModel(model, to: skewEngine)
        let skew = await EvalRunner(engine: skewEngine, hasAI: true).runSkewed(a, repeats: 8)
        let teachEngine = try makeEngine()
        let (teachAI, _) = try await attachLocalModel(model, to: teachEngine)
        let taught = try await teach(teachEngine)
        // 进程内存在反向检验之前读，和以前一样是同时留着 3 个装着模型的引擎时的读数（README 里的数是这样量的）
        let memory = RustEngine.memoryFootprint
        let reversalEngine = try makeEngine()
        let (reversalAI, _) = try await attachLocalModel(model, to: reversalEngine)
        let reversed = try await reversal(reversalEngine)
        let spaceEngine = try makeEngine()
        let (spaceAI, _) = try await attachLocalModel(model, to: spaceEngine)
        let accepted = try await spaceAccepted(spaceEngine)
        report.add("本地模型 \(model.id)",
                   summaries: sessionSummaries("同一批上文", roundsA) + sessionSummaries("换一批上文", roundsB)
                       + skewSummaries(skew) + taught + reversed + accepted,
                   notes: ["进程内存 " + megabytes(memory) + "（反向检验之前，同时留着 3 个装着模型的引擎）"])
        report.write(to: self)
        var off = IMESettings()
        off.aiMode = .off
        for controller in [ai, skewAI, teachAI, reversalAI, spaceAI] {
            controller.apply(off)
        }
    }

    // MARK: - 自检

    /// 护栏自检（ChooserGuard，不需要 Apple 端侧模型）：选词器一个答案也没交回时要报出来，照常作答时不报。
    /// 现在的引擎在宿主选词时限（4 秒）一到就让 aiPending 转为假：选词器一直不交回时每条只等 4 秒，
    /// 只看「每一条都等满了评测的 6 秒」是认不出来的
    func testF_ChooserGuardCatchesASilentChooser() async throws {
        let cases = Array(try EvalCases.load("alternating_cases").prefix(2))
        var off = IMESettings()
        off.aiMode = .off

        // 1. 假模型立即作答：拿到了答案，护栏不报
        let engine = try makeEngine()
        let (ai, tally) = attachChooser(SelfTestChoiceModel(answers: true), debounce: 0, to: engine)
        let runner = EvalRunner(engine: engine, hasAI: true)
        let answered = await runner.runAll(cases)
        XCTAssertGreaterThan(tally.counts.answers, 0, tally.counts.line)
        XCTAssertEqual(ChooserGuard.problems("自检", summaries: [EvalSummary(name: "照常作答", outcomes: answered)],
                                             runs: [("照常作答的选词器", tally.counts)],
                                             runnerTimeout: runner.aiTimeout), [])
        ai.apply(off)

        // 2. 模型每次都答不出（交回 nil）：每条都不用等，等待时间看不出来，只有数答案看得出来
        let emptyEngine = try makeEngine()
        let (emptyAI, emptyTally) = attachChooser(SelfTestChoiceModel(answers: false), debounce: 0, to: emptyEngine)
        let empty = await EvalRunner(engine: emptyEngine, hasAI: true).runAll(cases)
        XCTAssertGreaterThan(emptyTally.counts.deliveries, 0, emptyTally.counts.line)
        XCTAssertEqual(emptyTally.counts.answers, 0, emptyTally.counts.line)
        let emptyProblems = ChooserGuard.problems("自检", summaries: [EvalSummary(name: "空答案的结果", outcomes: empty)],
                                                  runs: [("只交回空答案的选词器", emptyTally.counts)],
                                                  runnerTimeout: runner.aiTimeout)
        XCTAssertTrue(emptyProblems.contains { $0.hasPrefix("自检 · 只交回空答案的选词器：") }, "\(emptyProblems)")
        emptyAI.apply(off)

        // 3. 接好之后 AIController 提前释放（testD 以前出过的错）：请求没人接，一次也不交回
        let silentEngine = try makeEngine()
        weak var released: AIController?
        var silentTally: ChoiceTally?
        do {
            let (controller, tally) = attachChooser(SelfTestChoiceModel(answers: true), debounce: 0, to: silentEngine)
            released = controller
            silentTally = tally
        }
        XCTAssertNil(released, "AIController 应当已经释放了")
        let silentRunner = EvalRunner(engine: silentEngine, hasAI: true)
        let silent = await silentRunner.runAll(cases)
        let silentCounts = try XCTUnwrap(silentTally).counts
        XCTAssertGreaterThan(silentCounts.requests, 0, silentCounts.line)
        XCTAssertEqual(silentCounts.deliveries, 0, silentCounts.line)
        let silentProblems = ChooserGuard.problems("自检", summaries: [EvalSummary(name: "没人接的结果", outcomes: silent)],
                                                   runs: [("没人接的选词器", silentCounts)],
                                                   runnerTimeout: silentRunner.aiTimeout)
        XCTAssertTrue(silentProblems.contains { $0.hasPrefix("自检 · 没人接的选词器：") }, "\(silentProblems)")
        // 每条都等到了引擎的时限（链接的引擎较早时等满评测的 6 秒），两种都在门槛之上
        XCTAssertTrue(silentProblems.contains { $0.hasPrefix("自检 · 没人接的结果：") }, "\(silentProblems)")
        print("没人接时每条等了 \(silent.map { String(format: "%.0f ms", $0.waitMs) })")
        silentEngine.onChoiceRequest = nil

        // 4. 等待时间那一项的门槛是引擎的时限：每一条都等了 4 秒的一组要报；
        //    Apple 选词器自己 1.5 秒超时、交回 nil 的（加上 120ms 防抖）不报
        func waited(_ ms: Double) -> [EvalOutcome] {
            cases.map {
                EvalOutcome(pinyin: $0.pinyin, context: $0.context, target: $0.target, top: "", tag: "", correct: false,
                            targetIndex: nil, waitMs: ms, worstKeyMs: 0)
            }
        }
        let answeredCounts = ChoiceTally.Counts(requests: 2, deliveries: 2, answers: 1)
        let atDeadline = ChooserGuard.problems("自检", summaries: [EvalSummary(name: "等了 4 秒", outcomes: waited(4002))],
                                               runs: [("选词器", answeredCounts)], runnerTimeout: 6)
        XCTAssertEqual(atDeadline.count, 1, "\(atDeadline)")
        XCTAssertEqual(ChooserGuard.problems("自检", summaries: [EvalSummary(name: "Apple 超时", outcomes: waited(1625))],
                                             runs: [("选词器", answeredCounts)], runnerTimeout: 6), [])
        XCTAssertEqual(ChooserGuard.waitLimit(runnerTimeout: 2), 2, "评测自己的时限更短时以它为准")
    }

    /// TeachResult 的分组（不需要引擎）：反向行（目标和第 1 条不同）不算进任何保持的数，按所在组的归类另放
    func testG_TeachResultKeepsReversalRowsApart() {
        func outcome(_ target: String, _ index: Int?) -> EvalOutcome {
            EvalOutcome(pinyin: "shiyan", context: "", target: target, top: index == 0 ? target : "", tag: "",
                        correct: index == 0, targetIndex: index, waitMs: 0, worstKeyMs: 0)
        }
        var result = TeachResult()
        // 教过：第 1 次目标排第 3；换上文保持住了，但上文明确要「实验」时还是给了「誓言」
        result.add(first: outcome("誓言", 2), rest: [outcome("誓言", 0), outcome("实验", 1)])
        // 没教过：第 1 次本来就对
        result.add(first: outcome("誓言", 0), rest: [outcome("誓言", 0), outcome("实验", 0)])
        // 教不了
        result.add(first: outcome("誓言", nil), rest: [outcome("誓言", nil), outcome("实验", 0)])
        XCTAssertEqual(result.first.count, 3)
        XCTAssertEqual(result.taught.map { $0.map(\.target) }, [["誓言"]])
        XCTAssertEqual(result.neverTaught.map { $0.map(\.target) }, [["誓言"]])
        XCTAssertEqual(result.unteachable.map { $0.map(\.target) }, [["誓言"]])
        XCTAssertEqual(result.later.map(\.target), ["誓言", "誓言", "誓言"])
        XCTAssertEqual(result.taughtReversal.map(\.correct), [false])
        XCTAssertEqual(result.neverTaughtReversal.map(\.correct), [true])
        XCTAssertEqual(result.unteachableReversal.map(\.correct), [true])
        XCTAssertTrue(result.hasReversal)
        XCTAssertEqual(result.keptRows, 1)

        // 没有反向行（habit_neutral_cases）：其余各条全是保持行
        var neutral = TeachResult()
        neutral.add(first: outcome("誓言", 2), rest: Array(repeating: outcome("誓言", 0), count: 4))
        XCTAssertFalse(neutral.hasReversal)
        XCTAssertEqual(neutral.keptRows, 4)
        XCTAssertEqual(neutral.taught.first?.count, 4)
        XCTAssertEqual(neutral.later.count, 4)
    }
}

/// 自检用的选词模型：立即作答。answers 为假时总是答不出（交回 nil），像一个一直出错的模型
@MainActor
private final class SelfTestChoiceModel: ChoiceModel {
    private let answers: Bool

    init(answers: Bool) {
        self.answers = answers
    }

    var isAvailable: Bool { true }
    func prewarm() {}

    func choose(_ request: IMEChoiceRequest) async throws -> String? {
        // 答候选里的最后一个：和规则重排的首选多半不同，答案被采纳时看得出来
        answers ? request.candidates.last : nil
    }
}
