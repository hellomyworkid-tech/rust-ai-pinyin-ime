import Foundation
import Combine
import UIKit

/// 把各个组件接在一起：设置、下载（模型、词库）、Apple 模型状态、键盘安装状态、网络。
@MainActor
final class AppModel: ObservableObject {
    let settings: SettingsStore
    let downloads: ModelDownloadManager
    let lexicon: LexiconDownloadManager
    let apple: AppleModelMonitor
    let keyboard: KeyboardInstallMonitor
    let network: NetworkMonitor
    /// 不自动下载完整词库（调试构建的键盘包里自带完整词库时，见 live()）
    private let skipsLexiconAutoDownload: Bool

    init(settings: SettingsStore, downloads: ModelDownloadManager, lexicon: LexiconDownloadManager,
         apple: AppleModelMonitor, keyboard: KeyboardInstallMonitor, network: NetworkMonitor,
         skipsLexiconAutoDownload: Bool = false) {
        self.settings = settings
        self.downloads = downloads
        self.lexicon = lexicon
        self.apple = apple
        self.keyboard = keyboard
        self.network = network
        self.skipsLexiconAutoDownload = skipsLexiconAutoDownload

        // 新版本装好之后，旧版本的词库文件就不要了
        lexicon.onCompleted = { release in LexiconCatalog.removeOtherVersions(keeping: release) }

        downloads.onCompleted = { [weak self] model in
            guard let self else { return }
            self.settings.update {
                $0.localModelID = ModelSelectionPolicy.afterDownload(current: $0.localModelID, completed: model.id)
            }
        }
        downloads.onRemoved = { [weak self] model in
            guard let self else { return }
            let remaining = self.downloads.downloadedIDs
            self.settings.update {
                $0.localModelID = ModelSelectionPolicy.afterRemoval(
                    current: $0.localModelID, removed: model.id, downloaded: remaining)
            }
        }
    }

    /// 完整词库没装好、也没在下，就开始下载。App 每次打开、回到前台时调用：
    /// 第一次打开时自动下载；上次失败了（比如没网）就再试一次。压缩包约 12 MB，蜂窝网络下也不再问。
    /// 要等传输层查完上次留在后台的任务：还在下的这时才报出来，提前开始会重复下载
    func ensureLexicon() {
        guard !skipsLexiconAutoDownload else { return }
        lexicon.afterReconnect { [weak self] in self?.startLexiconIfNeeded() }
    }

    private func startLexiconIfNeeded() {
        guard let release = lexicon.catalog.first else { return }
        switch lexicon.state(of: release) {
        // .paused：词库没有暂停按钮，这只会是上次下载失败（留下了续传数据）之后 App 被关掉，
        // 重新打开时按续传数据读成了暂停。照样接着下
        case .notDownloaded, .failed, .paused:
            lexicon.start(release)
        default:
            break
        }
    }

    static func live() -> AppModel {
        #if DEBUG
        let arguments = ProcessInfo.processInfo.arguments
        let env = ProcessInfo.processInfo.environment
        let network = NetworkMonitor(forceExpensive: env["SIM_EXPENSIVE"] == "1")
        let freeSpace: () -> Int64 = {
            Int64(env["SIM_FREE_BYTES"] ?? "") ?? StorageCheck.availableBytes()
        }
        let downloads = arguments.contains("-simulateDownloads")
            ? simulatedDownloads(network: network, freeSpace: freeSpace)
            : realDownloads(network: network, freeSpace: freeSpace)
        // 调试：-lexiconURL <地址> 从别处（比如本机）下载同一个压缩包；
        // 键盘包里自带完整词库时不自动下载，除非加 -forceLexiconDownload，或者以前下载过（有旧版本就照样更新）
        var release = LexiconCatalog.current
        if let index = arguments.firstIndex(of: "-lexiconURL"), index + 1 < arguments.count,
           let url = URL(string: arguments[index + 1]) {
            release = release.with(downloadURL: url)
        }
        let skipsLexicon = keyboardBundlesFullLexicon && !arguments.contains("-forceLexiconDownload")
            && LexiconCatalog.installedLexiconURL() == nil
        #else
        let network = NetworkMonitor()
        let freeSpace: () -> Int64 = { StorageCheck.availableBytes() }
        let downloads = realDownloads(network: network, freeSpace: freeSpace)
        let release = LexiconCatalog.current
        let skipsLexicon = false
        #endif
        let model = AppModel(settings: SettingsStore(), downloads: downloads,
                             lexicon: lexiconDownloads(release, network: network, freeSpace: freeSpace),
                             apple: AppleModelMonitor(), keyboard: KeyboardInstallMonitor(), network: network,
                             skipsLexiconAutoDownload: skipsLexicon)
        // 自动下载词库由 AIPinyinApp 的 .task 和回到前台时触发（ensureLexicon 会等传输层查完后台任务）
        return model
    }

    /// 调试构建的键盘包里有完整词库（scripts/build-rust.sh 拷进去的；Release 构建不打）
    static var keyboardBundlesFullLexicon: Bool {
        guard let plugins = Bundle.main.builtInPlugInsURL else { return false }
        let url = plugins.appendingPathComponent("AIPinyinKeyboard.appex/merged_lexicon.bin")
        return FileManager.default.fileExists(atPath: url.path)
    }

    /// 正式的下载：后台 URLSession，模型装进 App Group 里的 Models 目录，核对 SHA-256，
    /// 核对期间向系统要后台运行时间
    private static func realDownloads(network: NetworkMonitor,
                                      freeSpace: @escaping () -> Int64) -> ModelDownloadManager {
        ModelDownloadManager(catalog: ModelCatalog.all,
                             transport: BackgroundDownloadTransport.shared,
                             directory: AppGroup.modelsDirectory,
                             stagingDirectory: BackgroundDownloadTransport.stagingDirectory,
                             network: network, freeSpace: freeSpace,
                             beginBackgroundActivity: BackgroundActivity.begin)
    }

    /// 完整词库：另一条后台会话，下到 App Group 的 Lexicon 目录，核对压缩包和解压后的 SHA-256
    private static func lexiconDownloads(_ release: LexiconRelease, network: NetworkMonitor,
                                         freeSpace: @escaping () -> Int64) -> LexiconDownloadManager {
        LexiconDownloadManager(catalog: [release],
                               transport: BackgroundDownloadTransport.lexicon,
                               directory: AppGroup.lexiconDirectory,
                               stagingDirectory: BackgroundDownloadTransport.lexiconStagingDirectory,
                               network: network, freeSpace: freeSpace,
                               beginBackgroundActivity: BackgroundActivity.begin)
    }

    #if DEBUG
    /// 调试用的假下载（启动参数 -simulateDownloads）。假通道写的是全零的稀疏文件，哈希对不上，只核对大小。
    /// Models 和暂存目录都换成 App 自己临时目录里的一份：假文件进了 App Group，键盘、DeviceEval、
    /// 以后的正常启动都只看大小，会把它当成真模型；进了真正的暂存目录，正常启动时的检查会把它当成没核对完的下载
    private static func simulatedDownloads(network: NetworkMonitor,
                                           freeSpace: @escaping () -> Int64) -> ModelDownloadManager {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("SimulatedDownloads", isDirectory: true)
        let staging = root.appendingPathComponent("Staging", isDirectory: true)
        return ModelDownloadManager(catalog: ModelCatalog.all,
                                    transport: SimulatedTransport(stagingDirectory: staging),
                                    directory: root.appendingPathComponent("Models", isDirectory: true),
                                    stagingDirectory: staging,
                                    network: network, freeSpace: freeSpace,
                                    verifiesChecksums: false)
    }
    #endif
}

/// 一段后台运行时间（UIApplication.beginBackgroundTask）。下完的模型核对 SHA-256 时用：
/// App 被后台下载唤醒后，系统处理完下载事件就会把它挂起，核对要四五百 MB 的读盘，等不到做完。
@MainActor
private final class BackgroundActivity {
    private var identifier = UIBackgroundTaskIdentifier.invalid

    /// 开始一段后台时间，返回结束它的闭包。闭包调多次也只结束一次；时间用完时系统也会调它
    static func begin(_ name: String) -> @MainActor () -> Void {
        let activity = BackgroundActivity()
        activity.identifier = UIApplication.shared.beginBackgroundTask(withName: name) { activity.end() }
        return { activity.end() }
    }

    private func end() {
        guard identifier != .invalid else { return }
        UIApplication.shared.endBackgroundTask(identifier)
        identifier = .invalid
    }
}
