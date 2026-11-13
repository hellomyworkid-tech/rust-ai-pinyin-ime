import Foundation
import Combine
import Compression
import CryptoKit

/// 本地模型用的下载管理器
typealias ModelDownloadManager = DownloadManager<LocalModel>
/// 完整词库用的下载管理器（另一条后台下载会话、另一个目录）
typealias LexiconDownloadManager = DownloadManager<LexiconRelease>

/// 可下载文件（本地模型、完整词库）的下载状态机。下面说的「模型」「Models 目录」对词库同样适用，
/// 词库的目录是 App Group 里的 Lexicon。
///
/// 文件落地的规则（键盘用 mmap 直接读 Models 目录里的 fileName，所以不能让它看到没下完的文件）：
/// 1. 传输层把下好的文件放进 App 自己的暂存目录（stagingDirectory，排除在备份之外），名字是 <id>.download；
/// 2. 管理器核对大小，不等于目录里写的字节数就丢弃；目录里有 SHA-256 的，先把文件改名成 <id>.verify，
///    再在后台线程上核对内容，不一致也丢弃。传输层迟到或重复的完成事件（会删掉事件里的文件）、
///    下一次下载完成（会覆盖 <id>.download）都只碰 <id>.download，碰不到正在核对的文件。
///    核对期间状态是 .finishing，界面显示「正在校验文件」；同时向系统要一段后台时间，
///    免得 App 被后台下载唤醒、还没核对完就被挂起；
/// 3. 先挪到 Models/<fileName>.part，再改名成 Models/<fileName>。键盘只认最后这个名字。
///    下载的是 xz 压缩包（词库）时，核对完压缩包先在后台解压到 Models/<fileName>.<随机串>.part，
///    核对解压后的大小和 SHA-256，再改名成 Models/<fileName>；每次解压用自己的 .part，
///    解压途中取消、紧接着又下完一次时，两次解压不会写同一个文件。
/// 暂停时传输层交回续传数据，存在暂存目录里，App 重新打开后还能接着下。
/// 核对途中进程被杀，或者完成事件还没交到管理器手里 App 就退出了，暂存目录里会留下下完的文件：
/// 启动时模型已经装好的就删掉它，否则重新核对大小和 SHA-256，通过就装上，不通过就删掉。
@MainActor
final class DownloadManager<Item: DownloadableFile>: ObservableObject {
    @Published private(set) var states: [String: ModelDownloadState] = [:]

    /// 某个模型下完并通过校验
    var onCompleted: ((Item) -> Void)?
    /// 某个模型被删掉
    var onRemoved: ((Item) -> Void)?

    let catalog: [Item]
    private let transport: DownloadTransport
    /// 装好的文件放在这里（键盘读的目录）
    private let modelsDirectory: URL?
    private let stagingDirectory: URL
    private let freeSpace: () -> Int64
    private let network: NetworkConditions
    private let fileManager = FileManager.default

    private var resumeData: [String: Data] = [:]
    /// 已点暂停、还在等传输层交回续传数据的任务
    private var awaitingPause: Set<String> = []
    /// 暂停还没完成就点了继续：等续传数据到了再接着下
    private var resumeAfterPause: Set<String> = []
    /// 本次运行里取消过的任务；它们迟到的完成事件要丢掉
    private var cancelledIDs: Set<String> = []
    /// 正在后台核对 SHA-256（或解压）的模型。取消或删除时清掉，核对、解压结果回来时据此丢弃
    private var verifying: [String: UUID] = [:]
    /// 传输层已经查完上次留在后台的任务（收到 .reconnected）
    private(set) var hasReconnected = false
    /// 等 .reconnected 到了再做的事
    private var afterReconnectActions: [() -> Void] = []
    /// 是否核对 SHA-256（调试用的假下载通道写的是空数据，要关掉）
    private let verifiesChecksums: Bool
    /// 开始一段后台运行时间（参数是名字），返回结束它的闭包。核对 SHA-256 期间用
    private let beginBackgroundActivity: @MainActor (String) -> @MainActor () -> Void

    /// beginBackgroundActivity：App 里接 UIApplication.beginBackgroundTask（见 AppModel.live()）。
    /// 默认什么都不做：单元测试不挂宿主 App，没有 UIApplication
    init(catalog: [Item],
         transport: DownloadTransport,
         directory: URL?,
         stagingDirectory: URL,
         network: NetworkConditions,
         freeSpace: @escaping () -> Int64 = { StorageCheck.availableBytes() },
         verifiesChecksums: Bool = true,
         beginBackgroundActivity: @escaping @MainActor (String) -> @MainActor () -> Void = { _ in {} }) {
        self.catalog = catalog
        self.verifiesChecksums = verifiesChecksums
        self.beginBackgroundActivity = beginBackgroundActivity
        self.transport = transport
        self.modelsDirectory = directory
        self.stagingDirectory = stagingDirectory
        self.network = network
        self.freeSpace = freeSpace

        prepareStagingDirectory()
        prepareModelsDirectory()
        removeStalePartialFiles()
        removeUnknownStagedFiles()
        scanDisk()
        // 要在接上传输层之前：传输层积压的完成事件随后才送到，送到时留下的文件已经改名、在核对了
        recoverStagedFiles()
        transport.onEvent = { [weak self] event in
            MainActor.assumeIsolated { self?.handle(event) }
        }
        transport.reconnect()
    }

    // MARK: - 查询

    func state(of model: Item) -> ModelDownloadState {
        states[model.id] ?? .notDownloaded
    }

    var downloadedIDs: [String] {
        catalog.filter { state(of: $0) == .downloaded }.map(\.id)
    }

    /// 点「下载」或「继续」之前先问一下：空间够不够、是不是蜂窝网络
    func preflight(_ model: Item) -> DownloadPreflight {
        let remaining = max(0, model.bytes - knownReceivedBytes(of: model)) + model.expansionBytes
        let available = freeSpace()
        if !StorageCheck.hasEnoughSpace(remaining: remaining, available: available) {
            return .insufficientSpace(needed: StorageCheck.requiredBytes(remaining: remaining), available: available)
        }
        return network.isExpensive ? .cellular : .ok
    }

    /// 等传输层查完上次留在后台的任务再做（已经查完就马上做）。自动开始下载要等它：
    /// 还在后台跑的任务这时才报 .active、状态才变成下载中，提前开始会重复下载
    func afterReconnect(_ action: @escaping () -> Void) {
        if hasReconnected {
            action()
        } else {
            afterReconnectActions.append(action)
        }
    }

    // MARK: - 操作

    /// 开始下载；暂停过或失败过的会用保存的续传数据接着下
    func start(_ model: Item) {
        switch state(of: model) {
        case .downloading, .finishing, .downloaded:
            return
        case .paused where awaitingPause.contains(model.id):
            // 暂停还没落定：先标记，续传数据一到就继续
            resumeAfterPause.insert(model.id)
            states[model.id] = .downloading(received: knownReceivedBytes(of: model), total: model.bytes)
            return
        default:
            break
        }
        guard modelsDirectory != nil else {
            states[model.id] = .failed(.storage("读不到 App 和键盘共用的存储位置"))
            return
        }
        let data = resumeData[model.id] ?? readResumeFile(model.id)
        let received = data.flatMap(ResumeInfo.bytesReceived(in:)) ?? 0
        let remaining = max(0, model.bytes - received) + model.expansionBytes
        let available = freeSpace()
        guard StorageCheck.hasEnoughSpace(remaining: remaining, available: available) else {
            states[model.id] = .failed(.insufficientSpace(
                needed: StorageCheck.requiredBytes(remaining: remaining), available: available))
            return
        }
        prepareModelsDirectory()
        resumeData[model.id] = nil
        cancelledIDs.remove(model.id)
        removeResumeFile(model.id)
        states[model.id] = .downloading(received: received, total: model.bytes)
        transport.start(id: model.id, url: model.downloadURL, resumeData: data)
    }

    func pause(_ model: Item) {
        guard case .downloading(let received, let total) = state(of: model) else { return }
        states[model.id] = .paused(received: received, total: total)
        awaitingPause.insert(model.id)
        transport.pause(id: model.id)
    }

    func resume(_ model: Item) { start(model) }

    /// 取消下载，或丢掉暂停 / 失败的半成品。已下完的模型用 delete。
    func cancel(_ model: Item) {
        transport.cancel(id: model.id)
        cancelledIDs.insert(model.id)
        verifying[model.id] = nil
        awaitingPause.remove(model.id)
        resumeAfterPause.remove(model.id)
        resumeData[model.id] = nil
        removeResumeFile(model.id)
        removeStagedFile(model.id)
        if state(of: model) != .downloaded { states[model.id] = nil }
    }

    /// 删除已下载的模型（以及一切没下完的东西）
    func delete(_ model: Item) {
        let wasDownloaded = state(of: model) == .downloaded
        cancel(model)
        if let directory = modelsDirectory {
            try? fileManager.removeItem(at: directory.appendingPathComponent(model.fileName))
            try? fileManager.removeItem(at: directory.appendingPathComponent(model.fileName + ".part"))
        }
        states[model.id] = nil
        if wasDownloaded { onRemoved?(model) }
    }

    /// 回到前台时重新对一遍磁盘：别处删了文件，状态要跟着变
    func refresh() {
        for model in catalog {
            switch state(of: model) {
            case .downloaded where !fileIsComplete(model):
                states[model.id] = nil
                onRemoved?(model)
            case .notDownloaded where fileIsComplete(model):
                states[model.id] = .downloaded
            default:
                break
            }
        }
    }

    // MARK: - 传输层事件

    private func handle(_ event: DownloadEvent) {
        switch event {
        case .progress(let id, let received, let total):
            guard let model = model(id), case .downloading = state(of: model) else { return }
            states[id] = .downloading(received: received, total: total > 0 ? total : model.bytes)

        case .active(let id, let received, let total):
            guard let model = model(id) else { return }
            switch state(of: model) {
            case .notDownloaded, .failed, .paused:
                // App 重新打开时发现任务还在后台跑
                removeResumeFile(id)
                states[id] = .downloading(received: received, total: total > 0 ? total : model.bytes)
            default:
                break
            }

        case .finished(let id, let file):
            // 取消过的任务迟到的文件不要；其余（包括 App 重新打开后后台补发的）都收下
            guard let model = model(id), !cancelledIDs.contains(id) else {
                try? fileManager.removeItem(at: file)
                return
            }
            switch state(of: model) {
            case .finishing, .downloaded:
                // 迟到或重复的完成事件（比如启动时已经接手了留下的文件，传输层积压的事件随后才送到）：
                // 事件里的 <id>.download 不要了。正在核对的是 <id>.verify，不受影响
                try? fileManager.removeItem(at: file)
            default:
                awaitingPause.remove(id)
                resumeAfterPause.remove(id)
                removeResumeFile(id)
                finalize(model, staged: file)
            }

        case .failed(let id, let failure, let data):
            guard let model = model(id), case .downloading = state(of: model) else { return }
            if let data { storeResumeData(data, for: id) }
            states[id] = .failed(failure)

        case .reconnected:
            hasReconnected = true
            let actions = afterReconnectActions
            afterReconnectActions = []
            actions.forEach { $0() }

        case .paused(let id, let data):
            guard let model = model(id), awaitingPause.remove(id) != nil else { return }
            if let data { storeResumeData(data, for: id) }
            if resumeAfterPause.remove(id) != nil {
                states[id] = .paused(received: 0, total: model.bytes)
                start(model)
            } else if case .paused(let received, let total) = state(of: model) {
                let known = data.flatMap(ResumeInfo.bytesReceived(in:)) ?? received
                states[id] = .paused(received: data == nil ? 0 : known, total: total)
            }
        }
    }

    /// 校验大小（目录里有 SHA-256 的再核对内容），再挪到 Models 目录：先用 .part 名字，改名成正式文件名才算数。
    /// staged 是传输层放下的 <id>.download，或启动时找到的、上次没核对完的 <id>.verify
    private func finalize(_ model: Item, staged: URL) {
        states[model.id] = .finishing
        guard let directory = modelsDirectory else {
            try? fileManager.removeItem(at: staged)
            states[model.id] = .failed(.storage("读不到 App 和键盘共用的存储位置"))
            return
        }
        let size = (try? staged.resourceValues(forKeys: [.fileSizeKey]).fileSize).map(Int64.init) ?? -1
        guard size == model.bytes else {
            try? fileManager.removeItem(at: staged)
            states[model.id] = .failed(.sizeMismatch(expected: model.bytes, actual: max(0, size)))
            return
        }
        let expected = verifiesChecksums ? model.sha256?.lowercased() : nil
        guard expected != nil || model.isXZCompressed else {
            // 只核对大小、不用解压：在同一拍里装好，不用等
            install(model, staged: staged, in: directory)
            return
        }
        // 先改名成核对专用的 <id>.verify（同一个目录，只是改名）。传输层迟到或重复的完成事件、
        // 下一次下载完成都只碰 <id>.download，碰不到正在核对的文件
        let file = verifyFileURL(model.id)
        if staged.standardizedFileURL.path != file.standardizedFileURL.path {
            do {
                try? fileManager.removeItem(at: file)
                try fileManager.moveItem(at: staged, to: file)
            } catch {
                try? fileManager.removeItem(at: staged)
                states[model.id] = .failed(.storage(error.localizedDescription))
                return
            }
        }
        // 四五百 MB 的哈希在主线程上要算一两秒，放到后台；期间被取消或删除就丢掉结果。
        // 核对期间向系统要一段后台时间：App 被后台下载唤醒时，系统处理完下载事件就会把它挂起，等不到核对完。
        // 时间用完了也不要紧：.verify 还在，挂起后接着算；进程被杀的话，下次启动时重新核对
        let endActivity = beginBackgroundActivity("verify-\(model.id)")
        let token = UUID()
        verifying[model.id] = token
        Task { [weak self] in
            // 不论结果（装好、对不上、被取消）都交还后台时间
            defer { endActivity() }
            if let expected {
                let actual = await FileDigest.sha256Hex(of: file)
                // 核对期间被取消或删除：文件已经由 cancel 删掉，这里不再碰这个路径——那里可能已经是重新下载、
                // 正在核对的新文件。管理器不在了（只有测试里会这样）就把文件留给下次启动时的检查
                guard let self, self.verifying[model.id] == token else { return }
                guard actual == expected else {
                    self.verifying[model.id] = nil
                    try? self.fileManager.removeItem(at: file)
                    self.states[model.id] = .failed(.checksumMismatch)
                    return
                }
            }
            guard let self, self.verifying[model.id] == token else { return }
            if model.isXZCompressed {
                await self.expandAndInstall(model, archive: file, in: directory, token: token)
            } else {
                self.verifying[model.id] = nil
                self.install(model, staged: file, in: directory)
            }
        }
    }

    /// 解压核对过的 xz 压缩包（词库），核对解压后的大小和 SHA-256，再改名成正式文件名
    private func expandAndInstall(_ model: Item, archive: URL, in directory: URL, token: UUID) async {
        prepareModelsDirectory()
        let part = directory.appendingPathComponent("\(model.fileName).\(token.uuidString).part")
        let result = await FileDigest.expandXZ(from: archive, to: part)
        guard verifying[model.id] == token else {
            // 解压期间被取消或删除：压缩包已经由 cancel 删掉，解出来的半成品由这里删
            try? fileManager.removeItem(at: part)
            return
        }
        verifying[model.id] = nil
        try? fileManager.removeItem(at: archive)
        let failure: DownloadFailure?
        switch result {
        case .success(let output) where output.bytes != model.installedBytes:
            failure = .sizeMismatch(expected: model.installedBytes, actual: output.bytes)
        case .success(let output):
            let expected = verifiesChecksums ? model.installedSHA256?.lowercased() : nil
            failure = expected.map { $0 == output.sha256 ? nil : .checksumMismatch } ?? nil
        case .failure(let error):
            failure = .storage("解压失败（\(error.message)）")
        }
        if let failure {
            try? fileManager.removeItem(at: part)
            states[model.id] = .failed(failure)
            return
        }
        let final = directory.appendingPathComponent(model.fileName)
        do {
            try? fileManager.removeItem(at: final)
            try fileManager.moveItem(at: part, to: final)
        } catch {
            try? fileManager.removeItem(at: part)
            states[model.id] = .failed(.storage(error.localizedDescription))
            return
        }
        states[model.id] = .downloaded
        onCompleted?(model)
    }




    /// 核对通过的文件挪进 Models 目录：先用 .part 名字，改名成正式文件名才算数
    private func install(_ model: Item, staged: URL, in directory: URL) {
        let part = directory.appendingPathComponent(model.fileName + ".part")
        let final = directory.appendingPathComponent(model.fileName)
        do {
            prepareModelsDirectory()
            try? fileManager.removeItem(at: part)
            try fileManager.moveItem(at: staged, to: part)
            try? fileManager.removeItem(at: final)
            try fileManager.moveItem(at: part, to: final)
        } catch {
            try? fileManager.removeItem(at: part)
            try? fileManager.removeItem(at: staged)
            states[model.id] = .failed(.storage(error.localizedDescription))
            return
        }
        states[model.id] = .downloaded
        onCompleted?(model)
    }

    // MARK: - 磁盘

    private func model(_ id: String) -> Item? {
        catalog.first { $0.id == id }
    }

    private func knownReceivedBytes(of model: Item) -> Int64 {
        if let data = resumeData[model.id] ?? readResumeFile(model.id) {
            return ResumeInfo.bytesReceived(in: data) ?? 0
        }
        return 0
    }

    private func fileIsComplete(_ model: Item) -> Bool {
        guard let directory = modelsDirectory else { return false }
        let url = directory.appendingPathComponent(model.fileName)
        guard let size = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize else { return false }
        return Int64(size) == model.installedBytes
    }

    /// 建 Models 目录，并排除在 iCloud / 电脑备份之外（模型能重新下载，不值得占备份空间）
    private func prepareModelsDirectory() {
        guard let directory = modelsDirectory else { return }
        createExcludedFromBackup(directory)
    }

    /// 暂存目录同样排除备份：下完、没核对完的模型文件也有四五百 MB
    private func prepareStagingDirectory() {
        createExcludedFromBackup(stagingDirectory)
    }

    private func createExcludedFromBackup(_ url: URL) {
        var directory = url
        try? fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? directory.setResourceValues(values)
    }

    /// 上次进程中途退出可能留下 .part，键盘不会读它，但留着占空间
    private func removeStalePartialFiles() {
        guard let directory = modelsDirectory,
              let names = try? fileManager.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasSuffix(".part") {
            try? fileManager.removeItem(at: directory.appendingPathComponent(name))
        }
    }

    /// 暂存目录里不属于目录中任何一项的文件（比如 App 升级换了词库版本，旧版本留下的续传数据、下完的文件），
    /// 不会再有人认领，删掉。每个管理器有自己的暂存目录，碰不到别的管理器的文件
    private func removeUnknownStagedFiles() {
        guard let names = try? fileManager.contentsOfDirectory(atPath: stagingDirectory.path) else { return }
        let ids = Set(catalog.map(\.id))
        for name in names {
            let id = [".download", ".verify", ".resume"].first(where: name.hasSuffix)
                .map { String(name.dropLast($0.count)) }
            if let id, ids.contains(id) { continue }
            try? fileManager.removeItem(at: stagingDirectory.appendingPathComponent(name))
        }
    }

    private func scanDisk() {
        for model in catalog {
            if fileIsComplete(model) {
                states[model.id] = .downloaded
            } else if let data = readResumeFile(model.id) {
                states[model.id] = .paused(received: ResumeInfo.bytesReceived(in: data) ?? 0, total: model.bytes)
            }
        }
    }

    /// 启动时收拾暂存目录里下完、还没装进 Models 的文件：上次核对到一半进程被杀（<id>.verify），
    /// 或者完成事件还没交给管理器 App 就退出了（<id>.download）。模型已经装好的直接删掉；
    /// 否则当成刚下完的文件重新走一遍 finalize（核对大小和 SHA-256，通过就装上，不通过就删掉）。
    /// 两个都在时用 <id>.download（更新的那次下载），finalize 会把它改名、盖掉旧的 <id>.verify
    private func recoverStagedFiles() {
        for model in catalog {
            let download = stagedFileURL(model.id)
            let verify = verifyFileURL(model.id)
            let hasDownload = fileManager.fileExists(atPath: download.path)
            guard hasDownload || fileManager.fileExists(atPath: verify.path) else { continue }
            if state(of: model) == .downloaded {
                removeStagedFile(model.id)
                continue
            }
            // 和收到完成事件时一样：文件已经下完，续传数据没用了
            removeResumeFile(model.id)
            finalize(model, staged: hasDownload ? download : verify)
        }
    }

    private func resumeFileURL(_ id: String) -> URL { stagingDirectory.appendingPathComponent("\(id).resume") }
    /// 传输层放下的完整文件
    private func stagedFileURL(_ id: String) -> URL { stagingDirectory.appendingPathComponent("\(id).download") }
    /// 正在核对 SHA-256 的文件（从 <id>.download 改名而来）
    private func verifyFileURL(_ id: String) -> URL { stagingDirectory.appendingPathComponent("\(id).verify") }

    private func readResumeFile(_ id: String) -> Data? { try? Data(contentsOf: resumeFileURL(id)) }
    private func removeResumeFile(_ id: String) { try? fileManager.removeItem(at: resumeFileURL(id)) }
    /// 删掉下完、还没装进 Models 的文件（<id>.download 和 <id>.verify）
    private func removeStagedFile(_ id: String) {
        try? fileManager.removeItem(at: stagedFileURL(id))
        try? fileManager.removeItem(at: verifyFileURL(id))
    }

    private func storeResumeData(_ data: Data, for id: String) {
        resumeData[id] = data
        try? data.write(to: resumeFileURL(id), options: .atomic)
    }
}


/// 下载文件的后台处理：算 SHA-256、解压 xz。放在泛型的 DownloadManager 外面，
/// 免得从后台线程调用时带上 Item 的协议一致性
enum FileDigest {
    /// 在后台线程上流式计算文件的 SHA-256（十六进制小写）；读不了文件时返回 nil
    static func sha256Hex(of url: URL) async -> String? {
        await Task.detached(priority: .utility) {
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
        }.value
    }

    /// 解压的结果：解出来的字节数和 SHA-256（边解边算，不用再读一遍）
    struct ExpandedFile: Sendable {
        let bytes: Int64
        let sha256: String
    }

    struct ExpansionFailure: Error, Sendable {
        let message: String
    }

    /// 在后台线程上把 xz（LZMA）压缩包流式解压到 destination。压缩包由 scripts/package-lexicon.sh 用系统的
    /// compression_tool 生成，系统的 Compression 框架能解。坏的、被截断的压缩包会报错或解出来大小不对
    static func expandXZ(from source: URL, to destination: URL) async -> Result<ExpandedFile, ExpansionFailure> {
        await Task.detached(priority: .utility) {
            let fileManager = FileManager.default
            try? fileManager.removeItem(at: destination)
            guard fileManager.createFile(atPath: destination.path, contents: nil) else {
                return .failure(ExpansionFailure(message: "建不了文件"))
            }
            do {
                let output = try FileHandle(forWritingTo: destination)
                defer { try? output.close() }
                let input = try FileHandle(forReadingFrom: source)
                defer { try? input.close() }
                var hasher = SHA256()
                var bytes: Int64 = 0
                let filter = try OutputFilter(.decompress, using: .lzma, bufferCapacity: 1 << 20) { chunk in
                    guard let chunk, !chunk.isEmpty else { return }
                    try output.write(contentsOf: chunk)
                    hasher.update(data: chunk)
                    bytes += Int64(chunk.count)
                }
                while true {
                    let chunk = try autoreleasepool { try input.read(upToCount: 1 << 20) }
                    guard let chunk, !chunk.isEmpty else { break }
                    try filter.write(chunk)
                }
                try filter.finalize()
                let hex = hasher.finalize().map { String(format: "%02x", $0) }.joined()
                return .success(ExpandedFile(bytes: bytes, sha256: hex))
            } catch {
                return .failure(ExpansionFailure(message: error.localizedDescription))
            }
        }.value
    }
}
