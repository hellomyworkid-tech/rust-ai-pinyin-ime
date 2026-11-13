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
}
