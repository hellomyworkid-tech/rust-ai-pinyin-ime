#if DEBUG
import Foundation

/// 调试用的假下载通道：不联网，按时间推进度，下完在暂存目录里放一个稀疏文件。
/// 用启动参数 `-simulateDownloads` 打开，用来在模拟器里走一遍下载界面。
/// 这时 AppModel 把暂存目录和 Models 目录都换成 App 临时目录里的 SimulatedDownloads/，
/// 全零的假文件不会进 App Group（键盘看不到它），也不会混进真正的暂存目录。
/// 环境变量：
/// - SIM_DOWNLOAD_SECONDS：整个下载要多久（默认 20）
/// - SIM_DOWNLOAD_FAIL：这个模型 id 下到 40% 时网络失败
/// - SIM_DOWNLOAD_SHORT：这个模型 id 下完的文件少一个字节（模拟大小不符）
final class SimulatedTransport: DownloadTransport {
    var onEvent: ((DownloadEvent) -> Void)?

    private let stagingDirectory: URL
    private let seconds: Double
    private let failID: String?
    private let shortID: String?
    private var timers: [String: Timer] = [:]
    private var progress: [String: Double] = [:]
    private let catalog: [LocalModel]

    init(stagingDirectory: URL, catalog: [LocalModel] = ModelCatalog.all,
         environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.stagingDirectory = stagingDirectory
        self.catalog = catalog
        seconds = Double(environment["SIM_DOWNLOAD_SECONDS"] ?? "") ?? 20
        failID = environment["SIM_DOWNLOAD_FAIL"]
        shortID = environment["SIM_DOWNLOAD_SHORT"]
    }

    func start(id: String, url: URL, resumeData: Data?) {
        guard let model = catalog.first(where: { $0.id == id }) else { return }
        let offset = resumeData.flatMap(ResumeInfo.bytesReceived(in:)) ?? 0
        progress[id] = Double(offset) / Double(model.bytes)
        timers[id]?.invalidate()
        let step = 0.1 / max(0.5, seconds)
        timers[id] = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            guard let self else { return }
            let value = min(1, (self.progress[id] ?? 0) + step)
            self.progress[id] = value
            let received = Int64(Double(model.bytes) * value)
            if value >= 0.4, self.failID == id {
                self.timers[id]?.invalidate()
                self.timers[id] = nil
                self.progress[id] = nil
                self.onEvent?(.failed(id: id, failure: .network("网络连接中断"), resumeData: Self.resumeData(received)))
                return
            }
            if value >= 1 {
                self.timers[id]?.invalidate()
                self.timers[id] = nil
                self.progress[id] = nil
                self.onEvent?(.finished(id: id, file: self.makeFile(model, short: self.shortID == id)))
            } else {
                self.onEvent?(.progress(id: id, received: received, total: model.bytes))
            }
        }
    }

    func pause(id: String) {
        timers[id]?.invalidate()
        timers[id] = nil
        let received = Int64(Double(catalog.first { $0.id == id }?.bytes ?? 0) * (progress[id] ?? 0))
        onEvent?(.paused(id: id, resumeData: Self.resumeData(received)))
    }

    func cancel(id: String) {
        timers[id]?.invalidate()
        timers[id] = nil
        progress[id] = nil
    }

    /// 没有留在后台的任务，直接答复
    func reconnect() {
        DispatchQueue.main.async { [weak self] in self?.onEvent?(.reconnected) }
    }

    private static func resumeData(_ received: Int64) -> Data? {
        try? PropertyListSerialization.data(
            fromPropertyList: ["NSURLSessionResumeBytesReceived": received], format: .binary, options: 0)
    }

    private func makeFile(_ model: LocalModel, short: Bool) -> URL {
        let url = stagingDirectory.appendingPathComponent("\(model.id).download")
        try? FileManager.default.removeItem(at: url)
        FileManager.default.createFile(atPath: url.path, contents: nil)
        if let handle = try? FileHandle(forWritingTo: url) {
            try? handle.truncate(atOffset: UInt64(model.bytes - (short ? 1 : 0)))
            try? handle.close()
        }
        return url
    }
}
#endif
