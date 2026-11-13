import Foundation

/// App 与键盘扩展共享的容器。
///
/// 真机实测（iOS 27.2）：键盘在未开启「允许完全访问」时对 App Group **只读**——能读、能 mmap，不能写。
/// 所以这里的文件一律由 App 写、键盘读；键盘自己要写的东西（用户词频等）放在键盘自己的容器里。
enum AppGroup {
    static let identifier = "group.com.zengji.aipinyin"

    static var root: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: identifier)
    }

    /// App 写、键盘读的数据目录（设置、下载的模型）
    static var sharedDirectory: URL? {
        root?.appendingPathComponent("Library/Application Support/AIPinyin", isDirectory: true)
    }

    static var settingsURL: URL? { sharedDirectory?.appendingPathComponent("settings.json") }
    static var modelsDirectory: URL? { sharedDirectory?.appendingPathComponent("Models", isDirectory: true) }
    /// App 下载、解压好的完整词库（见 LexiconCatalog）
    static var lexiconDirectory: URL? { sharedDirectory?.appendingPathComponent("Lexicon", isDirectory: true) }
}
