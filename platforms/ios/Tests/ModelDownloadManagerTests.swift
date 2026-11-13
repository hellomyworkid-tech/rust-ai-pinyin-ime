import XCTest
import CryptoKit

@MainActor
final class ModelDownloadManagerTests: XCTestCase {
    private var root: URL!
    private var modelsDirectory: URL!
    private var stagingDirectory: URL!
    private var transport: FakeDownloadTransport!
    private var network: FakeNetwork!
    private var freeSpace: Int64 = 10_000_000_000
    private var manager: ModelDownloadManager!
    private var completed: [String] = []
    private var removed: [String] = []
    /// 管理器要过的后台运行时间（名字），和交还的次数
    private var activities: [String] = []
    private var endedActivities = 0

    private let model = LocalModel(
        id: "test-model", title: "测试模型", fileName: "test-model.gguf",
        downloadURL: URL(string: "https://example.com/test-model.gguf")!,
        bytes: 1000, residentMiB: 10, note: "")

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("downloads-\(UUID().uuidString)")
        modelsDirectory = root.appendingPathComponent("Shared/Models")
        stagingDirectory = root.appendingPathComponent("Staging")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        transport = FakeDownloadTransport()
        network = FakeNetwork()
        manager = makeManager()
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    private func makeManager(catalog: [LocalModel]? = nil) -> ModelDownloadManager {
        let manager = ModelDownloadManager(
            catalog: catalog ?? [model], transport: transport, directory: modelsDirectory,
            stagingDirectory: stagingDirectory, network: network, freeSpace: { [unowned self] in self.freeSpace },
            beginBackgroundActivity: { [weak self] name in
                self?.activities.append(name)
                return { [weak self] in self?.endedActivities += 1 }
            })
        manager.onCompleted = { [unowned self] in self.completed.append($0.id) }
        manager.onRemoved = { [unowned self] in self.removed.append($0.id) }
        return manager
    }

    private var finalFile: URL { modelsDirectory.appendingPathComponent(model.fileName) }
    private var partFile: URL { modelsDirectory.appendingPathComponent(model.fileName + ".part") }
    /// 传输层放下的完整文件，和核对期间改成的名字
    private var downloadFile: URL { stagingDirectory.appendingPathComponent("\(model.id).download") }
    private var verifyFile: URL { stagingDirectory.appendingPathComponent("\(model.id).verify") }
    private var resumeFile: URL { stagingDirectory.appendingPathComponent("\(model.id).resume") }
    private var state: ModelDownloadState { manager.state(of: model) }

    private func exists(_ url: URL) -> Bool { FileManager.default.fileExists(atPath: url.path) }

    @discardableResult
    private func stagedFile(size: Int) throws -> URL {
        try FileManager.default.createDirectory(at: stagingDirectory, withIntermediateDirectories: true)
        let url = stagingDirectory.appendingPathComponent("\(model.id).download")
        try Data(count: size).write(to: url)
        return url
    }

    private func resumeData(received: Int) throws -> Data {
        try PropertyListSerialization.data(
            fromPropertyList: ["NSURLSessionResumeBytesReceived": received], format: .binary, options: 0)
    }

    // MARK: - SHA-256 校验

    /// 目录里带哈希的同一个模型
    private func checksummedModel(sha256: String) -> LocalModel {
        LocalModel(
            id: model.id, title: model.title, fileName: model.fileName, downloadURL: model.downloadURL,
            bytes: model.bytes, residentMiB: model.residentMiB, note: model.note, sha256: sha256)
    }

    /// 目录里带哈希的模型：哈希核对通过才挪进 Models 目录
    private func makeChecksummedManager(sha256: String) -> (ModelDownloadManager, LocalModel) {
        let checked = checksummedModel(sha256: sha256)
        return (makeManager(catalog: [checked]), checked)
    }

    /// 在暂存目录里放一个文件（size 个零字节），当作上次运行留下的
    @discardableResult
    private func leftover(_ url: URL, size: Int = 1000) throws -> URL {
        try FileManager.default.createDirectory(at: stagingDirectory, withIntermediateDirectories: true)
        try Data(count: size).write(to: url)
        return url
    }

    /// 1000 个零字节的 SHA-256（与 stagedFile(size: 1000) 写出的内容一致）
    private var zerosSHA256: String {
        SHA256.hash(data: Data(count: 1000)).map { String(format: "%02x", $0) }.joined()
    }

    private func waitUntil(_ condition: @escaping () -> Bool, timeout: TimeInterval = 5) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition(), Date() < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    func testMatchingChecksumInstallsTheFile() async throws {
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256.uppercased())
        manager.start(checked)
        transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
        // 核对在后台进行，这期间是 finishing，文件还没进 Models 目录；核对的是改了名的 <id>.verify
        XCTAssertEqual(manager.state(of: checked), .finishing)
        XCTAssertFalse(exists(finalFile))
        XCTAssertFalse(exists(downloadFile))
        XCTAssertTrue(exists(verifyFile))
        // 核对期间要着后台运行时间，核对完才交还
        XCTAssertEqual(activities, ["verify-\(checked.id)"])
        XCTAssertEqual(endedActivities, 0)
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertEqual(completed, [checked.id])
        XCTAssertEqual(endedActivities, 1)
    }

    func testMismatchingChecksumDiscardsTheFile() async throws {
        let (manager, checked) = makeChecksummedManager(sha256: String(repeating: "0", count: 64))
        manager.start(checked)
        let staged = try stagedFile(size: 1000)
        transport.emit(.finished(id: checked.id, file: staged))
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .failed(.checksumMismatch))
        XCTAssertFalse(exists(finalFile))
        XCTAssertFalse(exists(partFile))
        XCTAssertFalse(exists(staged))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertTrue(completed.isEmpty)
        XCTAssertEqual(endedActivities, 1, "对不上也要交还后台时间")
        XCTAssertTrue(DownloadFailure.checksumMismatch.message.contains("校验"))
    }

    func testCancelWhileVerifyingDropsTheResult() async throws {
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        manager.start(checked)
        transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(manager.state(of: checked), .finishing)
        manager.cancel(checked)
        XCTAssertFalse(exists(verifyFile), "取消时删掉正在核对的文件")
        await waitUntil { self.endedActivities == 1 }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(manager.state(of: checked), .notDownloaded)
        XCTAssertFalse(exists(finalFile))
        XCTAssertTrue(completed.isEmpty)
        XCTAssertEqual(endedActivities, 1, "被取消的核对结束时也要交还后台时间")
    }

    /// 核对期间取消，马上重新下载并下完：两次核对的是同一个路径 <id>.verify。被取消的那次核对结果回来时
    /// 只能丢掉结果，不能删这个路径上的文件——那已经是重新下载、正在核对的新文件
    func testCancelWhileVerifyingThenRedownloadInstallsTheNewFile() async throws {
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        manager.start(checked)
        transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(manager.state(of: checked), .finishing)
        // 让被取消的那次核对先算完、它的结果先排进主线程：先让出一次，让它开始算哈希，再把主线程占住一会儿。
        // 不这样做，两次核对谁先回来看调度，重新下载的那次先装好时，「旧结果删文件」的错误就测不出来
        await Task.yield()
        blockCurrentThread(for: 0.2)
        manager.cancel(checked)
        manager.start(checked)
        transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(manager.state(of: checked), .finishing)
        XCTAssertTrue(exists(verifyFile))
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertEqual(completed, [checked.id])
        await waitUntil { self.endedActivities == 2 }
        XCTAssertEqual(activities, ["verify-\(checked.id)", "verify-\(checked.id)"])
        XCTAssertEqual(endedActivities, 2, "被取消的和重新下载的两次核对都交还了后台时间")
    }

    func testSizeOnlyCheckNeedsNoBackgroundTime() throws {
        manager.start(model)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(state, .downloaded, "只核对大小时当场装好")
        XCTAssertTrue(activities.isEmpty)
    }

    /// 核对期间又来一个同一模型的完成事件（传输层积压或重复发的）：只删事件里的 <id>.download，
    /// 正在核对的文件不受影响
    func testDuplicateFinishedWhileVerifyingLeavesTheFileBeingVerifiedAlone() async throws {
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        manager.start(checked)
        transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(manager.state(of: checked), .finishing)
        let duplicate = try stagedFile(size: 1000)
        transport.emit(.finished(id: checked.id, file: duplicate))
        XCTAssertFalse(exists(duplicate))
        XCTAssertTrue(exists(verifyFile))
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertEqual(completed, [checked.id])
    }

    // MARK: - 启动时留在暂存目录里的文件

    /// 核对到一半进程被杀（这里丢掉管理器来模拟），重新打开时接着核对、装好
    func testRelaunchWhileVerifyingFinishesTheDownload() async throws {
        let checked = checksummedModel(sha256: zerosSHA256)
        weak var weakFirst: ModelDownloadManager?
        do {
            let first = makeManager(catalog: [checked])
            weakFirst = first
            first.start(checked)
            transport.emit(.finished(id: checked.id, file: try stagedFile(size: 1000)))
            XCTAssertEqual(first.state(of: checked), .finishing)
        }
        XCTAssertNil(weakFirst, "旧的管理器应当已经不在了")
        XCTAssertTrue(exists(verifyFile), "旧管理器的核对结果没人收，文件要留给下次启动")

        transport = FakeDownloadTransport()
        let second = makeManager(catalog: [checked])
        XCTAssertEqual(second.state(of: checked), .finishing)
        await waitUntil { second.state(of: checked) != .finishing }
        XCTAssertEqual(second.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertEqual(completed, [checked.id])
        await waitUntil { self.endedActivities == 2 }
        XCTAssertEqual(endedActivities, 2, "两次核对都交还了后台时间")
    }

    /// 上次留下核对到一半的 <id>.verify，内容是对的：启动时重新核对后装上
    func testLeftoverWithTheRightHashIsInstalledAtLaunch() async throws {
        try leftover(verifyFile)
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        XCTAssertEqual(manager.state(of: checked), .finishing)
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertEqual(completed, [checked.id], "和正常下完一样，第一个模型会被自动选上")
        XCTAssertEqual(activities, ["verify-\(checked.id)"])
        XCTAssertEqual(endedActivities, 1)
    }

    /// 完成事件还没交给管理器 App 就退出了：留下 <id>.download（和下载途中存的续传数据）。
    /// 启动时接手它；传输层积压的同一个完成事件随后才送到，不能删掉正在核对的文件
    func testLeftoverDownloadIsAdoptedBeforeTheQueuedFinishedEventArrives() async throws {
        try leftover(downloadFile)
        try resumeData(received: 400).write(to: resumeFile)
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        XCTAssertEqual(manager.state(of: checked), .finishing)
        XCTAssertFalse(exists(downloadFile))
        XCTAssertTrue(exists(verifyFile))
        XCTAssertFalse(exists(resumeFile), "文件已经下完，续传数据没用了")

        transport.emit(.finished(id: checked.id, file: downloadFile))
        XCTAssertTrue(exists(verifyFile))
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertEqual(completed, [checked.id])
    }

    /// 两个都在（核对到一半时又下完了一次）：用更新的 <id>.download，旧的 <id>.verify 被它盖掉
    func testLeftoverDownloadWinsOverAnOlderVerifyFile() async throws {
        try leftover(verifyFile, size: 999)
        try leftover(downloadFile)
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertFalse(exists(downloadFile))
        XCTAssertFalse(exists(verifyFile))
    }

    /// 留下的文件内容不对：校验失败，文件删掉
    func testLeftoverWithAWrongHashFailsAndIsRemovedAtLaunch() async throws {
        try leftover(downloadFile)
        let (manager, checked) = makeChecksummedManager(sha256: String(repeating: "0", count: 64))
        await waitUntil { manager.state(of: checked) != .finishing }
        XCTAssertEqual(manager.state(of: checked), .failed(.checksumMismatch))
        XCTAssertFalse(exists(downloadFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertFalse(exists(finalFile))
        XCTAssertFalse(exists(partFile))
        XCTAssertTrue(completed.isEmpty)
    }

    /// 留下的文件大小不对：当场判失败、删掉，不用算哈希
    func testLeftoverWithAWrongSizeIsRemovedAtLaunch() throws {
        try leftover(verifyFile, size: 999)
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        XCTAssertEqual(manager.state(of: checked), .failed(.sizeMismatch(expected: 1000, actual: 999)))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertTrue(activities.isEmpty)
    }

    /// 模型已经装好：留下的文件直接删掉，不再核对，也不算一次新的下载完成
    func testLeftoverForAnInstalledModelIsRemovedAtLaunch() throws {
        try FileManager.default.createDirectory(at: modelsDirectory, withIntermediateDirectories: true)
        try Data(count: 1000).write(to: finalFile)
        try leftover(downloadFile)
        try leftover(verifyFile)
        let (manager, checked) = makeChecksummedManager(sha256: zerosSHA256)
        XCTAssertEqual(manager.state(of: checked), .downloaded)
        XCTAssertFalse(exists(downloadFile))
        XCTAssertFalse(exists(verifyFile))
        XCTAssertTrue(exists(finalFile))
        XCTAssertTrue(activities.isEmpty, "不用再核对")
        XCTAssertTrue(completed.isEmpty)
    }

    /// 只核对大小时（假下载通道）留下的文件也能接手：当场装好
    func testLeftoverIsInstalledAtLaunchWhenOnlyTheSizeIsChecked() throws {
        try leftover(downloadFile)
        manager = makeManager()
        XCTAssertEqual(state, .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertFalse(exists(downloadFile))
    }

    func testStagingDirectoryIsCreatedAndExcludedFromBackup() throws {
        XCTAssertTrue(exists(stagingDirectory))
        // 新建一个 URL 来读，免得读到缓存的属性
        let values = try URL(fileURLWithPath: stagingDirectory.path).resourceValues(forKeys: [.isExcludedFromBackupKey])
        XCTAssertEqual(values.isExcludedFromBackup, true)
    }

    func testChecksumOfARealFile() async throws {
        let url = root.appendingPathComponent("hello.txt")
        try Data("hello".utf8).write(to: url)
        let hex = await FileDigest.sha256Hex(of: url)
        XCTAssertEqual(hex, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        let missing = await FileDigest.sha256Hex(of: root.appendingPathComponent("nope"))
        XCTAssertNil(missing)
    }

    func testCatalogModelsCarryTheirPublishedChecksums() {
        for model in ModelCatalog.all {
            XCTAssertEqual(model.sha256?.count, 64, model.id)
            XCTAssertEqual(model.sha256, model.sha256?.lowercased(), model.id)
        }
    }

    // MARK: - 开始、进度

    func testStartShowsDownloadingAndTellsTransport() {
        XCTAssertEqual(state, .notDownloaded)
        manager.start(model)
        XCTAssertEqual(state, .downloading(received: 0, total: 1000))
        XCTAssertEqual(transport.starts, [.init(id: model.id, url: model.downloadURL, resumeData: nil)])
    }

    func testStartingTwiceDoesNotStartTwoTasks() {
        manager.start(model)
        manager.start(model)
        XCTAssertEqual(transport.starts.count, 1)
    }

    func testProgressUpdatesState() {
        manager.start(model)
        transport.emit(.progress(id: model.id, received: 250, total: 1000))
        XCTAssertEqual(state, .downloading(received: 250, total: 1000))
        // 服务器没给长度时用目录里的字节数
        transport.emit(.progress(id: model.id, received: 500, total: -1))
        XCTAssertEqual(state, .downloading(received: 500, total: 1000))
    }

    // MARK: - 暂停、继续

    func testPauseThenResumeUsesResumeData() throws {
        manager.start(model)
        transport.emit(.progress(id: model.id, received: 400, total: 1000))
        manager.pause(model)
        XCTAssertEqual(transport.pauses, [model.id])
        XCTAssertEqual(state, .paused(received: 400, total: 1000))

        let data = try resumeData(received: 400)
        transport.emit(.paused(id: model.id, resumeData: data))
        XCTAssertEqual(state, .paused(received: 400, total: 1000))

        manager.resume(model)
        XCTAssertEqual(transport.starts.count, 2)
        XCTAssertEqual(transport.starts[1].resumeData, data)
        XCTAssertEqual(state, .downloading(received: 400, total: 1000))
    }

    func testProgressWhilePausedIsIgnored() {
        manager.start(model)
        transport.emit(.progress(id: model.id, received: 400, total: 1000))
        manager.pause(model)
        transport.emit(.progress(id: model.id, received: 450, total: 1000))
        XCTAssertEqual(state, .paused(received: 400, total: 1000))
    }

    func testResumeBeforePauseSettlesWaitsForResumeData() throws {
        manager.start(model)
        manager.pause(model)
        manager.resume(model)               // 暂停还没落定
        XCTAssertEqual(transport.starts.count, 1)
        XCTAssertEqual(state, .downloading(received: 0, total: 1000))

        let data = try resumeData(received: 300)
        transport.emit(.paused(id: model.id, resumeData: data))
        XCTAssertEqual(transport.starts.count, 2)
        XCTAssertEqual(transport.starts[1].resumeData, data)
        XCTAssertEqual(state, .downloading(received: 300, total: 1000))
    }

    func testPauseWithoutResumeDataRestartsFromScratch() {
        manager.start(model)
        transport.emit(.progress(id: model.id, received: 400, total: 1000))
        manager.pause(model)
        transport.emit(.paused(id: model.id, resumeData: nil))
        XCTAssertEqual(state, .paused(received: 0, total: 1000))
        manager.resume(model)
        XCTAssertNil(transport.starts.last?.resumeData)
    }

    func testPausedStateSurvivesRelaunch() throws {
        manager.start(model)
        manager.pause(model)
        let data = try resumeData(received: 640)
        transport.emit(.paused(id: model.id, resumeData: data))

        let second = FakeDownloadTransport()
        transport = second
        manager = makeManager()
        XCTAssertEqual(manager.state(of: model), .paused(received: 640, total: 1000))
        manager.resume(model)
        XCTAssertEqual(second.starts.last?.resumeData, data)
    }

    // MARK: - 失败

    func testFailureShowsMessageAndRetryUsesResumeData() throws {
        manager.start(model)
        let data = try resumeData(received: 200)
        transport.emit(.failed(id: model.id, failure: .network("网络连接中断"), resumeData: data))
        XCTAssertEqual(state, .failed(.network("网络连接中断")))

        manager.start(model)   // 重试
        XCTAssertEqual(transport.starts.count, 2)
        XCTAssertEqual(transport.starts[1].resumeData, data)
        XCTAssertEqual(state, .downloading(received: 200, total: 1000))
    }

    func testHTTPFailure() {
        manager.start(model)
        transport.emit(.failed(id: model.id, failure: .http(404), resumeData: nil))
        XCTAssertEqual(state, .failed(.http(404)))
        XCTAssertTrue(DownloadFailure.http(404).message.contains("404"))
        XCTAssertFalse(exists(finalFile))
    }

    // MARK: - 完成与校验

    func testSuccessMovesFileToModelsDirectory() throws {
        manager.start(model)
        let staged = try stagedFile(size: 1000)
        transport.emit(.finished(id: model.id, file: staged))

        XCTAssertEqual(state, .downloaded)
        XCTAssertTrue(exists(finalFile))
        XCTAssertEqual(try finalFile.resourceValues(forKeys: [.fileSizeKey]).fileSize, 1000)
        XCTAssertFalse(exists(partFile), "不能留下 .part")
        XCTAssertFalse(exists(staged), "暂存文件要挪走")
        XCTAssertEqual(completed, [model.id])
        XCTAssertEqual(manager.downloadedIDs, [model.id])
    }

    func testSizeMismatchIsRejectedAndNothingLeftBehind() throws {
        manager.start(model)
        let staged = try stagedFile(size: 999)
        transport.emit(.finished(id: model.id, file: staged))

        XCTAssertEqual(state, .failed(.sizeMismatch(expected: 1000, actual: 999)))
        XCTAssertFalse(exists(finalFile), "键盘不能读到大小不对的文件")
        XCTAssertFalse(exists(partFile))
        XCTAssertFalse(exists(staged))
        XCTAssertTrue(completed.isEmpty)
        XCTAssertTrue(manager.downloadedIDs.isEmpty)
    }

    func testRedownloadReplacesAnOldFile() throws {
        try FileManager.default.createDirectory(at: modelsDirectory, withIntermediateDirectories: true)
        try Data(count: 5).write(to: finalFile)   // 旧的、大小不对的
        manager = makeManager()
        XCTAssertEqual(state, .notDownloaded)
        manager.start(model)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(try finalFile.resourceValues(forKeys: [.fileSizeKey]).fileSize, 1000)
    }

    func testFinishedWhileAppWasNotRunning() throws {
        // App 被杀掉，后台下完，重新打开时才收到完成事件：状态里什么都没有
        XCTAssertEqual(state, .notDownloaded)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(state, .downloaded)
        XCTAssertEqual(completed, [model.id])
    }

    func testUnknownModelFileIsDiscarded() throws {
        let stray = try stagedFile(size: 10)
        transport.emit(.finished(id: "没有这个模型", file: stray))
        XCTAssertFalse(exists(stray))
    }

    // MARK: - 取消、删除

    func testCancelClearsStateAndIgnoresLateEvents() throws {
        manager.start(model)
        transport.emit(.progress(id: model.id, received: 100, total: 1000))
        manager.cancel(model)
        XCTAssertEqual(transport.cancels, [model.id])
        XCTAssertEqual(state, .notDownloaded)

        transport.emit(.progress(id: model.id, received: 200, total: 1000))
        XCTAssertEqual(state, .notDownloaded)

        let late = try stagedFile(size: 1000)
        transport.emit(.finished(id: model.id, file: late))
        XCTAssertEqual(state, .notDownloaded)
        XCTAssertFalse(exists(finalFile))
        XCTAssertFalse(exists(late))
    }

    func testCancelAfterPauseDropsResumeData() throws {
        manager.start(model)
        manager.pause(model)
        transport.emit(.paused(id: model.id, resumeData: try resumeData(received: 500)))
        manager.cancel(model)
        XCTAssertEqual(state, .notDownloaded)

        manager = makeManager()   // 重新打开也不该冒出「已暂停」
        XCTAssertEqual(state, .notDownloaded)
        manager.start(model)
        XCTAssertNil(transport.starts.last?.resumeData)
    }

    func testCancelThenStartAgainWorks() throws {
        manager.start(model)
        manager.cancel(model)
        manager.start(model)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(state, .downloaded)
    }

    func testDeleteRemovesFileAndNotifies() throws {
        manager.start(model)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        XCTAssertEqual(state, .downloaded)

        manager.delete(model)
        XCTAssertFalse(exists(finalFile))
        XCTAssertEqual(state, .notDownloaded)
        XCTAssertEqual(removed, [model.id])
    }

    // MARK: - 磁盘状态

    func testInitialStateFromDisk() throws {
        try FileManager.default.createDirectory(at: modelsDirectory, withIntermediateDirectories: true)
        try Data(count: 1000).write(to: finalFile)
        XCTAssertEqual(makeManager().state(of: model), .downloaded)

        try Data(count: 999).write(to: finalFile)
        XCTAssertEqual(makeManager().state(of: model), .notDownloaded)
    }

    func testStalePartialFileIsRemovedAtLaunch() throws {
        try FileManager.default.createDirectory(at: modelsDirectory, withIntermediateDirectories: true)
        try Data(count: 400).write(to: partFile)
        manager = makeManager()
        XCTAssertFalse(exists(partFile))
        XCTAssertEqual(state, .notDownloaded)
    }

    func testModelsDirectoryIsCreatedAndExcludedFromBackup() throws {
        XCTAssertTrue(exists(modelsDirectory))
        let values = try modelsDirectory.resourceValues(forKeys: [.isExcludedFromBackupKey])
        XCTAssertEqual(values.isExcludedFromBackup, true)
    }

    func testRefreshNoticesDeletedFile() throws {
        manager.start(model)
        transport.emit(.finished(id: model.id, file: try stagedFile(size: 1000)))
        try FileManager.default.removeItem(at: finalFile)
        manager.refresh()
        XCTAssertEqual(state, .notDownloaded)
        XCTAssertEqual(removed, [model.id])
    }

    func testReconnectFindsTaskStillRunningInBackground() {
        XCTAssertEqual(transport.reconnectCount, 1)
        transport.emit(.active(id: model.id, received: 321, total: 1000))
        XCTAssertEqual(state, .downloading(received: 321, total: 1000))
        transport.emit(.progress(id: model.id, received: 400, total: 1000))
        XCTAssertEqual(state, .downloading(received: 400, total: 1000))
    }

    // MARK: - 空间与网络

    func testInsufficientSpaceBlocksStart() {
        freeSpace = 1000 + StorageCheck.safetyMargin - 1
        let expectedNeed = StorageCheck.requiredBytes(remaining: 1000)
        XCTAssertEqual(manager.preflight(model), .insufficientSpace(needed: expectedNeed, available: freeSpace))
        manager.start(model)
        XCTAssertEqual(state, .failed(.insufficientSpace(needed: expectedNeed, available: freeSpace)))
        XCTAssertTrue(transport.starts.isEmpty, "空间不够不能发起下载")
        XCTAssertTrue(state != .downloaded)
    }

    func testExactlyEnoughSpaceStarts() {
        freeSpace = StorageCheck.requiredBytes(remaining: 1000)
        XCTAssertEqual(manager.preflight(model), .ok)
        manager.start(model)
        XCTAssertEqual(transport.starts.count, 1)
    }

    func testResumeOnlyNeedsRemainingSpace() throws {
        manager.start(model)
        manager.pause(model)
        transport.emit(.paused(id: model.id, resumeData: try resumeData(received: 900)))
        freeSpace = StorageCheck.requiredBytes(remaining: 100)   // 够下剩下的 100 字节
        XCTAssertEqual(manager.preflight(model), .ok)
    }

    func testCellularNeedsConfirmation() {
        network.isExpensive = true
        XCTAssertEqual(manager.preflight(model), .cellular)
        network.isExpensive = false
        XCTAssertEqual(manager.preflight(model), .ok)
    }

    func testNoSpaceCheckResultOverridesCellularWarning() {
        network.isExpensive = true
        freeSpace = 0
        if case .insufficientSpace = manager.preflight(model) {} else { XCTFail("空间不够应先于蜂窝提醒") }
    }

    func testFailedStateCanBeAbandoned() {
        manager.start(model)
        transport.emit(.failed(id: model.id, failure: .network("没有网络连接"), resumeData: nil))
        manager.cancel(model)
        XCTAssertEqual(state, .notDownloaded)
    }
}

/// 把当前线程（在 @MainActor 的测试里就是主线程）占住一会儿，不让出给别的任务。
/// 异步函数里不能直接调用 `Thread.sleep`（Swift 6 会报错），放在同步函数里调用；
/// 也不能换成 `Task.sleep`：它会把主线程让出去，被取消的那次核对的结果就不一定先排进主线程了
private func blockCurrentThread(for seconds: TimeInterval) {
    Thread.sleep(forTimeInterval: seconds)
}
