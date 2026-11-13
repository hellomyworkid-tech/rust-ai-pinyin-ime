import XCTest

/// 真引擎上的 AI 选词：宿主选词（Apple 端侧模型那条路，这里用假模型代替）和本地模型。
/// 词库从源码目录读，见 RustEngineTests。
@MainActor
final class RustEngineAITests: XCTestCase {
    private var engine: RustEngine!

    /// 规则重排在这句上文下选不对「誓言」，要靠模型
    private let context = "我对天发誓这是我的"

    override func setUp() async throws {
        engine = try RustEngineTests.makeEngine()
    }

    override func tearDown() async throws {
        engine = nil
    }

    @discardableResult
    private func type(_ letters: String) -> IMEView {
        var last = IMEView.idle
        for letter in letters {
            if case .update(let view) = engine.handle(.character(letter)) { last = view }
        }
        return last
    }

    // MARK: - 宿主选词

    func testChooserRoundTripPromotesTheDeliveredCandidate() async {
        var requests: [IMEChoiceRequest] = []
        let asked = expectation(description: "引擎为整串输入发出选词请求")
        asked.assertForOverFulfill = false
        engine.onChoiceRequest = { request in
            XCTAssertTrue(Thread.isMainThread, "请求应当在主线程交给界面")
            requests.append(request)
            if request.pinyin == "shiyan" { asked.fulfill() }
        }
        engine.setContext(context)
        XCTAssertEqual(type("shiyan").visibleCandidates.first?.text, "实验")
        await fulfillment(of: [asked], timeout: 5)

        let request = requests.last { $0.pinyin == "shiyan" }!
        XCTAssertEqual(request.context, context)
        XCTAssertEqual(request.candidates.first, "实验", "第 0 个候选是当前首选")
        XCTAssertTrue(request.candidates.contains("誓言"))

        // 不在候选里的答案被丢弃，不通知
        var updates = 0
        engine.onAsyncUpdate = { updates += 1 }
        engine.deliverChoice(id: request.id &+ 1000, text: "誓言")
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertEqual(updates, 0)

        let updated = expectation(description: "采纳后通知界面刷新")
        engine.onAsyncUpdate = {
            XCTAssertTrue(Thread.isMainThread)
            updated.fulfill()
        }
        engine.deliverChoice(id: request.id, text: "誓言")
        await fulfillment(of: [updated], timeout: 5)
        let view = engine.refreshAfterAI()
        XCTAssertEqual(view.visibleCandidates.first?.text, "誓言")
        XCTAssertEqual(view.visibleCandidates.first?.aiBoosted, true)
        XCTAssertEqual(view.composedFullSentence, "誓言")
        XCTAssertTrue(view.visibleCandidates.contains { $0.text == "实验" }, "被换下来的原首选还在")
    }

    func testRemovingTheCallbackStopsRequests() async {
        var count = 0
        engine.onChoiceRequest = { _ in count += 1 }
        engine.setContext(context)
        type("shiyan")
        try? await Task.sleep(for: .milliseconds(200))
        XCTAssertGreaterThan(count, 0)

        engine.reset()
        engine.onChoiceRequest = nil
        try? await Task.sleep(for: .milliseconds(100))
        count = 0
        engine.setContext(context)
        type("shiyan")
        try? await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(count, 0, "卸载之后引擎不应再发请求")
    }

    /// SW-7：输入结束时引擎作废等待中的请求，作废通知带着编号经 onChoiceCancel 回到主线程
    func testCancelOfAPendingRequestReachesOnChoiceCancel() async {
        var lastRequest: IMEChoiceRequest?
        let asked = expectation(description: "引擎为整串输入发出选词请求")
        asked.assertForOverFulfill = false
        engine.onChoiceRequest = { request in
            lastRequest = request
            if request.pinyin == "shiyan" { asked.fulfill() }
        }
        var cancelled: [UInt64] = []
        let voided = expectation(description: "上屏后这个请求被作废")
        voided.assertForOverFulfill = false
        engine.onChoiceCancel = { id in
            XCTAssertTrue(Thread.isMainThread, "作废通知应当在主线程交给界面")
            cancelled.append(id)
            if let last = lastRequest, last.pinyin == "shiyan", last.id == id { voided.fulfill() }
        }
        engine.setContext(context)
        type("shiyan")
        await fulfillment(of: [asked], timeout: 5)
        guard case .commit = engine.handle(.space) else { return XCTFail("空格应当上屏") }
        await fulfillment(of: [voided], timeout: 5)
        XCTAssertEqual(cancelled.last, lastRequest?.id)
    }

    /// SW-7 全程：上屏之后引擎作废请求，AIController 让选词器立即停下，不用等模型答完或 1.5 秒超时
    func testCommitStopsTheRunningAppleRequestRightAway() async throws {
        let model = FakeChoiceModel()
        model.behavior = .hang
        let chooser = AppleModelChooser(model: model, debounce: 0)
        let ai = AIController(chooser: chooser, localLoader: LocalModelLoader(availableMemory: { 0 }))
        var settings = IMESettings()
        settings.aiMode = .apple
        ai.apply(settings)
        ai.attach(engine)

        engine.setContext(context)
        type("shiyan")
        for _ in 0..<250 where !(model.requests.last?.pinyin == "shiyan" && chooser.isBusy) {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(model.requests.last?.pinyin, "shiyan")
        XCTAssertTrue(chooser.isBusy)

        guard case .commit = engine.handle(.space) else { return XCTFail("空格应当上屏") }
        let stoppedAt = CFAbsoluteTimeGetCurrent()
        for _ in 0..<100 where chooser.isBusy {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(chooser.isBusy)
        XCTAssertLessThan(CFAbsoluteTimeGetCurrent() - stoppedAt, 1.0, "应当远早于 1.5 秒超时")
    }

    /// SW-2：往后加载了候选时到达的 AI 结果先压着，用户正在看的列表不变；下一次视图变化后结果照样用上
    func testAIResultWhileBrowsingMoreCandidatesIsHeldThenApplied() async throws {
        let output = RecordingTextOutput()
        output.hostText = context
        let engine = self.engine!
        let controller = InputController(makeEngine: { engine })
        controller.output = output
        controller.ensureEngine()
        let notified = expectation(description: "AI 结果到达")
        notified.assertForOverFulfill = false
        let forward = engine.onAsyncUpdate
        engine.onAsyncUpdate = {
            forward?()
            notified.fulfill()
        }
        var requests: [IMEChoiceRequest] = []
        let asked = expectation(description: "引擎为整串输入发出选词请求")
        asked.assertForOverFulfill = false
        engine.onChoiceRequest = { request in
            requests.append(request)
            if request.pinyin == "shiyan" { asked.fulfill() }
        }

        "shiyan".forEach { controller.typeLetter($0) }
        await fulfillment(of: [asked], timeout: 5)
        XCTAssertEqual(controller.pager.candidates.first?.text, "实验")
        XCTAssertTrue(controller.loadMoreCandidates(), "shiyan 的候选不止一页")
        let browsed = controller.pager.candidates

        let request = try XCTUnwrap(requests.last { $0.pinyin == "shiyan" })
        engine.deliverChoice(id: request.id, text: "誓言")
        await fulfillment(of: [notified], timeout: 5)
        XCTAssertEqual(controller.pager.candidates, browsed, "浏览中的列表不能被抽走")

        // 下一次视图变化（点音节再点回整句）之后，模型的答案排在首位
        controller.tapSyllable(at: 0)
        controller.tapSyllable(at: 0)
        XCTAssertNil(controller.view.activeSyllableIndex)
        XCTAssertEqual(controller.pager.candidates.first?.text, "誓言")
        XCTAssertEqual(controller.pager.candidates.first?.aiBoosted, true)
        controller.space()
        XCTAssertEqual(output.hostText, context + "誓言", "空格上屏的就是看到的首选")
    }

    /// Apple 端侧模型那条路的全程，只把模型换成假的：
    /// 按键 → InputController → 引擎发请求 → AIController / AppleModelChooser → 模型作答 → 引擎重排 → 界面刷新
    func testApplePathEndToEndWithAFakeModel() async {
        let model = FakeChoiceModel()
        model.behavior = .answer("誓言")
        let ai = AIController(chooser: AppleModelChooser(model: model, debounce: 0.05),
                              localLoader: LocalModelLoader(availableMemory: { 0 }))
        let output = RecordingTextOutput()
        output.hostText = context
        let engine = self.engine!
        let controller = InputController(makeEngine: { engine })
        controller.output = output
        controller.onEngineCreated = { ai.attach($0) }
        var settings = IMESettings()
        settings.aiMode = .apple
        controller.apply(settings)
        ai.apply(settings)
        controller.ensureEngine()
        XCTAssertEqual(model.prewarmCount, 1)

        let boosted = expectation(description: "模型的答案排到首位")
        boosted.assertForOverFulfill = false
        controller.onChange = {
            if controller.pager.candidates.first?.text == "誓言" { boosted.fulfill() }
        }
        "shiyan".forEach { controller.typeLetter($0) }
        XCTAssertEqual(controller.pager.candidates.first?.text, "实验", "模型作答之前是规则重排的结果")
        await fulfillment(of: [boosted], timeout: 5)

        XCTAssertEqual(controller.pager.candidates.first?.aiBoosted, true)
        XCTAssertEqual(model.requests.last?.context, context)
        XCTAssertEqual(model.requests.last?.pinyin, "shiyan")
        // 连续打字时，被取代的前缀请求大多等不到交给模型
        XCTAssertLessThan(model.requests.count, 6)

        // 关掉 AI：不再有请求，候选回到规则重排。
        // （这里先不上屏：上屏会让引擎记住这次选择，之后同一拼音的首选就由个人记忆决定了）
        controller.resetComposition()
        controller.onChange = nil
        settings.aiMode = .off
        ai.apply(settings)
        let before = model.requests.count
        "shiyan".forEach { controller.typeLetter($0) }
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(model.requests.count, before)
        XCTAssertEqual(controller.pager.candidates.first?.text, "实验")
        controller.resetComposition()

        // 再打开：模型的答案回来后按空格上屏的就是它
        settings.aiMode = .apple
        ai.apply(settings)
        let again = expectation(description: "重新打开后模型的答案再次排到首位")
        again.assertForOverFulfill = false
        controller.onChange = {
            if controller.pager.candidates.first?.text == "誓言" { again.fulfill() }
        }
        "shiyan".forEach { controller.typeLetter($0) }
        await fulfillment(of: [again], timeout: 5)
        controller.space()
        XCTAssertEqual(output.hostText, context + "誓言")
    }

    // MARK: - 本地模型

    /// 需要一个 GGUF 文件：xcodebuild 用 TEST_RUNNER_IME_LLM_TEST_MODEL=<路径> 传入，没有就跳过
    private func modelURL() throws -> URL {
        guard let path = ProcessInfo.processInfo.environment["IME_LLM_TEST_MODEL"], !path.isEmpty else {
            throw XCTSkip("没有设置 IME_LLM_TEST_MODEL")
        }
        guard RustEngine.supportsLocalModel else {
            throw XCTSkip("这个构建的 Rust 库不含本地模型（scripts/build-rust.sh --no-llama）")
        }
        return URL(fileURLWithPath: path)
    }

    func testMissingModelFileThrows() {
        XCTAssertFalse(engine.isLocalModelLoaded)
        XCTAssertThrowsError(try engine.loadLocalModel(at: URL(fileURLWithPath: "/no/such/model.gguf")))
        XCTAssertFalse(engine.isLocalModelLoaded)
        engine.unloadLocalModel()
    }

    func testLocalModelLoadsThroughTheLoaderAndPicksByContext() async throws {
        let url = try modelURL()
        let loader = LocalModelLoader(availableMemory: { .max })
        let loadedState = expectation(description: "模型加载完成")
        loader.onStateChange = { state in
            if state == .loaded { loadedState.fulfill() }
        }
        let before = RustEngine.memoryFootprint
        let started = CFAbsoluteTimeGetCurrent()
        loader.load(url, into: engine)
        XCTAssertEqual(loader.state, .loading)
        // 加载在后台队列，这期间引擎照常响应按键
        XCTAssertEqual(type("nihao").visibleCandidates.first?.text, "你好")
        engine.reset()
        await fulfillment(of: [loadedState], timeout: 30)
        let loadSeconds = CFAbsoluteTimeGetCurrent() - started
        XCTAssertTrue(engine.isLocalModelLoaded)

        let updated = expectation(description: "模型打完分后通知刷新")
        updated.assertForOverFulfill = false
        engine.onAsyncUpdate = { updated.fulfill() }
        engine.setContext(context)
        XCTAssertEqual(type("shiyan").visibleCandidates.first?.text, "实验")
        let asked = CFAbsoluteTimeGetCurrent()
        await fulfillment(of: [updated], timeout: 20)
        let waitSeconds = CFAbsoluteTimeGetCurrent() - asked
        let view = engine.refreshAfterAI()
        XCTAssertEqual(view.visibleCandidates.first?.text, "誓言")
        XCTAssertEqual(view.visibleCandidates.first?.aiBoosted, true)

        let megabyte = 1024.0 * 1024.0
        print(String(format: "local model: load %.0f ms, wait after last key %.0f ms, footprint %.1f -> %.1f MiB",
                     loadSeconds * 1000, waitSeconds * 1000,
                     Double(before) / megabyte, Double(RustEngine.memoryFootprint) / megabyte))

        // 不上屏（上屏会形成个人记忆，下面要看的是卸载后规则重排的结果）
        engine.reset()

        // 内存警告：卸载，回到规则重排
        loader.didReceiveMemoryWarning(engine: engine)
        XCTAssertEqual(loader.state, .unloadedForMemory)
        let unloaded = expectation(description: "模型已卸载")
        Task {
            while engine.isLocalModelLoaded { try? await Task.sleep(for: .milliseconds(20)) }
            unloaded.fulfill()
        }
        await fulfillment(of: [unloaded], timeout: 10)
        engine.setContext(context)
        XCTAssertEqual(type("shiyan").visibleCandidates.first?.text, "实验")
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(engine.refreshAfterAI().visibleCandidates.first?.text, "实验", "卸载之后只剩规则重排")
    }
}
