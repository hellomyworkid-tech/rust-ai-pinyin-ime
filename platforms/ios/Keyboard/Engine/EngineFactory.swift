import Foundation
import os

/// 键盘用的是哪份词库
enum LexiconSource: Equatable {
    /// App 下载、解压好的完整词库（App Group 的 Lexicon 目录）
    case downloaded(URL)
    /// 调试构建打在键盘包里的完整词库
    case bundledFull(URL)
    /// 包里的基础词库：完整词库下完之前先能打中文
    case fallback(URL)

    var url: URL {
        switch self {
        case .downloaded(let url), .bundledFull(let url), .fallback(let url): return url
        }
    }

    var isComplete: Bool {
        if case .fallback = self { return false }
        return true
    }
}

/// 引擎只从这里创建。
enum EngineFactory {
    private static let log = Logger(subsystem: "com.zengji.aipinyin.keyboard", category: "engine")

    /// 键盘用哪份词库，按顺序：
    /// 1. App 下载、解压好的完整词库（见 LexiconCatalog）；
    /// 2. 调试构建打在键盘包里的完整词库 merged_lexicon.bin（scripts/build-rust.sh 生成）。
    ///    Release 构建不打它：词库来自雾凇拼音，GPL-3.0-only，不放进 App Store 的安装包；
    /// 3. 包里的基础词库 fallback_lexicon.bin（宽松协议的小词库）。
    /// 都没有时返回 nil，键盘只能打英文。键盘每次出现时都会再问一次：App 刚下完完整词库，就换新引擎
    static func lexiconSource(lexiconDirectory: URL? = AppGroup.lexiconDirectory,
                              bundle: Bundle = .main) -> LexiconSource? {
        if let url = LexiconCatalog.installedLexiconURL(in: lexiconDirectory) { return .downloaded(url) }
        if let url = bundle.url(forResource: "merged_lexicon", withExtension: "bin") { return .bundledFull(url) }
        if let url = bundle.url(forResource: "fallback_lexicon", withExtension: "bin") { return .fallback(url) }
        return nil
    }

    /// 个人词频存在键盘自己的容器里：没有「允许完全访问」时键盘写不了 App Group，自己的容器始终可写
    static var userDictionaryURL: URL? {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first?
            .appendingPathComponent("AIPinyin/user_dict.json")
    }

    static func makeEngine(source: LexiconSource?) -> IMEEngine {
        guard let source else {
            #if DEBUG
            // 调试构建里找不到词库（工程的构建检查本应拦住）：用假引擎，界面照样能调
            return MockEngine()
            #else
            // 还没下载完整词库，包里也没有基础词库
            log.error("没有可用的词库")
            return PassthroughEngine()
            #endif
        }
        do {
            return try RustEngine(lexiconURL: source.url, userDictionaryURL: userDictionaryURL)
        } catch {
            log.fault("引擎创建失败：\(error.localizedDescription, privacy: .public)")
            return PassthroughEngine()
        }
    }
}

/// 引擎起不来时的替身：什么都不处理，键盘退化成只能打英文字母和符号，但不会显示假候选。
final class PassthroughEngine: IMEEngine {
    var onAsyncUpdate: (() -> Void)?
    var onChoiceRequest: ((IMEChoiceRequest) -> Void)?
    var onChoiceCancel: ((UInt64) -> Void)?
    /// 没有引擎就没有中文，始终按英文处理
    let isEnglishMode = true
    let isComposing = false
    let isLocalModelLoaded = false

    func handle(_ key: IMEKey) -> IMEResult { key == .toggleChineseEnglish ? .update(.idle) : .passThrough }
    func selectCandidate(at globalIndex: Int) -> IMEResult { .passThrough }
    func candidates(from offset: Int, count: Int) -> [IMECandidate] { [] }
    func tapSyllable(at index: Int) -> IMEResult { .passThrough }
    func commitFullSentence() -> IMEResult { .passThrough }
    func currentView() -> IMEView { .idle }
    func refreshAfterAI() -> IMEView { .idle }
    func setContext(_ textBeforeCursor: String?) {}
    func reset() {}
    func deliverChoice(id: UInt64, text: String?) {}
    func apply(_ settings: IMESettings) {}
    func reloadUserDictionaryIfChanged() -> Bool { false }
    func loadLocalModel(at url: URL) throws { throw CocoaError(.featureUnsupported) }
    func unloadLocalModel() {}
}
