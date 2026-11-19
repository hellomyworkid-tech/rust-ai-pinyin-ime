import XCTest

/// AIController 按设置接线，以及本地模型的内存门槛
@MainActor
final class AIWiringTests: XCTestCase {
    private func makeModelFile() throws -> URL {
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("model-\(UUID()).gguf")
        try Data([0]).write(to: url)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    private func waitForState(_ loader: LocalModelLoader, _ state: LocalModelLoader.State) async {
        if loader.state == state { return }
        let reached = expectation(description: "状态变为 \(state)")
        loader.onStateChange = { if $0 == state { reached.fulfill() } }
        await fulfillment(of: [reached], timeout: 2)
    }

    /// 等串行队列上已排的活干完
    private func drain(_ queue: DispatchQueue) async {
        await withCheckedContinuation { continuation in
            queue.async { continuation.resume() }
        }
    }

    // MARK: - AIController

    func testAppleModeAnswersChoiceRequestsWhenAvailable() {
        let model = FakeChoiceModel()
        let ai = AIController(chooser: AppleModelChooser(model: model))
        let engine = RecordingEngine()
        var settings = IMESettings()
        settings.aiMode = .apple
        ai.apply(settings)
        ai.attach(engine)
        XCTAssertNotNil(engine.onChoiceRequest)
        XCTAssertEqual(model.prewarmCount, 1)
        XCTAssertEqual(ai.statusText, "AI 选词 · Apple 端侧模型")
    }

    func testAppleModeWithoutModelLeavesEngineAlone() {
        let model = FakeChoiceModel()
        model.isAvailable = false
        let ai = AIController(chooser: AppleModelChooser(model: model))
        let engine = RecordingEngine()
        ai.attach(engine)
        ai.apply(IMESettings())
        XCTAssertNil(engine.onChoiceRequest)
        XCTAssertEqual(model.prewarmCount, 0)
        XCTAssertTrue(ai.statusText.contains("不可用"))
    }

    func testAppleModeFallsBackToTheSelectedLocalModelWhenAppleIsUnavailable() async throws {
        let model = FakeChoiceModel()
        model.isAvailable = false
        let file = try makeModelFile()
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let ai = AIController(chooser: AppleModelChooser(model: model), localLoader: loader,
                              localModelURL: { $0.localModelID == "picked" ? file : nil })
        let engine = RecordingEngine()
        ai.attach(engine)
        var settings = IMESettings()
        settings.aiMode = .apple
        settings.localModelID = "picked"
        ai.apply(settings)
        XCTAssertEqual(ai.statusText, "Apple 端侧模型不可用，正在加载本地模型…")
        await waitForState(loader, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)
        XCTAssertNil(engine.onChoiceRequest, "Apple 模型用不了，不应发选词请求")
        XCTAssertEqual(model.prewarmCount, 0)
        XCTAssertEqual(ai.statusText, "AI 选词 · 本地模型（Apple 端侧模型不可用）")

        // Apple 端侧模型好了：换回它，本地模型卸掉
        model.isAvailable = true
        ai.apply(settings)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertNotNil(engine.onChoiceRequest)
        XCTAssertEqual(ai.statusText, "AI 选词 · Apple 端侧模型")
    }

    func testAppleModeWithoutAnyModelUsesRules() async throws {
        let model = FakeChoiceModel()
        model.isAvailable = false
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        // 选了模型但文件不在：不加载，状态写按词频选词
        let ai = AIController(chooser: AppleModelChooser(model: model), localLoader: loader,
                              localModelURL: { _ in URL(fileURLWithPath: "/no/such/model.gguf") })
        let engine = RecordingEngine()
        ai.attach(engine)
        var settings = IMESettings()
        settings.aiMode = .apple
        settings.localModelID = "picked"
        ai.apply(settings)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertEqual(loader.state, .missing)
        XCTAssertEqual(ai.statusText, "Apple 端侧模型不可用，按词频选词")
        // 没选模型：同样只按词频
        settings.localModelID = nil
        ai.apply(settings)
        XCTAssertEqual(ai.statusText, "Apple 端侧模型不可用，按词频选词")
    }

    /// 轮询等加载器到某个状态。不像 waitForState 那样换掉 onStateChange：AIController 靠它刷新状态
    private func poll(_ loader: LocalModelLoader, until state: LocalModelLoader.State) async {
        for _ in 0..<200 where loader.state != state {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    /// 键盘显示期间 Apple 端侧模型变得可用：选词器要到下次出现才接上。
    /// 在那之前，本地模型加载完、被内存警告卸掉，状态都要照实说，不能说成在用 Apple 端侧模型
    func testStatusFollowsTheRouteChosenAtAppearanceNotLiveAvailability() async throws {
        let model = FakeChoiceModel()
        model.isAvailable = false
        let file = try makeModelFile()
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let ai = AIController(chooser: AppleModelChooser(model: model), localLoader: loader,
                              localModelURL: { $0.localModelID == "picked" ? file : nil })
        var shown: [String] = []
        ai.onStatusChange = { shown.append($0) }
        let engine = RecordingEngine()
        ai.attach(engine)
        var settings = IMESettings()
        settings.aiMode = .apple
        settings.localModelID = "picked"
        ai.apply(settings)

        // 本地模型还在加载时 Apple 端侧模型好了；加载完成照样报本地模型
        model.isAvailable = true
        await poll(loader, until: .loaded)
        XCTAssertEqual(loader.state, .loaded)
        XCTAssertNil(engine.onChoiceRequest, "这次显示期间选词器没有接上")
        XCTAssertEqual(ai.statusText, "AI 选词 · 本地模型（Apple 端侧模型不可用）")
        XCTAssertEqual(shown.last, "AI 选词 · 本地模型（Apple 端侧模型不可用）")

        // 内存警告卸掉本地模型：实际只剩词频和规则
        ai.didReceiveMemoryWarning()
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertEqual(loader.state, .unloadedForMemory)
        XCTAssertEqual(ai.statusText, "Apple 端侧模型不可用，按词频选词")
        XCTAssertEqual(shown.last, "Apple 端侧模型不可用，按词频选词")
        XCTAssertFalse(shown.contains("AI 选词 · Apple 端侧模型"), "从没接上 Apple 端侧模型，状态不能这么说")

        // 下次出现才换成 Apple 端侧模型
        ai.apply(settings)
        XCTAssertNotNil(engine.onChoiceRequest)
        XCTAssertEqual(ai.statusText, "AI 选词 · Apple 端侧模型")
    }

    /// 只按规则的路线也一样：显示期间 Apple 端侧模型变得可用，状态不跟着变
    func testRulesRouteStatusDoesNotFollowLiveAvailability() {
        let model = FakeChoiceModel()
        model.isAvailable = false
        let ai = AIController(chooser: AppleModelChooser(model: model),
                              localLoader: LocalModelLoader(availableMemory: { .max }),
                              localModelURL: { _ in nil })
        let engine = RecordingEngine()
        ai.attach(engine)
        ai.apply(IMESettings())
        model.isAvailable = true
        XCTAssertNil(engine.onChoiceRequest)
        XCTAssertEqual(ai.statusText, "Apple 端侧模型不可用，按词频选词")
    }

    func testOffAndLocalModesDoNotAskTheChooser() {
        let ai = AIController(chooser: AppleModelChooser(model: FakeChoiceModel()),
                              localLoader: LocalModelLoader(availableMemory: { 0 }))
        let engine = RecordingEngine()
        ai.attach(engine)
        XCTAssertNotNil(engine.onChoiceRequest)
        var settings = IMESettings()
        settings.aiMode = .off
        ai.apply(settings)
        XCTAssertNil(engine.onChoiceRequest)
        XCTAssertEqual(ai.statusText, "AI 选词已关闭")
        settings.aiMode = .local
        settings.localModelID = nil
        ai.apply(settings)
        XCTAssertNil(engine.onChoiceRequest)
        XCTAssertEqual(ai.localLoader.state, .missing)
    }

    /// 端到端：MockEngine 发请求 → 假模型回答 → 引擎把答案提到首位
    func testChoiceFlowsFromEngineThroughChooserBack() async throws {
        let scheduler = ManualScheduler()
        let engine = MockEngine(schedule: scheduler.schedule)
        let model = FakeChoiceModel()
        model.behavior = .answer("你号")
        let ai = AIController(chooser: AppleModelChooser(model: model))
        ai.attach(engine)
        var updates = 0
        engine.onAsyncUpdate = { updates += 1 }

        for c in "nihao" { _ = engine.handle(.character(c)) }
        scheduler.run(upTo: 0)
        XCTAssertEqual(model.requests.count, 0, "请求在模型那边是异步执行的")
        for _ in 0..<50 where scheduler.pending.isEmpty {
            try await Task.sleep(for: .milliseconds(10))
        }
        scheduler.runAll()
        XCTAssertEqual(model.requests.map(\.candidates), [["你好", "拟好", "你号"]])
        XCTAssertEqual(updates, 1)
        let view = engine.refreshAfterAI()
        XCTAssertEqual(view.visibleCandidates.first?.text, "你号")
        XCTAssertEqual(view.visibleCandidates.first?.aiBoosted, true)
    }

    // MARK: - LocalModelLoader

    func testLocalModelLoadsInBackgroundWhenMemoryAllows() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { 200 * 1024 * 1024 })
        let engine = MockEngine()
        loader.load(try makeModelFile(), into: engine)
        XCTAssertEqual(loader.state, .loading, "加载不在调用线程上做")
        await waitForState(loader, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)
    }

    func testLocalModelSkippedBelowMemoryThreshold() throws {
        let loader = LocalModelLoader(availableMemory: { LocalModelLoader.minimumAvailableBytes - 1 })
        let engine = MockEngine()
        loader.load(try makeModelFile(), into: engine)
        XCTAssertEqual(loader.state, .skippedLowMemory)
        XCTAssertFalse(engine.isLocalModelLoaded)
    }

    func testMissingModelFile() {
        let loader = LocalModelLoader(availableMemory: { .max })
        let engine = MockEngine()
        loader.load(URL(fileURLWithPath: "/nonexistent/model.gguf"), into: engine)
        XCTAssertEqual(loader.state, .missing)
        loader.load(nil, into: engine)
        XCTAssertEqual(loader.state, .missing)
    }

    func testMemoryWarningUnloadsAndBlocksReloadUntilNextAppearance() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { 200 * 1024 * 1024 })
        let engine = MockEngine()
        let url = try makeModelFile()
        loader.load(url, into: engine)
        await waitForState(loader, .loaded)

        loader.didReceiveMemoryWarning(engine: engine)
        XCTAssertEqual(loader.state, .unloadedForMemory)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)

        loader.load(url, into: engine)
        XCTAssertEqual(loader.state, .unloadedForMemory, "同一次显示里不再重试")

        loader.keyboardWillAppear()
        loader.load(url, into: engine)
        await waitForState(loader, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)
    }

    func testMemoryWarningDuringLoadEndsUnloaded() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { 200 * 1024 * 1024 })
        let engine = MockEngine()
        loader.load(try makeModelFile(), into: engine)
        loader.didReceiveMemoryWarning(engine: engine)
        await drain(queue)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(loader.state, .unloadedForMemory, "过期的加载结果不能把状态改回已加载")
        XCTAssertFalse(engine.isLocalModelLoaded)
    }

    // MARK: - 换模型、加载失败、模式切换（SW-6、SW-A1）

    /// 模拟真机：引擎里有模型时剩余内存不够再加载一份，卸掉之后才够
    private func realisticMemory(_ engine: RecordingEngine) -> () -> UInt64 {
        { engine.isLocalModelLoaded ? 90 * 1024 * 1024 : 200 * 1024 * 1024 }
    }

    func testSwitchingModelsUnloadsFirstThenChecksMemory() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let engine = RecordingEngine()
        let loader = LocalModelLoader(queue: queue, availableMemory: realisticMemory(engine))
        let a = try makeModelFile()
        let b = try makeModelFile()
        loader.load(a, into: engine)
        await waitForState(loader, .loaded)

        loader.load(b, into: engine)
        XCTAssertEqual(loader.state, .loading, "旧模型还在时量到的内存不算数，不能报内存不足")
        await waitForState(loader, .loaded)
        XCTAssertEqual(engine.loadedModel, b)
        XCTAssertEqual(engine.modelEvents, [.load(a), .unload, .load(b)], "先卸旧的再加载新的")
    }

    func testSwitchingModelsReportsLowMemoryMeasuredAfterUnloading() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let engine = RecordingEngine()
        var memory: UInt64 = 200 * 1024 * 1024
        let loader = LocalModelLoader(queue: queue, availableMemory: { memory })
        loader.load(try makeModelFile(), into: engine)
        await waitForState(loader, .loaded)

        memory = 50 * 1024 * 1024
        loader.load(try makeModelFile(), into: engine)
        await waitForState(loader, .skippedLowMemory)
        XCTAssertFalse(engine.isLocalModelLoaded, "旧模型已经卸掉")
        XCTAssertEqual(engine.modelEvents.count, 2, "内存不够就不加载新模型")
    }

    func testFailedReplacementLeavesNoModel() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let engine = RecordingEngine()
        let loader = LocalModelLoader(queue: queue, availableMemory: realisticMemory(engine))
        let a = try makeModelFile()
        let broken = try makeModelFile()
        engine.failingModels = [broken]
        loader.load(a, into: engine)
        await waitForState(loader, .loaded)
        loader.load(broken, into: engine)
        await waitForState(loader, .failed)
        XCTAssertFalse(engine.isLocalModelLoaded)
        // 失败之后还能换回能用的模型
        loader.load(a, into: engine)
        await waitForState(loader, .loaded)
        XCTAssertEqual(engine.loadedModel, a)
    }

    /// 状态是 .failed 等「不该有模型」的状态，引擎里却有模型：卸载和内存警告照样卸掉
    func testUnloadAndMemoryWarningActWheneverTheEngineHasAModel() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let engine = RecordingEngine()
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let broken = try makeModelFile()
        engine.failingModels = [broken]
        loader.load(broken, into: engine)
        await waitForState(loader, .failed)

        engine.forceLoadedModel(try makeModelFile())
        loader.didReceiveMemoryWarning(engine: engine)
        XCTAssertEqual(loader.state, .unloadedForMemory)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded, "内存警告必须卸掉引擎里的模型")

        loader.keyboardWillAppear()
        engine.forceLoadedModel(try makeModelFile())
        loader.unload(into: engine)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
    }

    /// 换到 Apple 或关闭时，本地模型（哪怕加载失败留下的状态不对）一律卸掉，Apple 选词器不会被本地模型悄悄顶掉
    func testAppleAndOffModesNeverKeepALocalModel() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let engine = RecordingEngine()
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let ai = AIController(chooser: AppleModelChooser(model: FakeChoiceModel()), localLoader: loader)
        ai.attach(engine)
        let broken = try makeModelFile()
        engine.failingModels = [broken]
        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = nil
        ai.apply(settings)
        XCTAssertEqual(loader.state, .missing)

        // 加载失败的状态下引擎里却有模型
        loader.load(broken, into: engine)
        await waitForState(loader, .failed)
        engine.forceLoadedModel(try makeModelFile())
        settings.aiMode = .apple
        ai.apply(settings)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertNotNil(engine.onChoiceRequest)

        // 正常加载完成后换成关闭
        let model = try makeModelFile()
        loader.load(model, into: engine)
        await waitForState(loader, .loaded)
        settings.aiMode = .off
        ai.apply(settings)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertEqual(loader.state, .idle)

        // 加载还在队列里时换成 Apple：加载完也会被随后排队的卸载卸掉
        settings.aiMode = .local
        ai.apply(settings)
        loader.load(model, into: engine)
        XCTAssertEqual(loader.state, .loading)
        settings.aiMode = .apple
        ai.apply(settings)
        await drain(queue)
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertEqual(loader.state, .idle, "过期的加载结果不能把状态改回已加载")
    }

    // MARK: - 引擎作废请求（SW-7）

    func testEngineCancelStopsTheChooserWithoutDelivering() async {
        let model = FakeChoiceModel()
        model.behavior = .hang
        let chooser = AppleModelChooser(model: model)
        let ai = AIController(chooser: chooser, localLoader: LocalModelLoader(availableMemory: { 0 }))
        let engine = RecordingEngine()
        ai.attach(engine)
        let request = IMEChoiceRequest(id: 7, context: "上文", pinyin: "shiyan", candidates: ["实验", "誓言"])
        engine.onChoiceRequest?(request)
        XCTAssertTrue(chooser.isBusy)
        engine.onChoiceCancel?(6)
        XCTAssertTrue(chooser.isBusy, "别的编号不影响")
        engine.onChoiceCancel?(7)
        XCTAssertFalse(chooser.isBusy)
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(engine.calls.contains { if case .deliverChoice = $0 { return true } else { return false } },
                       "引擎作废的请求不交回")
    }

    // MARK: - 同一扩展进程内多控制器协同（跨应用切换不误报内存不足）

    func testPeerControllerReusesModelInsteadOfReportingLowMemory() async throws {
        let queue = DispatchQueue(label: "test.local-model.shared")
        let coordinator = LocalModelCoordinator(queue: queue)
        let engine1 = RecordingEngine()
        let engine2 = RecordingEngine()
        // 模拟真机 177MB 进程上限：任意一个控制器装着模型时，进程剩余内存只剩 80MB（低于加载门槛）
        let sharedMemory: () -> UInt64 = {
            (engine1.isLocalModelLoaded || engine2.isLocalModelLoaded)
                ? 80 * 1024 * 1024
                : 160 * 1024 * 1024
        }
        let modelFile = try makeModelFile()
        let loader1 = LocalModelLoader(coordinator: coordinator, availableMemory: sharedMemory)
        let loader2 = LocalModelLoader(coordinator: coordinator, availableMemory: sharedMemory)
        let ai1 = AIController(chooser: AppleModelChooser(model: FakeChoiceModel()),
                               localLoader: loader1,
                               localModelURL: { _ in modelFile })
        let ai2 = AIController(chooser: AppleModelChooser(model: FakeChoiceModel()),
                               localLoader: loader2,
                               localModelURL: { _ in modelFile })

        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = "qwen3-0.6b-q4km"

        // 1. 先在「AI 拼音」App 的试一试框里加载本地模型
        ai1.attach(engine1)
        ai1.apply(settings)
        await poll(loader1, until: .loaded)
        XCTAssertTrue(engine1.isLocalModelLoaded)
        XCTAssertEqual(ai1.statusText, "AI 选词 · 本地模型")

        // 2. 切到别的 App（如搜索框 / 微信）：系统在同一扩展进程新建第二个控制器，此时进程剩余内存仅 80MB，
        //    应当复用并接管已驻留的模型，不能误报「内存不足，本地模型没有加载」
        ai2.attach(engine2)
        ai2.apply(settings)
        await poll(loader2, until: .loaded)
        await drain(queue)
        XCTAssertTrue(engine2.isLocalModelLoaded)
        XCTAssertFalse(engine1.isLocalModelLoaded, "旧控制器的引用应已卸下，同一时刻只留一份")
        XCTAssertEqual(loader1.state, .idle)
        XCTAssertEqual(ai2.statusText, "AI 选词 · 本地模型")

        // 3. 回到第一个控制器继续打字时，按需无缝接回本地模型
        ai1.restoreLocalModelIfYielded()
        await poll(loader1, until: .loaded)
        await drain(queue)
        XCTAssertTrue(engine1.isLocalModelLoaded)
        XCTAssertFalse(engine2.isLocalModelLoaded)
        XCTAssertEqual(ai1.statusText, "AI 选词 · 本地模型")
    }

    func testPeerControllerModelSwitchAndOffEvictsBackgroundPeer() async throws {
        let queue = DispatchQueue(label: "test.local-model.switch")
        let coordinator = LocalModelCoordinator(queue: queue)
        let engine1 = RecordingEngine()
        let engine2 = RecordingEngine()
        let sharedMemory: () -> UInt64 = {
            (engine1.isLocalModelLoaded || engine2.isLocalModelLoaded)
                ? 80 * 1024 * 1024
                : 160 * 1024 * 1024
        }
        let a = try makeModelFile()
        let b = try makeModelFile()
        let loader1 = LocalModelLoader(coordinator: coordinator, availableMemory: sharedMemory)
        let loader2 = LocalModelLoader(coordinator: coordinator, availableMemory: sharedMemory)

        loader1.load(a, into: engine1)
        await waitForState(loader1, .loaded)
        XCTAssertEqual(engine1.loadedModel, a)

        // 第二个控制器要换另一个模型 b：先卸掉控制器 1 的 a 腾出内存，再加载 b
        loader2.load(b, into: engine2)
        await waitForState(loader2, .loaded)
        await drain(queue)
        XCTAssertFalse(engine1.isLocalModelLoaded)
        XCTAssertEqual(loader1.state, .idle)
        XCTAssertEqual(engine2.loadedModel, b)

        // 在任意控制器上收到内存警告，进程里所有控制器的本地模型都卸掉，且正在持有模型的控制器进入 .unloadedForMemory
        loader1.didReceiveMemoryWarning(engine: engine1)
        await drain(queue)
        XCTAssertFalse(engine1.isLocalModelLoaded)
        XCTAssertFalse(engine2.isLocalModelLoaded)
        XCTAssertEqual(loader1.state, .idle)
        XCTAssertEqual(loader2.state, .unloadedForMemory)
    }

    @MainActor
    private final class ColdStartChoiceModel: ChoiceModel {
        var knownAvailability: Bool?
        var resolvedAvailability = true
        var prewarmCount = 0
        private var pendingResult: Bool?
        private var continuation: CheckedContinuation<Bool, Never>?

        var isAvailable: Bool {
            XCTFail("冷启动未知可用性时不应在主线程同步读取 isAvailable")
            return resolvedAvailability
        }

        func refreshAvailability() async -> Bool {
            let value: Bool
            if let pending = pendingResult {
                pendingResult = nil
                value = pending
            } else {
                value = await withCheckedContinuation { cont in
                    continuation = cont
                }
            }
            knownAvailability = value
            return value
        }

        func resolve(_ available: Bool) {
            resolvedAvailability = available
            if let cont = continuation {
                continuation = nil
                cont.resume(returning: available)
            } else {
                pendingResult = available
            }
        }

        func prewarm() {
            prewarmCount += 1
        }

        func choose(_ request: IMEChoiceRequest) async throws -> String? {
            request.candidates.first
        }
    }

    func testColdStartAppleModeProbesAvailabilityWithoutBlockingMainThread() async {
        let model = ColdStartChoiceModel()
        let ai = AIController(chooser: AppleModelChooser(model: model))
        let engine = RecordingEngine()
        var settings = IMESettings()
        settings.aiMode = .apple

        ai.attach(engine)
        ai.apply(settings)
        // 后台探测完成前：主线程零阻塞返回，状态显示准备中，不提前触发 isAvailable
        XCTAssertEqual(ai.statusText, "AI 选词准备中…")
        XCTAssertNil(engine.onChoiceRequest)

        let ready = expectation(description: "异步可用性探测完成")
        ai.onStatusChange = { status in
            if status == "AI 选词 · Apple 端侧模型" { ready.fulfill() }
        }
        await Task.yield()
        model.resolve(true)
        await fulfillment(of: [ready], timeout: 2)
        XCTAssertNotNil(engine.onChoiceRequest)
        XCTAssertEqual(model.prewarmCount, 1)
    }

    func testPendingCompositionStoreSkipsHostXPCWhenNoSnapshotExists() {
        let missingURL = FileManager.default.temporaryDirectory
            .appendingPathComponent("nonexistent-\(UUID().uuidString).json")
        let store = PendingCompositionStore(fileURL: missingURL)
        XCTAssertFalse(store.mayHaveEntries)

        let docID = UUID()
        store.save("{\"buf\":\"ni\"}", for: docID)
        XCTAssertTrue(store.mayHaveEntries)

        store.remove(for: docID)
        try? FileManager.default.removeItem(at: missingURL)
        XCTAssertFalse(store.mayHaveEntries)
    }

    // MARK: - 后台释放、冷启动延后加载与内存余量保护

    func testBackgroundDisappearKeepsModelWarmWhenHeadroomHealthyAndEvictsWhenTight() async throws {
        let queue = DispatchQueue(label: "test.local-model.bg-evict")
        var releaseCount = 0
        var headroomBytes: UInt64 = 108 * 1024 * 1024 // 模拟 Qwen2.5 常驻后仍有 ~108 MiB 充裕余量
        let coordinator = LocalModelCoordinator(
            queue: queue,
            availableMemory: { headroomBytes },
            lowHeadroomUnloadBytes: 48 * 1024 * 1024,
            releaseCaches: { releaseCount += 1 }
        )
        let loader = LocalModelLoader(coordinator: coordinator, availableMemory: { .max })
        let engine = RecordingEngine()
        let url = try makeModelFile()
        let token = ObjectIdentifier(engine)

        coordinator.controllerWillAppear(token)
        loader.load(url, into: engine)
        await waitForState(loader, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)

        // 1. 余量充裕（≥48 MiB）时收起键盘或切 App：保留本地模型热驻留供下次亚毫秒复用，仅回收空闲堆页
        coordinator.controllerDidDisappear(token, hostResigned: true)
        await drain(queue)
        XCTAssertTrue(engine.isLocalModelLoaded, "内存余量充裕时不应盲目卸载本地模型")
        XCTAssertEqual(loader.state, .loaded)
        XCTAssertEqual(releaseCount, 1)

        // 2. 余量偏紧（<48 MiB）时退入后台：立即同步入队卸载模型，不依赖可能拖到下次唤醒才触发的延迟定时器
        coordinator.controllerWillAppear(token)
        headroomBytes = 32 * 1024 * 1024
        coordinator.controllerDidDisappear(token, hostResigned: true)
        await drain(queue)
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertEqual(loader.state, .idle, "后台主动卸载应回到 .idle 而不是 .unloadedForMemory")
        XCTAssertEqual(releaseCount, 2)

        // 3. 下次余量恢复后唤起可正常重新加载
        headroomBytes = 120 * 1024 * 1024
        coordinator.controllerWillAppear(token)
        loader.load(url, into: engine)
        await waitForState(loader, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)
    }

    func testProbeAppleAvailabilityInLocalModeDoesNotBlockMainThread() async {
        let model = ColdStartChoiceModel()
        let ai = AIController(chooser: AppleModelChooser(model: model))
        var settings = IMESettings()
        settings.aiMode = .local
        ai.apply(settings)
        XCTAssertNil(ai.chooser.knownAvailability)

        let updated = expectation(description: "后台异步探测完成")
        ai.probeAppleAvailabilityIfNeeded {
            updated.fulfill()
        }
        await Task.yield()
        model.resolve(true)
        await fulfillment(of: [updated], timeout: 2)
        XCTAssertEqual(ai.chooser.knownAvailability, true)
    }

    func testMemoryWarningFlushesCachesEvenWhenEngineNotCreated() async throws {
        let queue = DispatchQueue(label: "test.local-model.mem-nil-engine")
        var releaseCount = 0
        let coordinator = LocalModelCoordinator(
            queue: queue,
            releaseCaches: { releaseCount += 1 }
        )
        let loader1 = LocalModelLoader(coordinator: coordinator, availableMemory: { .max })
        let loader2 = LocalModelLoader(coordinator: coordinator, availableMemory: { .max })
        let engine1 = RecordingEngine()
        let url = try makeModelFile()

        loader1.load(url, into: engine1)
        await waitForState(loader1, .loaded)
        XCTAssertTrue(engine1.isLocalModelLoaded)

        // 控制器 2 尚未 attach 任何 engine 时收到内存警告，也必须清掉同进程模型并归还静态缓存
        let ai2 = AIController(chooser: AppleModelChooser(model: FakeChoiceModel()), localLoader: loader2)
        ai2.didReceiveMemoryWarning()
        await drain(queue)
        XCTAssertFalse(engine1.isLocalModelLoaded)
        XCTAssertEqual(loader1.state, .unloadedForMemory)
        XCTAssertGreaterThanOrEqual(releaseCount, 1)
    }

    func testDeferredColdLocalLoadPropagatesThroughAttachAndCancelsOnHide() async throws {
        let queue = DispatchQueue(label: "test.local-model.defer-cold")
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let url = try makeModelFile()
        let ai = AIController(
            chooser: AppleModelChooser(model: FakeChoiceModel()),
            localLoader: loader,
            localModelURL: { _ in url },
            coldLocalLoadDelay: 0.08
        )
        let engine = RecordingEngine()
        var settings = IMESettings()
        settings.aiMode = .local
        settings.localModelID = "qwen2.5-0.5b-q4km"

        // 模拟冷启动顺序：先 apply(deferColdLocalLoad: true)，再 attach(engine)
        ai.apply(settings, deferColdLocalLoad: true)
        ai.attach(engine)
        XCTAssertEqual(loader.state, .idle, "冷启动本地模型应等待键盘弹出动画完成后再加载")
        XCTAssertEqual(ai.statusText, "正在加载本地模型…")
        XCTAssertFalse(engine.isLocalModelLoaded)

        // 延迟窗口内键盘收起：取消冷加载，不抢占内存
        ai.keyboardWillHide()
        try await Task.sleep(for: .milliseconds(120))
        await drain(queue)
        XCTAssertEqual(loader.state, .idle)
        XCTAssertFalse(engine.isLocalModelLoaded)

        // 再次出现并保持可见：延后窗口结束后正常加载完成
        ai.apply(settings, deferColdLocalLoad: true)
        XCTAssertEqual(loader.state, .idle)
        await poll(loader, until: .loaded)
        XCTAssertEqual(loader.state, .loaded)
        XCTAssertTrue(engine.isLocalModelLoaded)
    }

    func testModelCatalogMinimumAvailableHeadroom() {
        for model in ModelCatalog.all {
            XCTAssertGreaterThanOrEqual(model.minimumAvailableMiB, 95)
            XCTAssertGreaterThanOrEqual(model.minimumAvailableMiB, model.residentMiB + 25)
        }
    }
}
