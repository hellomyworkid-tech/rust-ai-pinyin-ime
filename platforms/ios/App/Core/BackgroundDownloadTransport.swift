import Foundation

/// 用 URLSession 后台会话下载：App 退到后台或被系统杀掉，下载照样继续，
/// 下完后系统再把 App 唤起来交付结果。
///
/// 任务用 taskDescription 记模型 id。下好的文件在 didFinishDownloadingTo 返回前就会被系统删掉，
/// 所以当场挪到暂存目录（同一个磁盘，是改名，不是复制）。
final class BackgroundDownloadTransport: NSObject, DownloadTransport, URLSessionDownloadDelegate, @unchecked Sendable {
    static let sessionIdentifier = "com.zengji.aipinyin.model-download"
    /// 词库用另一条后台会话：两个下载管理器各接一个传输层的事件
    static let lexiconSessionIdentifier = "com.zengji.aipinyin.lexicon-download"

    /// 下模型用的实例。暂存目录在 App 自己的容器里，键盘看不到。
    static let shared = make(identifier: sessionIdentifier, stagingDirectory: stagingDirectory)
    /// 下词库用的实例
    static let lexicon = make(identifier: lexiconSessionIdentifier, stagingDirectory: lexiconStagingDirectory)

    static var stagingDirectory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("ModelDownloads", isDirectory: true)
    }

    static var lexiconStagingDirectory: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("LexiconDownloads", isDirectory: true)
    }

    /// 按会话标识找实例（AppDelegate 收到系统唤起时用）
    static func instance(for identifier: String) -> BackgroundDownloadTransport? {
        switch identifier {
        case sessionIdentifier: return shared
        case lexiconSessionIdentifier: return lexicon
        default: return nil
        }
    }

    private static func make(identifier: String, stagingDirectory: URL) -> BackgroundDownloadTransport {
        let config = URLSessionConfiguration.background(withIdentifier: identifier)
        config.sessionSendsLaunchEvents = true
        config.isDiscretionary = false
        return BackgroundDownloadTransport(configuration: config, stagingDirectory: stagingDirectory)
    }

    var onEvent: ((DownloadEvent) -> Void)? {
        didSet { flushPending() }
    }

    private enum Intent { case pause, cancel }

    private let configuration: URLSessionConfiguration
    private let stagingDirectory: URL
    private let queue: OperationQueue
    private lazy var session = URLSession(configuration: configuration, delegate: self, delegateQueue: queue)

    private let lock = NSLock()
    private var tasks: [String: URLSessionDownloadTask] = [:]
    /// 每个 id 最近一次开始（或重新接上）的任务编号。任务结束后也留着：更早的任务迟到的回调据此认出来、丢掉
    private var latestTaskIDs: [String: Int] = [:]
    private var intents: [String: Intent] = [:]
    private var lastProgress: [String: Date] = [:]
    private var pendingEvents: [DownloadEvent] = []
    private var backgroundCompletion: (() -> Void)?

    init(configuration: URLSessionConfiguration, stagingDirectory: URL) {
        self.configuration = configuration
        self.stagingDirectory = stagingDirectory
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        queue.name = "com.zengji.aipinyin.download-delegate"
        self.queue = queue
        super.init()
        try? FileManager.default.createDirectory(at: stagingDirectory, withIntermediateDirectories: true)
    }

    // MARK: - DownloadTransport

    func start(id: String, url: URL, resumeData: Data?) {
        let task = resumeData.map { session.downloadTask(withResumeData: $0) }
            ?? session.downloadTask(with: url)
        task.taskDescription = id
        locked {
            tasks[id] = task
            latestTaskIDs[id] = task.taskIdentifier
            intents[id] = nil
            lastProgress[id] = nil
        }
        task.resume()
    }

    func pause(id: String) {
        guard let task = locked({ tasks[id] }) else {
            emit(.paused(id: id, resumeData: nil))
            return
        }
        locked { intents[id] = .pause }
        task.cancel(byProducingResumeData: { [weak self] data in
            self?.emit(.paused(id: id, resumeData: data), from: task)
        })
    }

    func cancel(id: String) {
        locked { intents[id] = .cancel }
        locked({ tasks[id] })?.cancel()
        locked { tasks[id] = nil }
        try? FileManager.default.removeItem(at: stagedURL(id))
    }

    func reconnect() {
        session.getAllTasks { [weak self] all in
            guard let self else { return }
            for case let task as URLSessionDownloadTask in all {
                guard let id = task.taskDescription, task.state == .running || task.state == .suspended else { continue }
                self.locked {
                    self.tasks[id] = task
                    self.latestTaskIDs[id] = task.taskIdentifier
                }
                if task.state == .suspended { task.resume() }
                self.emit(.active(id: id, received: task.countOfBytesReceived,
                                  total: task.countOfBytesExpectedToReceive))
            }
            // 和 .active 走同一条主线程队列，排在它们后面
            self.emit(.reconnected)
        }
    }

    /// AppDelegate 收到系统唤起时调用：事件处理完再通知系统
    func setBackgroundCompletionHandler(_ handler: @escaping () -> Void) {
        locked { backgroundCompletion = handler }
        _ = session // 重新建立会话，系统才会把积压的事件发过来
    }

    // MARK: - URLSessionDownloadDelegate

    func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask,
                    didWriteData bytesWritten: Int64, totalBytesWritten: Int64,
                    totalBytesExpectedToWrite: Int64) {
        guard let id = downloadTask.taskDescription, !isSuperseded(downloadTask, id: id) else { return }
        // 进度回调很密，界面不需要这么多，每 0.25 秒上报一次
        let now = Date()
        let skip: Bool = locked {
            if let last = lastProgress[id], now.timeIntervalSince(last) < 0.25 { return true }
            lastProgress[id] = now
            return false
        }
        if skip { return }
        emit(.progress(id: id, received: totalBytesWritten, total: totalBytesExpectedToWrite), from: downloadTask)
    }

    func urlSession(_ session: URLSession, downloadTask: URLSessionDownloadTask, didFinishDownloadingTo location: URL) {
        guard let id = downloadTask.taskDescription else { return }
        // 同一个 id 已经换了新任务：旧任务迟到的结果不要（新任务会自己下完）。判断和改记录在同一把锁里，
        // 免得这中间主线程刚开始的新任务被当成这个任务清掉
        let superseded: Bool = locked {
            if let latest = latestTaskIDs[id], latest != downloadTask.taskIdentifier { return true }
            if tasks[id] === downloadTask { tasks[id] = nil }
            return false
        }
        if superseded { return }
        if let http = downloadTask.response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            emit(.failed(id: id, failure: .http(http.statusCode), resumeData: nil), from: downloadTask)
            return
        }
        let target = stagedURL(id)
        do {
            try? FileManager.default.removeItem(at: target)
            try FileManager.default.moveItem(at: location, to: target)
            emit(.finished(id: id, file: target), from: downloadTask)
        } catch {
            emit(.failed(id: id, failure: .storage(error.localizedDescription), resumeData: nil), from: downloadTask)
        }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let error, let id = task.taskDescription else { return }
        // 三件事在同一把锁里做完，免得中间主线程开始了新任务（start 会清掉暂停 / 取消记录）：
        // 1. 同一个 id 已经换了新任务：旧任务的结果不算数，也不能动新任务的暂停 / 取消记录；
        // 2. 我们自己暂停 / 取消的不当失败（暂停的结果走 pause 的回调）；
        // 3. 其余的（包括系统取消的：关掉了「后台 App 刷新」、系统资源不够）报失败，不然状态一直停在下载中
        let reportsFailure: Bool = locked {
            if let latest = latestTaskIDs[id], latest != task.taskIdentifier { return false }
            let intent = intents[id]
            intents[id] = nil
            if intent != nil { return false }
            if tasks[id] === task { tasks[id] = nil }
            return true
        }
        guard reportsFailure else { return }
        let nsError = error as NSError
        let data = nsError.userInfo[NSURLSessionDownloadTaskResumeData] as? Data
        emit(.failed(id: id, failure: .network(Self.describe(nsError)), resumeData: data), from: task)
    }

    func urlSessionDidFinishEvents(forBackgroundURLSession session: URLSession) {
        let handler: (() -> Void)? = locked {
            let value = backgroundCompletion
            backgroundCompletion = nil
            return value
        }
        DispatchQueue.main.async { handler?() }
    }

    // MARK: - 内部

    static func describe(_ error: NSError) -> String {
        guard error.domain == NSURLErrorDomain else { return "下载出错了（\(error.localizedDescription)）" }
        switch error.code {
        case NSURLErrorNotConnectedToInternet: return "没有网络连接"
        case NSURLErrorTimedOut: return "连接超时"
        case NSURLErrorNetworkConnectionLost: return "网络连接中断"
        case NSURLErrorCannotFindHost, NSURLErrorCannotConnectToHost, NSURLErrorDNSLookupFailed:
            return "连不上下载服务器"
        case NSURLErrorInternationalRoamingOff, NSURLErrorDataNotAllowed:
            return "当前网络不允许下载（蜂窝数据或漫游被关闭）"
        case NSURLErrorCancelled:
            return "下载被系统中止了（比如关掉了「后台 App 刷新」），请重试"
        default: return "下载出错了（错误码 \(error.code)）"
        }
    }

    /// 同一个 id 后来又开始了别的任务（重新下载、继续下载）：这个任务的回调迟到了，不再算数。
    /// App 重新启动后系统补发的、本次运行里还没见过的任务的回调照常处理
    private func isSuperseded(_ task: URLSessionTask, id: String) -> Bool {
        isSuperseded(taskID: task.taskIdentifier, id: id)
    }

    private func isSuperseded(taskID: Int, id: String) -> Bool {
        locked {
            guard let latest = latestTaskIDs[id] else { return false }
            return latest != taskID
        }
    }

    private func stagedURL(_ id: String) -> URL {
        stagingDirectory.appendingPathComponent("\(id).download")
    }

    /// 事件一律回到主线程；管理器还没接上时先存着。
    /// from：发出事件的任务。送到主线程时再看一次它是不是还是这个 id 最新的任务：这期间主线程上可能已经
    /// 开始了新任务（start 也在主线程上调用），比如 App 重新打开后系统补发的旧任务的失败，
    /// 不能把正在下的新任务标成失败
    private func emit(_ event: DownloadEvent, from task: URLSessionTask? = nil) {
        let taskID = task?.taskIdentifier
        DispatchQueue.main.async { [self] in
            if let taskID, let id = event.downloadID, isSuperseded(taskID: taskID, id: id) { return }
            if let onEvent {
                onEvent(event)
            } else {
                pendingEvents.append(event)
            }
        }
    }

    private func flushPending() {
        guard onEvent != nil else { return }
        DispatchQueue.main.async { [self] in
            let events = pendingEvents
            pendingEvents = []
            for event in events { onEvent?(event) }
        }
    }

    @discardableResult
    private func locked<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }
}
