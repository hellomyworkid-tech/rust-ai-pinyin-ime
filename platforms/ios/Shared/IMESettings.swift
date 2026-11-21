import Foundation

/// 用户设置。App 负责写入 App Group 里的 settings.json，键盘每次出现时读取（只读）。
/// 不用 UserDefaults(suiteName:)：键盘没有完全访问时走 cfprefsd 不可靠，直接读文件已实测可行。
struct IMESettings: Codable, Equatable {
    /// AI 选词用哪条路线
    enum AIMode: String, Codable, CaseIterable {
        /// 只用规则，不调模型
        case off
        /// Apple 端侧模型（FoundationModels）：不占键盘内存、无需下载，但只在支持 Apple Intelligence 的设备上可用
        case apple
        /// 本地开源模型（llama.cpp 在键盘进程内打分）：需要先在 App 里下载模型
        case local
    }

    /// 模糊音开关，字段和默认值都与 Rust 侧 FuzzyPinyinConfig 一一对应
    /// （平翘舌、前后鼻音默认开，n/l、f/h 默认关——和 Windows 版一致，词库回归也是在这组默认值下做的）
    struct Fuzzy: Codable, Equatable {
        var zZh = true
        var cCh = true
        var sSh = true
        var nL = false
        var fH = false
        var inIng = true
        var enEng = true
        var anAng = true
        var typoCorrection = true

        init() {}

        /// 逐个字段解码：缺了的键或读不懂的值只让那一个字段回到默认值（以后加字段，旧文件照样读得进来）
        init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            let d = Fuzzy()
            zZh = c.value(.zZh) ?? d.zZh
            cCh = c.value(.cCh) ?? d.cCh
            sSh = c.value(.sSh) ?? d.sSh
            nL = c.value(.nL) ?? d.nL
            fH = c.value(.fH) ?? d.fH
            inIng = c.value(.inIng) ?? d.inIng
            enEng = c.value(.enEng) ?? d.enEng
            anAng = c.value(.anAng) ?? d.anAng
            typoCorrection = c.value(.typoCorrection) ?? d.typoCorrection
        }
    }

    var aiMode: AIMode = .apple
    /// 选用的本地模型（ModelCatalog 里的 id）；nil 表示还没选
    var localModelID: String?
    var fuzzy = Fuzzy()
    var keyClickSound = true
    /// 触感反馈需要「允许完全访问」，未开启时自动失效
    var haptics = true
    /// 常用词候选后面跟上对应的表情（打 mao，「猫」后面是 🐈 🐱），和系统键盘一样
    var emojiCandidates = true
    /// 折叠屏展开内屏（如 iPhone Duo）时自动采用左右两侧分离式键盘，方便双手拇指输入
    var splitUnfoldedKeyboard = true

    init() {}

    /// 逐个字段解码：缺了的键、读不懂的值（比如新版本才有的 aiMode）只让那一个字段回到默认值，
    /// 不会把整份设置连同用户选过的模型、模糊音一起丢掉（App 随后保存时就再也找不回来了）
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = IMESettings()
        aiMode = c.value(.aiMode) ?? d.aiMode
        localModelID = c.value(.localModelID)
        fuzzy = c.value(.fuzzy) ?? d.fuzzy
        keyClickSound = c.value(.keyClickSound) ?? d.keyClickSound
        haptics = c.value(.haptics) ?? d.haptics
        emojiCandidates = c.value(.emojiCandidates) ?? d.emojiCandidates
        splitUnfoldedKeyboard = c.value(.splitUnfoldedKeyboard) ?? d.splitUnfoldedKeyboard
    }

    static func load() -> IMESettings {
        guard let url = AppGroup.settingsURL else { return IMESettings() }
        return load(from: url)
    }

    /// 键盘读取设置：已开完全访问时与 App 共用 App Group 里的 settings.json；
    /// 未开完全访问且用户在键盘快捷面板里改过开关时，比较 App Group 与键盘容器内覆盖文件的修改时间，取更新的一份。
    static func loadForKeyboard(sharedURL: URL? = AppGroup.settingsURL, overrideURL: URL?) -> IMESettings {
        let fm = FileManager.default
        let hasShared = sharedURL.map { fm.fileExists(atPath: $0.path) } ?? false
        let hasOverride = overrideURL.map { fm.fileExists(atPath: $0.path) } ?? false

        if hasShared, hasOverride, let sURL = sharedURL, let oURL = overrideURL {
            // 直接走 FileManager.attributesOfItem（stat），避免 URL.resourceValues 在复用同一 URL 实例时读到旧缓存
            let sharedDate = ((try? fm.attributesOfItem(atPath: sURL.path))?[.modificationDate] as? Date) ?? .distantPast
            let overrideDate = ((try? fm.attributesOfItem(atPath: oURL.path))?[.modificationDate] as? Date) ?? .distantPast
            if overrideDate > sharedDate {
                return load(from: oURL)
            }
            // App 端后来又改过设置：清掉键盘本地的旧覆盖文件，回到同一份配置
            try? fm.removeItem(at: oURL)
            return load(from: sURL)
        }
        if hasOverride, let oURL = overrideURL {
            return load(from: oURL)
        }
        if let sURL = sharedURL {
            return load(from: sURL)
        }
        return IMESettings()
    }

    /// 从指定文件读取；文件不存在或根本不是 JSON 对象时回到默认设置，个别字段缺失或读不懂只影响那个字段。
    static func load(from url: URL) -> IMESettings {
        guard let data = try? Data(contentsOf: url),
              let settings = try? JSONDecoder().decode(IMESettings.self, from: data) else {
            return IMESettings()
        }
        return settings
    }

    /// 只应由 App 调用；未开完全访问时键盘对 App Group 没有写权限。
    func save() throws {
        guard let url = AppGroup.settingsURL else { throw CocoaError(.fileNoSuchFile) }
        try save(to: url)
    }

    /// 键盘快捷面板保存设置：优先直接写进 App Group（开启完全访问时成功，App 打开后也能读到）；
    /// 未开启完全访问写不了 App Group 时，退回写进键盘自己的容器。
    func saveFromKeyboard(sharedURL: URL? = AppGroup.settingsURL, overrideURL: URL?) {
        if let sURL = sharedURL, (try? save(to: sURL)) != nil {
            if let oURL = overrideURL {
                try? FileManager.default.removeItem(at: oURL)
            }
            return
        }
        if let oURL = overrideURL {
            try? save(to: oURL)
        }
    }

    /// 写到指定文件（目录不存在会先建），原子替换，键盘不会读到写了一半的文件。
    func save(to url: URL) throws {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try JSONEncoder().encode(self).write(to: url, options: .atomic)
    }
}

private extension KeyedDecodingContainer {
    /// 有这个键而且值读得懂就返回它，否则 nil（不抛错）
    func value<T: Decodable>(_ key: Key) -> T? {
        (try? decodeIfPresent(T.self, forKey: key)) ?? nil
    }
}
