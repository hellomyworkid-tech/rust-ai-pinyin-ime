import XCTest
import CryptoKit

/// 用 URLProtocol 假装服务器，测真正的 URLSession 通道：完成后文件被收进暂存目录、HTTP 错误、断网。
/// 后台会话本身（系统唤起 App）没法在单元测试里跑，这里换成普通会话配置，其余代码是同一份。
final class BackgroundDownloadTransportTests: XCTestCase {
    final class StubProtocol: URLProtocol {
        nonisolated(unsafe) static var handler: ((URLRequest) -> Result<(Int, Data), Error>)?
        override class func canInit(with request: URLRequest) -> Bool { true }
        override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }
        override func startLoading() {
            switch Self.handler?(request) ?? .failure(URLError(.unknown)) {
            case .success(let (status, body)):
                let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: "HTTP/1.1",
                                               headerFields: ["Content-Length": "\(body.count)"])!
                client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
                client?.urlProtocol(self, didLoad: body)
                client?.urlProtocolDidFinishLoading(self)
            case .failure(let error):
                client?.urlProtocol(self, didFailWithError: error)
            }
        }
        override func stopLoading() {}
    }

    private var staging: URL!
    private var transport: BackgroundDownloadTransport!
    private var events: [DownloadEvent] = []

    override func setUpWithError() throws {
        staging = FileManager.default.temporaryDirectory.appendingPathComponent("transport-\(UUID().uuidString)")
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [StubProtocol.self]
        transport = BackgroundDownloadTransport(configuration: config, stagingDirectory: staging)
        transport.onEvent = { [weak self] event in
            self?.events.append(event)
        }
    }

    override func tearDownWithError() throws {
        StubProtocol.handler = nil
        try? FileManager.default.removeItem(at: staging)
    }

    /// 转动主线程的 run loop，等到出现进度以外的事件（最多 5 秒）
    private func waitForTerminalEvent() {
        let deadline = Date().addingTimeInterval(5)
        while Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.05))
            if events.contains(where: { if case .progress = $0 { return false } else { return true } }) { break }
        }
    }

    func testFinishedFileIsMovedToStaging() throws {
        let body = Data(repeating: 7, count: 4096)
        StubProtocol.handler = { _ in .success((200, body)) }
        transport.start(id: "m1", url: URL(string: "https://example.com/m.gguf")!, resumeData: nil)
        waitForTerminalEvent()

        guard case .finished(let id, let file)? = events.last else { return XCTFail("没有收到完成事件：\(events)") }
        XCTAssertEqual(id, "m1")
        XCTAssertEqual(file.deletingLastPathComponent().standardizedFileURL, staging.standardizedFileURL)
        XCTAssertEqual(try Data(contentsOf: file), body)
    }

    func testHTTPErrorIsAFailureNotAFile() throws {
        StubProtocol.handler = { _ in .success((404, Data("not found".utf8))) }
        transport.start(id: "m1", url: URL(string: "https://example.com/m.gguf")!, resumeData: nil)
        waitForTerminalEvent()

        guard case .failed(_, let failure, _)? = events.last else { return XCTFail("应当失败：\(events)") }
        XCTAssertEqual(failure, .http(404))
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: staging.path), [], "404 的网页不能当模型文件留下")
    }

    func testOfflineIsReportedInChinese() throws {
        StubProtocol.handler = { _ in .failure(URLError(.notConnectedToInternet)) }
        transport.start(id: "m1", url: URL(string: "https://example.com/m.gguf")!, resumeData: nil)
        waitForTerminalEvent()

        guard case .failed(_, let failure, _)? = events.last else { return XCTFail("应当失败：\(events)") }
        XCTAssertEqual(failure, .network("没有网络连接"))
    }

    /// App 被后台下载唤起、管理器还没建好时，传输层把完成事件先存着（文件已经在暂存目录里）。
    /// 管理器建好时先接手暂存目录里的文件、开始核对，随后才收到积压的同一个完成事件：
    /// 这个事件不能删掉正在核对的文件，模型照样装好
    @MainActor
    func testQueuedFinishedEventDoesNotDisturbTheFileAdoptedAtLaunch() async throws {
        let body = Data(repeating: 7, count: 4096)
        StubProtocol.handler = { _ in .success((200, body)) }
        let config = URLSessionConfiguration.ephemeral
        config.protocolClasses = [StubProtocol.self]
        // 还没接上 onEvent 的通道，事件只能先存着
        let queued = BackgroundDownloadTransport(configuration: config, stagingDirectory: staging)
        queued.start(id: "m1", url: URL(string: "https://example.com/m.gguf")!, resumeData: nil)
        let download = staging.appendingPathComponent("m1.download")
        for _ in 0..<500 where !FileManager.default.fileExists(atPath: download.path) {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: download.path))
        // 让通道把完成事件存进待发队列
        try await Task.sleep(for: .milliseconds(100))

        let models = FileManager.default.temporaryDirectory.appendingPathComponent("transport-models-\(UUID().uuidString)")
        addTeardownBlock { try? FileManager.default.removeItem(at: models) }
        let sha = SHA256.hash(data: body).map { String(format: "%02x", $0) }.joined()
        let model = LocalModel(id: "m1", title: "m1", fileName: "m1.gguf",
                               downloadURL: URL(string: "https://example.com/m.gguf")!,
                               bytes: Int64(body.count), residentMiB: 1, note: "", sha256: sha)
        let manager = ModelDownloadManager(catalog: [model], transport: queued, directory: models,
                                           stagingDirectory: staging, network: FakeNetwork(), freeSpace: { .max })
        XCTAssertEqual(manager.state(of: model), .finishing, "启动时接手了暂存目录里的文件")
        for _ in 0..<500 where manager.state(of: model) == .finishing {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(manager.state(of: model), .downloaded)
        XCTAssertEqual(try Data(contentsOf: models.appendingPathComponent("m1.gguf")), body)
        XCTAssertFalse(FileManager.default.fileExists(atPath: download.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: staging.appendingPathComponent("m1.verify").path))
    }

    /// 系统取消的任务（关掉了「后台 App 刷新」、系统资源不够）要报失败，不能让状态一直停在下载中
    func testSystemCancellationIsAFailure() throws {
        StubProtocol.handler = { _ in .failure(URLError(.cancelled)) }
        transport.start(id: "m1", url: URL(string: "https://example.com/m.gguf")!, resumeData: nil)
        waitForTerminalEvent()

        guard case .failed(_, let failure, _)? = events.last else { return XCTFail("应当失败：\(events)") }
        XCTAssertEqual(failure, .network("下载被系统中止了（比如关掉了「后台 App 刷新」），请重试"))
    }

    /// 同一个 id 重新开始以后，旧任务迟到的失败不报出来（不然会把正在下的新任务标成失败）
    func testLateFailureOfAnOlderTaskIsIgnored() throws {
        let body = Data(repeating: 1, count: 2048)
        StubProtocol.handler = { request in
            if request.url?.lastPathComponent == "old.bin" {
                Thread.sleep(forTimeInterval: 0.5)
                return .failure(URLError(.networkConnectionLost))
            }
            return .success((200, body))
        }
        transport.start(id: "m1", url: URL(string: "https://example.com/old.bin")!, resumeData: nil)
        transport.start(id: "m1", url: URL(string: "https://example.com/new.bin")!, resumeData: nil)
        let deadline = Date().addingTimeInterval(3)
        while Date() < deadline { RunLoop.main.run(until: Date().addingTimeInterval(0.05)) }

        XCTAssertTrue(events.contains { if case .finished = $0 { return true } else { return false } }, "\(events)")
        XCTAssertFalse(events.contains { if case .failed = $0 { return true } else { return false } },
                       "旧任务迟到的失败不能报出来：\(events)")
    }

    /// 旧任务的失败已经排进主线程队列、还没送到，主线程上就开始了同一个 id 的新任务（App 重新打开后
    /// 系统补发旧任务的结果时会这样）：送到时再看一次，丢掉，不把正在下的新任务标成失败
    func testFailureQueuedBeforeANewStartIsDropped() throws {
        let body = Data(repeating: 2, count: 2048)
        StubProtocol.handler = { request in
            request.url?.lastPathComponent == "old.bin" ? .failure(URLError(.networkConnectionLost)) : .success((200, body))
        }
        transport.start(id: "m1", url: URL(string: "https://example.com/old.bin")!, resumeData: nil)
        // 不转主线程的 run loop：旧任务的失败在委托队列上发生，排进主线程队列后停在那里
        Thread.sleep(forTimeInterval: 0.5)
        XCTAssertTrue(events.isEmpty)
        transport.start(id: "m1", url: URL(string: "https://example.com/new.bin")!, resumeData: nil)
        waitForTerminalEvent()

        XCTAssertFalse(events.contains { if case .failed = $0 { return true } else { return false } },
                       "旧任务的失败送到时新任务已经开始了，要丢掉：\(events)")
        guard case .finished? = events.last else { return XCTFail("新任务应当下完：\(events)") }
    }

    func testReconnectAnswersWhenDone() {
        transport.reconnect()
        waitForTerminalEvent()
        guard case .reconnected? = events.last else { return XCTFail("应当答复 .reconnected：\(events)") }
    }

    func testErrorDescriptions() {
        XCTAssertEqual(BackgroundDownloadTransport.describe(NSError(domain: NSURLErrorDomain, code: NSURLErrorTimedOut)), "连接超时")
        XCTAssertTrue(BackgroundDownloadTransport.describe(NSError(domain: NSURLErrorDomain, code: -9999)).contains("-9999"))
    }
}
