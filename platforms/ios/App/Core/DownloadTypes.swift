import Foundation

/// 字节数的显示（统一用十进制单位，和系统「存储空间」里的写法一致）
enum ByteFormat {
    static func string(_ bytes: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }

    /// 精确字节数，如「396,705,472 字节」
    static func exact(_ bytes: Int64) -> String {
        bytes.formatted(.number.grouping(.automatic)) + " 字节"
    }
}

/// 下载管理器（DownloadManager）能处理的文件：本地模型、完整词库
protocol DownloadableFile: Identifiable, Equatable where ID == String {
    var downloadURL: URL { get }
    /// 下载的字节数（压缩包的大小）
    var bytes: Int64 { get }
    /// 下载文件的 SHA-256（十六进制小写）；nil 表示不核对
    var sha256: String? { get }
    /// 装好后的文件名（键盘读的就是它）
    var fileName: String { get }
    /// 装好后的字节数（不压缩时等于 bytes）
    var installedBytes: Int64 { get }
    /// 解压后的 SHA-256；不压缩或不核对时为 nil
    var installedSHA256: String? { get }
    /// 下载的是 xz 压缩包：核对后先解压再装
    var isXZCompressed: Bool { get }
}

extension DownloadableFile {
    /// 解压要另外占的空间（下载的压缩包和解出来的文件会同时在盘上）
    var expansionBytes: Int64 { isXZCompressed ? installedBytes : 0 }
}

extension LocalModel: DownloadableFile {
    var installedBytes: Int64 { bytes }
    var installedSHA256: String? { nil }
    var isXZCompressed: Bool { false }
}

extension LexiconRelease: DownloadableFile {
    var isXZCompressed: Bool { true }
}

/// 一个模型（或词库）当前的下载状态
enum ModelDownloadState: Equatable {
    case notDownloaded
    case downloading(received: Int64, total: Int64)
    /// 暂停。received 为 0 表示不知道已下了多少
    case paused(received: Int64, total: Int64)
    /// 下完了，正在核对大小和 SHA-256、移到键盘能读到的位置
    /// （也可能是启动时接手的、上次没核对完的文件）
    case finishing
    case downloaded
    case failed(DownloadFailure)

    var isActive: Bool {
        switch self {
        case .downloading, .paused, .finishing: return true
        default: return false
        }
    }
}

enum DownloadFailure: Equatable {
    case network(String)
    case http(Int)
    case sizeMismatch(expected: Int64, actual: Int64)
    /// 大小对，但内容的 SHA-256 与目录里写的不同（传输中损坏，或服务器上的文件换了）
    case checksumMismatch
    case insufficientSpace(needed: Int64, available: Int64)
    case storage(String)

    var message: String {
        switch self {
        case .network(let text):
            return text
        case .http(let code):
            return "服务器返回了错误（\(code)），请稍后再试"
        case .sizeMismatch(let expected, let actual):
            // 差得很少时换算成 MB 会显示成一样的数，所以直接写字节数
            return "下载的文件大小不对（应为 \(ByteFormat.exact(expected))，实际 \(ByteFormat.exact(actual))），已丢弃，请重新下载"
        case .checksumMismatch:
            return "下载的文件没有通过校验（内容和官方发布的不一致），已丢弃，请重新下载"
        case .insufficientSpace(let needed, let available):
            return "存储空间不足：需要约 \(ByteFormat.string(needed))，目前只剩 \(ByteFormat.string(available))"
        case .storage(let text):
            return "保存文件失败：\(text)"
        }
    }
}

/// 传输层上报的事件。回调一律在主线程。
enum DownloadEvent {
    /// 下载进度。total 为 -1 表示服务器没给长度
    case progress(id: String, received: Int64, total: Int64)
    /// 重新连上了还在后台跑的任务（App 被系统杀掉又重新打开之后）
    case active(id: String, received: Int64, total: Int64)
    /// 下载完成。file 已经被传输层放到 App 自己的暂存目录，由管理器校验后挪走
    case finished(id: String, file: URL)
    case failed(id: String, failure: DownloadFailure, resumeData: Data?)
    /// 暂停完成。resumeData 为 nil 表示服务器不支持续传，继续时要从头下
    case paused(id: String, resumeData: Data?)
    /// reconnect() 查完了：还在后台跑的任务都已经报过 .active（排在它们后面送到）
    case reconnected
}

extension DownloadEvent {
    /// 事件属于哪一项下载；.reconnected 不属于任何一项
    var downloadID: String? {
        switch self {
        case .progress(let id, _, _), .active(let id, _, _), .finished(let id, _),
             .failed(let id, _, _), .paused(let id, _):
            return id
        case .reconnected:
            return nil
        }
    }
}

/// 真正干活的下载通道。生产环境是后台 URLSession，测试里换成假的。
protocol DownloadTransport: AnyObject {
    var onEvent: ((DownloadEvent) -> Void)? { get set }
    func start(id: String, url: URL, resumeData: Data?)
    /// 暂停；完成后发 .paused
    func pause(id: String)
    /// 取消，不发事件
    func cancel(id: String)
    /// 查询还在后台跑的任务，对每个发 .active，查完发 .reconnected
    func reconnect()
}

/// 开始下载前的检查结果
enum DownloadPreflight: Equatable {
    case ok
    case insufficientSpace(needed: Int64, available: Int64)
    /// 当前是蜂窝网络或个人热点，要先让用户确认
    case cellular
}

/// 当前网络是否按流量计费（蜂窝网络、个人热点）
@MainActor
protocol NetworkConditions: AnyObject {
    var isExpensive: Bool { get }
}

/// 下载续传数据（URLSession 给的一段不透明数据）里能读出的信息
enum ResumeInfo {
    /// 已经下了多少字节；读不出来返回 nil
    static func bytesReceived(in data: Data) -> Int64? {
        guard let object = try? PropertyListSerialization.propertyList(from: data, options: [], format: nil),
              let dict = object as? [String: Any],
              let value = dict["NSURLSessionResumeBytesReceived"] as? NSNumber else { return nil }
        return value.int64Value
    }
}

/// 开始下载前的空间检查
enum StorageCheck {
    /// 除了文件本身，再留一点余量，免得把手机塞满
    static let safetyMargin: Int64 = 100 * 1000 * 1000

    static func requiredBytes(remaining: Int64) -> Int64 {
        max(0, remaining) + safetyMargin
    }

    static func hasEnoughSpace(remaining: Int64, available: Int64) -> Bool {
        available >= requiredBytes(remaining: remaining)
    }

    /// 指定位置所在磁盘的可用字节数（「重要用途」口径，系统会把可清理的缓存也算进来）
    static func availableBytes(at url: URL = URL(fileURLWithPath: NSHomeDirectory())) -> Int64 {
        if let values = try? url.resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey]),
           let bytes = values.volumeAvailableCapacityForImportantUsage {
            return bytes
        }
        if let attributes = try? FileManager.default.attributesOfFileSystem(forPath: url.path),
           let free = attributes[.systemFreeSize] as? NSNumber {
            return free.int64Value
        }
        return 0
    }
}

/// 选中模型的规则：下完第一个自动选上；删掉选中的就换一个已下载的，没有就清空
enum ModelSelectionPolicy {
    static func afterDownload(current: String?, completed: String) -> String? {
        current ?? completed
    }

    static func afterRemoval(current: String?, removed: String, downloaded: [String]) -> String? {
        guard current == removed else { return current }
        return downloaded.first { $0 != removed }
    }
}
