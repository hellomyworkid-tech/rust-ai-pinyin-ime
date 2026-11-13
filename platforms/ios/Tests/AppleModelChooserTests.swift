import XCTest

/// 选词调度：一次一个、新请求取消旧请求、超时、失败都交回 nil
@MainActor
final class AppleModelChooserTests: XCTestCase {
    private struct Delivery: Equatable {
        let id: UInt64
        let text: String?
    }

    private var model: FakeChoiceModel!
    private var chooser: AppleModelChooser!
    private var deliveries: [Delivery] = []
    private var onDeliver: (() -> Void)?

    override func setUp() async throws {
        model = FakeChoiceModel()
        makeChooser(timeout: 5)
    }

    private func makeChooser(timeout: TimeInterval) {
        chooser = AppleModelChooser(model: model, timeout: timeout)
        deliveries = []
        chooser.deliver = { [weak self] id, text in
            self?.deliveries.append(Delivery(id: id, text: text))
            self?.onDeliver?()
        }
    }

    private func request(_ id: UInt64, _ candidates: [String] = ["你好", "拟好", "你号"]) -> IMEChoiceRequest {
        IMEChoiceRequest(id: id, context: "上文", pinyin: "ni'hao", candidates: candidates)
    }

    /// 等到交回的结果达到 count 个
    private func waitForDeliveries(_ count: Int, timeout: TimeInterval = 2) async {
        if deliveries.count >= count { return }
        let done = expectation(description: "交回 \(count) 个结果")
        onDeliver = { [weak self] in
            if (self?.deliveries.count ?? 0) >= count { done.fulfill() }
        }
        await fulfillment(of: [done], timeout: timeout)
        onDeliver = nil
    }

    /// 让主线程上排着的任务都跑一轮
    private func settle() async {
        for _ in 0..<10 { await Task.yield() }
        try? await Task.sleep(for: .milliseconds(20))
    }

    func testDebounceKeepsSupersededRequestsAwayFromTheModel() async {
        chooser = AppleModelChooser(model: model, timeout: 5, debounce: 0.15)
        deliveries = []
        chooser.deliver = { [weak self] id, text in
            self?.deliveries.append(Delivery(id: id, text: text))
            self?.onDeliver?()
        }
        model.behavior = .answer("拟好")
        // 连续三次按键各来一个请求，间隔远小于等待时间
        chooser.request(request(1))
        chooser.request(request(2))
        chooser.request(request(3))
        await waitForDeliveries(3)
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: nil), Delivery(id: 2, text: nil), Delivery(id: 3, text: "拟好")])
        XCTAssertEqual(model.requests.map(\.id), [3], "被取代的请求不应到达模型")
        XCTAssertFalse(chooser.isBusy)
    }

    func testDebouncedRequestCancelledBeforeItStarts() async {
        chooser = AppleModelChooser(model: model, timeout: 5, debounce: 0.15)
        deliveries = []
        chooser.deliver = { [weak self] id, text in
            self?.deliveries.append(Delivery(id: id, text: text))
        }
        model.behavior = .answer("拟好")
        chooser.request(request(1))
        chooser.cancel()
        try? await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: nil)])
        XCTAssertTrue(model.requests.isEmpty)
    }

    func testDeliversModelAnswer() async {
        model.behavior = .answer("拟好")
        chooser.request(request(1))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: "拟好")])
        XCTAssertEqual(model.requests.first?.pinyin, "ni'hao")
        XCTAssertFalse(chooser.isBusy)
    }

    func testAnswerOutsideCandidatesBecomesNil() async {
        model.behavior = .answer("泥嚎")
        chooser.request(request(1))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: nil)])
    }

    func testModelErrorBecomesNil() async {
        model.behavior = .fail
        chooser.request(request(7))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 7, text: nil)])
    }

    func testUnavailableModelAnswersNilImmediately() {
        model.isAvailable = false
        chooser.request(request(3))
        XCTAssertEqual(deliveries, [Delivery(id: 3, text: nil)])
        XCTAssertTrue(model.requests.isEmpty)
    }

    func testSingleCandidateIsNotSentToModel() {
        chooser.request(request(4, ["你好"]))
        XCTAssertEqual(deliveries, [Delivery(id: 4, text: nil)])
        XCTAssertTrue(model.requests.isEmpty)
    }

    func testNewerRequestCancelsOlderOne() async {
        model.behavior = .wait
        chooser.request(request(1))
        await settle()
        chooser.request(request(2))
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: nil)], "旧请求立即作废")
        await settle()

        // 旧请求的模型这才答出来：不能再交一次
        model.resolve(1, "你好")
        await settle()
        XCTAssertEqual(deliveries.count, 1)

        model.resolve(2, "你号")
        await waitForDeliveries(2)
        XCTAssertEqual(deliveries, [Delivery(id: 1, text: nil), Delivery(id: 2, text: "你号")])
    }

    func testTimeoutDeliversNilOnce() async {
        makeChooser(timeout: 0.05)
        model.behavior = .hang
        chooser.request(request(9))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 9, text: nil)])
        try? await Task.sleep(for: .milliseconds(150))
        XCTAssertEqual(deliveries.count, 1, "超时后不会再交第二次")
        XCTAssertFalse(chooser.isBusy)
    }

    func testAnswerJustBeforeTimeoutWins() async {
        makeChooser(timeout: 0.3)
        model.behavior = .wait
        chooser.request(request(5))
        await settle()
        model.resolve(5, "拟好")
        await waitForDeliveries(1)
        try? await Task.sleep(for: .milliseconds(400))
        XCTAssertEqual(deliveries, [Delivery(id: 5, text: "拟好")])
    }

    func testCancelDeliversNil() async {
        model.behavior = .hang
        chooser.request(request(6))
        chooser.cancel()
        XCTAssertEqual(deliveries, [Delivery(id: 6, text: nil)])
        chooser.cancel()
        XCTAssertEqual(deliveries.count, 1, "没有在跑的请求时取消不交结果")
    }

    func testPrewarmOnlyWhenAvailable() {
        chooser.prewarm()
        XCTAssertEqual(model.prewarmCount, 1)
        model.isAvailable = false
        chooser.prewarm()
        XCTAssertEqual(model.prewarmCount, 1)
    }

    func testAppleModelPromptFormat() {
        let prompt = AppleChoiceModel.prompt(for: request(1), options: ["你好", "拟好"])
        XCTAssertEqual(prompt, "上文：上文\n拼音：ni'hao\n候选：你好 / 拟好")
    }

    func testRealAppleModelNeverCrashesWhenUnavailable() async throws {
        // 模拟器和不支持的机器上模型不可用：应当老实返回 nil
        let real = AppleChoiceModel()
        guard !real.isAvailable else { throw XCTSkip("这台机器上 Apple 模型可用，跳过") }
        real.prewarm()
        let answer = try await real.choose(request(1))
        XCTAssertNil(answer)
    }

    // MARK: - 引擎作废请求（SW-7）

    /// 引擎作废了请求：只中止编号对上的那个，不交回任何结果
    func testEngineCancelStopsOnlyThatRequestWithoutDelivering() async {
        model.behavior = .hang
        chooser.request(request(1))
        await settle()
        XCTAssertEqual(model.requests.map(\.id), [1])
        chooser.cancel(id: 2)
        XCTAssertTrue(chooser.isBusy, "别的编号不影响正在跑的请求")
        chooser.cancel(id: 1)
        XCTAssertFalse(chooser.isBusy, "作废后立即停下，不等模型答完或超时")
        await settle()
        XCTAssertEqual(deliveries, [], "引擎已经不等了，不交回")

        // 之后的请求照常
        model.behavior = .answer("你号")
        chooser.request(request(3))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 3, text: "你号")])
    }

    /// 作废通知先于同一编号的请求到达：那个请求不再交给模型
    func testCancelArrivingBeforeItsRequestSkipsTheModel() async {
        model.behavior = .answer("你号")
        chooser.cancel(id: 5)
        chooser.request(request(5))
        XCTAssertFalse(chooser.isBusy)
        await settle()
        XCTAssertTrue(model.requests.isEmpty)
        XCTAssertEqual(deliveries, [])
        chooser.request(request(6))
        await waitForDeliveries(1)
        XCTAssertEqual(deliveries, [Delivery(id: 6, text: "你号")])
    }
}
