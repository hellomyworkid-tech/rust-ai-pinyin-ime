import XCTest
import Compression
import CryptoKit

/// 完整词库首次启动时下载：压缩包的核对、解压、安装，键盘找词库的顺序，词库换了之后换引擎
@MainActor
final class LexiconDownloadTests: XCTestCase {
    private var root: URL!
    private var lexiconDirectory: URL!
    private var stagingDirectory: URL!
    private var transport: FakeDownloadTransport!
    private var completed: [String] = []
    /// 解压前的「词库」内容：有重复、能压缩，又不全是零
    private let payload: Data = {
        var bytes = [UInt8]()
        bytes.reserveCapacity(300_000)
        var x: UInt32 = 12345
        for i in 0..<300_000 {
            x = x &* 1_103_515_245 &+ 12345
            bytes.append(i % 7 == 0 ? UInt8(truncatingIfNeeded: x >> 16) : UInt8(i % 31))
        }
        return Data(bytes)
    }()

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("lexicon-\(UUID().uuidString)")
        lexiconDirectory = root.appendingPathComponent("Shared/Lexicon")
        stagingDirectory = root.appendingPathComponent("Staging")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        transport = FakeDownloadTransport()
        completed = []
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    // MARK: - 工具

    private static func hex(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    /// 用系统的 LZMA（xz）编码器压缩，和 scripts/package-lexicon.sh 用的 compression_tool 一样
    private static func xz(_ data: Data) throws -> Data {
        var out = Data()
        let filter = try OutputFilter(.compress, using: .lzma) { chunk in
            if let chunk { out.append(chunk) }
        }
        try filter.write(data)
        try filter.finalize()
        return out
    }

    private func release(archive: Data, sha256: String?, installedBytes: Int64? = nil,
                         installedSHA256: String?) -> LexiconRelease {
        LexiconRelease(version: "2099.1.1", downloadURL: URL(string: "https://example.com/lexicon.bin.xz")!,
                       bytes: Int64(archive.count), sha256: sha256,
                       installedBytes: installedBytes ?? Int64(payload.count), installedSHA256: installedSHA256,
                       sources: "测试")
    }

    private func makeManager(_ release: LexiconRelease) -> LexiconDownloadManager {
        let manager = LexiconDownloadManager(
            catalog: [release], transport: transport, directory: lexiconDirectory,
            stagingDirectory: stagingDirectory, network: FakeNetwork(), freeSpace: { 10_000_000_000 })
        manager.onCompleted = { [unowned self] in self.completed.append($0.id) }
        return manager
    }

    /// 传输层把下好的压缩包放进暂存目录
    private func staged(_ archive: Data, id: String) throws -> URL {
        try FileManager.default.createDirectory(at: stagingDirectory, withIntermediateDirectories: true)
        let url = stagingDirectory.appendingPathComponent("\(id).download")
        try archive.write(to: url)
        return url
    }

    private func waitUntil(_ condition: @escaping () -> Bool, timeout: TimeInterval = 5) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition(), Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    private func names(in directory: URL) -> [String] {
        ((try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []).sorted()
    }

    // MARK: - 下载、核对、解压

    func testArchiveIsCheckedExpandedAndInstalled() async throws {
        let archive = try Self.xz(payload)
        XCTAssertLessThan(archive.count, payload.count, "测试数据应当能压缩")
        let item = release(archive: archive, sha256: Self.hex(archive), installedSHA256: Self.hex(payload))
        let manager = makeManager(item)
        manager.start(item)
        XCTAssertEqual(transport.starts.map(\.url), [item.downloadURL])
        transport.emit(.finished(id: item.id, file: try staged(archive, id: item.id)))
        XCTAssertEqual(manager.state(of: item), .finishing, "核对和解压在后台")
        await waitUntil { manager.state(of: item) != .finishing }

        XCTAssertEqual(manager.state(of: item), .downloaded)
        XCTAssertEqual(try Data(contentsOf: lexiconDirectory.appendingPathComponent(item.fileName)), payload)
        XCTAssertEqual(names(in: lexiconDirectory), [item.fileName], "不留 .part")
        XCTAssertEqual(names(in: stagingDirectory), [], "压缩包装好后删掉")
        XCTAssertEqual(completed, [item.id])

        // 重新打开 App：按解压后的大小认出已经装好
        let reopened = makeManager(item)
        XCTAssertEqual(reopened.state(of: item), .downloaded)
    }

    func testArchiveWithWrongChecksumIsNotExpanded() async throws {
        let archive = try Self.xz(payload)
        let item = release(archive: archive, sha256: String(repeating: "0", count: 64),
                           installedSHA256: Self.hex(payload))
        let manager = makeManager(item)
        manager.start(item)
        transport.emit(.finished(id: item.id, file: try staged(archive, id: item.id)))
        await waitUntil { manager.state(of: item) != .finishing }
        XCTAssertEqual(manager.state(of: item), .failed(.checksumMismatch))
        XCTAssertEqual(names(in: lexiconDirectory), [])
        XCTAssertEqual(completed, [])
    }

    func testCorruptArchiveFailsWithoutLeavingFiles() async throws {
        var archive = try Self.xz(payload)
        // 压缩包中间改坏几个字节；不核对压缩包的哈希，看解压这一步拦不拦得住
        for i in stride(from: archive.count / 3, to: archive.count / 3 + 64, by: 1) { archive[i] ^= 0x5A }
        let item = release(archive: archive, sha256: nil, installedSHA256: Self.hex(payload))
        let manager = makeManager(item)
        manager.start(item)
        transport.emit(.finished(id: item.id, file: try staged(archive, id: item.id)))
        await waitUntil { manager.state(of: item) != .finishing }
        guard case .failed = manager.state(of: item) else {
            return XCTFail("坏的压缩包应当失败，实际 \(manager.state(of: item))")
        }
        XCTAssertEqual(names(in: lexiconDirectory), [], "不留半成品")
        XCTAssertEqual(completed, [])
    }

    func testExpandedFileMustMatchItsChecksumAndSize() async throws {
        let archive = try Self.xz(payload)
        let wrongHash = release(archive: archive, sha256: Self.hex(archive),
                                installedSHA256: String(repeating: "1", count: 64))
        let manager = makeManager(wrongHash)
        manager.start(wrongHash)
        transport.emit(.finished(id: wrongHash.id, file: try staged(archive, id: wrongHash.id)))
        await waitUntil { manager.state(of: wrongHash) != .finishing }
        XCTAssertEqual(manager.state(of: wrongHash), .failed(.checksumMismatch))
        XCTAssertEqual(names(in: lexiconDirectory), [])

        let wrongSize = release(archive: archive, sha256: Self.hex(archive), installedBytes: 42,
                                installedSHA256: Self.hex(payload))
        let other = makeManager(wrongSize)
        other.start(wrongSize)
        transport.emit(.finished(id: wrongSize.id, file: try staged(archive, id: wrongSize.id)))
        await waitUntil { other.state(of: wrongSize) != .finishing }
        XCTAssertEqual(other.state(of: wrongSize), .failed(.sizeMismatch(expected: 42, actual: Int64(payload.count))))
        XCTAssertEqual(names(in: lexiconDirectory), [])
    }

    func testCancelWhileExpandingDiscardsTheResult() async throws {
        let archive = try Self.xz(payload)
        let item = release(archive: archive, sha256: nil, installedSHA256: Self.hex(payload))
        let manager = makeManager(item)
        manager.start(item)
        transport.emit(.finished(id: item.id, file: try staged(archive, id: item.id)))
        XCTAssertEqual(manager.state(of: item), .finishing)
        manager.cancel(item)
        XCTAssertEqual(manager.state(of: item), .notDownloaded)
        // 后台的解压做完以后自己收拾：不装、不留 .part
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(manager.state(of: item), .notDownloaded)
        XCTAssertEqual(names(in: lexiconDirectory), [])
        XCTAssertEqual(completed, [])
    }

    func testSpaceCheckCountsTheExpandedFile() {
        let item = release(archive: Data(count: 1000), sha256: nil, installedBytes: 50_000_000, installedSHA256: nil)
        let manager = LexiconDownloadManager(
            catalog: [item], transport: transport, directory: lexiconDirectory, stagingDirectory: stagingDirectory,
            network: FakeNetwork(), freeSpace: { StorageCheck.safetyMargin + 10_000_000 })
        XCTAssertEqual(manager.preflight(item), .insufficientSpace(
            needed: StorageCheck.requiredBytes(remaining: 1000 + 50_000_000),
            available: StorageCheck.safetyMargin + 10_000_000))
    }

    /// 目录里写的就是打包脚本打印的数：压缩包约 12 MB，解压后 58 MB，地址指向 GitHub Release
    func testCatalogDescribesTheCompressedRelease() {
        let current = LexiconCatalog.current
        XCTAssertEqual(current.sha256?.count, 64)
        XCTAssertEqual(current.installedSHA256?.count, 64)
        XCTAssertLessThan(current.bytes, current.installedBytes)
        XCTAssertTrue(current.isXZCompressed)
        XCTAssertEqual(current.downloadURL.scheme, "https")
        XCTAssertTrue(current.downloadURL.path.hasSuffix("/\(current.fileName).xz"), current.downloadURL.path)
        XCTAssertTrue(current.downloadURL.path.contains("/lexicon-\(current.version)/"), current.downloadURL.path)
    }

    /// 打包脚本生成过发布包时（build/lexicon-release/<版本>/manifest.json），目录里的数要和它一致
    func testCatalogMatchesThePackagedManifestWhenPresent() throws {
        let current = LexiconCatalog.current
        let manifest = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("build/lexicon-release/\(current.version)/manifest.json")
        guard let data = try? Data(contentsOf: manifest) else {
            throw XCTSkip("没有 \(manifest.path)，先运行 scripts/package-lexicon.sh \(current.version)")
        }
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(json["bytes"] as? Int64, current.bytes)
        XCTAssertEqual(json["sha256"] as? String, current.sha256)
        XCTAssertEqual(json["installedBytes"] as? Int64, current.installedBytes)
        XCTAssertEqual(json["installedSHA256"] as? String, current.installedSHA256)
        XCTAssertEqual(json["installedFile"] as? String, current.fileName)
    }

    // MARK: - 键盘找词库

    /// 按目录里写的大小建一个（稀疏的）词库文件
    private func placeLexicon(_ name: String, bytes: Int64) throws {
        try FileManager.default.createDirectory(at: lexiconDirectory, withIntermediateDirectories: true)
        let url = lexiconDirectory.appendingPathComponent(name)
        FileManager.default.createFile(atPath: url.path, contents: nil)
        let handle = try FileHandle(forWritingTo: url)
        try handle.truncate(atOffset: UInt64(bytes))
        try handle.close()
    }

    func testInstalledLexiconPrefersTheCurrentVersion() throws {
        let current = LexiconCatalog.current
        XCTAssertNil(LexiconCatalog.installedLexiconURL(in: lexiconDirectory), "目录还不存在")
        try placeLexicon("merged_lexicon-2026.1.1.bin", bytes: 10)
        try placeLexicon("merged_lexicon-2026.10.2.bin", bytes: 10)
        try placeLexicon("merged_lexicon-2026.10.2.bin.ABC.part", bytes: 10)
        // 当前版本还没装：用装好的版本里最新的（按数字比较，10 月比 1 月新）
        XCTAssertEqual(LexiconCatalog.installedLexiconURL(in: lexiconDirectory)?.lastPathComponent,
                       "merged_lexicon-2026.10.2.bin")
        // 当前版本的文件大小不对（没装完整）也不用
        try placeLexicon(current.fileName, bytes: 5)
        XCTAssertEqual(LexiconCatalog.installedLexiconURL(in: lexiconDirectory)?.lastPathComponent,
                       "merged_lexicon-2026.10.2.bin")
        try placeLexicon(current.fileName, bytes: current.installedBytes)
        XCTAssertEqual(LexiconCatalog.installedLexiconURL(in: lexiconDirectory)?.lastPathComponent, current.fileName)

        // 同一天的第二版比第一版新（逐段按数字比，不按字符串比）
        try placeLexicon("merged_lexicon-2026.10.2.2.bin", bytes: 10)
        try FileManager.default.removeItem(at: lexiconDirectory.appendingPathComponent(current.fileName))
        XCTAssertEqual(LexiconCatalog.installedLexiconURL(in: lexiconDirectory)?.lastPathComponent,
                       "merged_lexicon-2026.10.2.2.bin")
        XCTAssertEqual(LexiconCatalog.version(ofFile: "merged_lexicon-2026.11.12.2.bin"), [2026, 11, 12, 2])
        XCTAssertNil(LexiconCatalog.version(ofFile: "merged_lexicon-2026.11.12.bin.ABC.part"))
        XCTAssertNil(LexiconCatalog.version(ofFile: "merged_lexicon-.bin"))
        try placeLexicon(current.fileName, bytes: current.installedBytes)

        LexiconCatalog.removeOtherVersions(in: lexiconDirectory)
        XCTAssertEqual(names(in: lexiconDirectory), [current.fileName, "merged_lexicon-2026.10.2.bin.ABC.part"].sorted(),
                       "别的版本删掉；半成品留给下载管理器启动时清理")
    }

    func testEngineFactoryPrefersTheDownloadedLexicon() throws {
        // 测试包里没有词库资源：目录里没有时就没有可用的词库
        let bundle = Bundle(for: Self.self)
        XCTAssertNil(EngineFactory.lexiconSource(lexiconDirectory: lexiconDirectory, bundle: bundle))
        try placeLexicon(LexiconCatalog.current.fileName, bytes: LexiconCatalog.current.installedBytes)
        let source = EngineFactory.lexiconSource(lexiconDirectory: lexiconDirectory, bundle: bundle)
        XCTAssertEqual(source, .downloaded(lexiconDirectory.appendingPathComponent(LexiconCatalog.current.fileName)))
        XCTAssertEqual(source?.isComplete, true)
        XCTAssertEqual(LexiconSource.fallback(URL(fileURLWithPath: "/x")).isComplete, false)
    }

    /// 找词库的顺序：下载的完整词库 → 包里的完整词库（只有调试构建有）→ 包里的基础词库
    func testLexiconSourceOrder() throws {
        func kind(_ source: LexiconSource?) -> String {
            switch source {
            case .downloaded(let url): return "downloaded:" + url.lastPathComponent
            case .bundledFull(let url): return "bundledFull:" + url.lastPathComponent
            case .fallback(let url): return "fallback:" + url.lastPathComponent
            case nil: return "none"
            }
        }
        // 只有基础词库的「包」（Release 构建的键盘）
        let releaseBundle = root.appendingPathComponent("ReleaseBundle")
        try FileManager.default.createDirectory(at: releaseBundle, withIntermediateDirectories: true)
        try Data([1]).write(to: releaseBundle.appendingPathComponent("fallback_lexicon.bin"))
        // 两个都有的「包」（调试构建的键盘）
        let debugBundle = root.appendingPathComponent("DebugBundle")
        try FileManager.default.createDirectory(at: debugBundle, withIntermediateDirectories: true)
        try Data([1]).write(to: debugBundle.appendingPathComponent("fallback_lexicon.bin"))
        try Data([1]).write(to: debugBundle.appendingPathComponent("merged_lexicon.bin"))
        let release = try XCTUnwrap(Bundle(path: releaseBundle.path))
        let debug = try XCTUnwrap(Bundle(path: debugBundle.path))

        XCTAssertEqual(kind(EngineFactory.lexiconSource(lexiconDirectory: lexiconDirectory, bundle: release)),
                       "fallback:fallback_lexicon.bin")
        XCTAssertEqual(kind(EngineFactory.lexiconSource(lexiconDirectory: lexiconDirectory, bundle: debug)),
                       "bundledFull:merged_lexicon.bin")
        try placeLexicon(LexiconCatalog.current.fileName, bytes: LexiconCatalog.current.installedBytes)
        for bundle in [release, debug] {
            XCTAssertEqual(kind(EngineFactory.lexiconSource(lexiconDirectory: lexiconDirectory, bundle: bundle)),
                           "downloaded:" + LexiconCatalog.current.fileName)
        }
    }

    /// 基础词库（scripts/build-fallback-lexicon.sh 生成的 Resources/fallback_lexicon.bin）常用词照样打得出
    func testFallbackLexiconTypesCommonWords() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Resources/fallback_lexicon.bin")
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw XCTSkip("没有 \(url.path)，先运行 scripts/build-fallback-lexicon.sh")
        }
        let engine = try RustEngine(lexiconURL: url, userDictionaryURL: nil)
        for (pinyin, word) in [("nihao", "你好"), ("women", "我们"), ("zhongguo", "中国"), ("xiexie", "谢谢"),
                               ("jintiantianqihenhao", "今天天气很好")] {
            var view = IMEView.idle
            for letter in pinyin {
                if case .update(let next) = engine.handle(.character(letter)) { view = next }
            }
            XCTAssertEqual(view.visibleCandidates.first?.text, word, pinyin)
            engine.reset()
        }
    }

    // MARK: - 换引擎

    func testEngineIsReplacedWhenTheLexiconChanges() {
        let fallback = LexiconSource.fallback(URL(fileURLWithPath: "/fallback.bin"))
        let downloaded = LexiconSource.downloaded(URL(fileURLWithPath: "/downloaded.bin"))
        var lexicon: LexiconSource? = fallback
        var made: [LexiconSource?] = []
        var engines: [RecordingEngine] = []
        var dropped = 0
        let controller = InputController(makeEngine: { source in
            made.append(source)
            let engine = RecordingEngine()
            engines.append(engine)
            return engine
        }, currentLexicon: { lexicon })
        controller.onEngineDropped = { dropped += 1 }

        controller.ensureEngine()
        controller.keyboardWillAppear()
        XCTAssertEqual(dropped, 0, "词库没变：不换引擎，只检查个人词库")
        XCTAssertEqual(engines[0].count(.reloadUserDictionary), 1)

        // App 下完了完整词库；键盘下次出现时换引擎，正在输入的先放弃
        controller.typeLetter("n")
        XCTAssertTrue(controller.isComposing)
        lexicon = downloaded
        controller.keyboardWillAppear()
        XCTAssertEqual(dropped, 1)
        XCTAssertNil(controller.engine)
        XCTAssertNil(controller.engineLexicon)
        XCTAssertEqual(engines[0].count(.reset), 1)
        XCTAssertEqual(engines[0].count(.reloadUserDictionary), 1, "要丢掉的引擎不用再读个人词库")

        controller.ensureEngine()
        XCTAssertEqual(made, [fallback, downloaded])
        XCTAssertEqual(controller.engineLexicon, downloaded)
        XCTAssertTrue(controller.engine === engines[1])
        controller.keyboardWillAppear()
        XCTAssertEqual(dropped, 1)
        XCTAssertEqual(engines[1].count(.reloadUserDictionary), 1)
    }

    /// 还没有中文词库时（只能打英文）点中 / 英键，不能记成「喜欢英文」：词库下完、换了引擎之后要从中文开始
    func testTogglingWithoutALexiconKeepsTheChinesePreference() {
        var lexicon: LexiconSource? = nil
        let controller = InputController(makeEngine: { source in
            source == nil ? PassthroughEngine() as IMEEngine : RecordingEngine()
        }, currentLexicon: { lexicon })
        controller.ensureEngine()
        XCTAssertFalse(controller.isChineseMode, "没有词库只能打英文")
        controller.toggleLanguage()
        XCTAssertFalse(controller.isChineseMode)

        lexicon = .downloaded(URL(fileURLWithPath: "/lexicon.bin"))
        controller.keyboardWillAppear()
        controller.ensureEngine()
        XCTAssertTrue(controller.isChineseMode, "词库到了以后照样从中文开始")
    }

    func testAfterReconnectWaitsForTheTransport() throws {
        let item = release(archive: Data(count: 100), sha256: nil, installedSHA256: nil)
        let manager = makeManager(item)
        var ran = 0
        manager.afterReconnect { ran += 1 }
        XCTAssertEqual(ran, 0, "还没查完后台任务")
        // 上次留在后台的任务先报到，查完的答复排在后面
        transport.emit(.active(id: item.id, received: 10, total: item.bytes))
        transport.emit(.reconnected)
        XCTAssertEqual(ran, 1)
        XCTAssertEqual(manager.state(of: item), .downloading(received: 10, total: item.bytes))
        manager.afterReconnect { ran += 1 }
        XCTAssertEqual(ran, 2, "查完以后马上做")
    }

    /// App 升级换了词库版本：旧版本留在暂存目录里的续传数据、下完的文件没人认领，启动时删掉
    func testUnknownStagedFilesAreRemovedAtLaunch() throws {
        let item = release(archive: Data(count: 100), sha256: nil, installedSHA256: nil)
        try FileManager.default.createDirectory(at: stagingDirectory, withIntermediateDirectories: true)
        for name in ["lexicon-2026.1.1.resume", "lexicon-2026.1.1.download", "lexicon-2026.1.1.verify", "stray.tmp",
                     "\(item.id).resume"] {
            try Data([1]).write(to: stagingDirectory.appendingPathComponent(name))
        }
        _ = makeManager(item)
        XCTAssertEqual(names(in: stagingDirectory), ["\(item.id).resume"])
    }

    func testLocalModelIsReloadedIntoTheNewEngine() async throws {
        let queue = DispatchQueue(label: "test.local-model")
        let loader = LocalModelLoader(queue: queue, availableMemory: { .max })
        let url = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("model-\(UUID()).gguf")
        try Data([0]).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }

        let old = MockEngine()
        loader.load(url, into: old)
        await waitUntil { loader.state == .loaded }
        XCTAssertTrue(old.isLocalModelLoaded)

        loader.engineReplaced(old: old)
        XCTAssertEqual(loader.state, .idle)
        // 旧引擎上的模型在加载队列上先卸掉，再轮到新引擎加载
        await waitUntil { !old.isLocalModelLoaded }
        XCTAssertFalse(old.isLocalModelLoaded)
        let new = MockEngine()
        loader.load(url, into: new)
        XCTAssertEqual(loader.state, .loading, "同一个模型也要往新引擎里重新加载")
        await waitUntil { loader.state == .loaded }
        XCTAssertTrue(new.isLocalModelLoaded)
    }
}
