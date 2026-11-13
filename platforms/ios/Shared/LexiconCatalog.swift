import Foundation

/// 完整词库的一个发布版本：由雾凇拼音（rime-ice）等开源词表编译成的 AIMELX03 镜像。
///
/// 雾凇拼音是 GPL-3.0-only，不打进 App Store 的安装包（见 ime-ios-research/词库协议调研.md）：
/// App 第一次打开时下载固定版本的压缩包（xz），核对 SHA-256 后解压，放进 App Group 的 Lexicon 目录，
/// 键盘只读 mmap。下载、解压的流程见 App/Core/ModelDownloadManager.swift（DownloadManager）。
/// 压缩包由 scripts/package-lexicon.sh 生成，下面的大小和 SHA-256 也照它打印的抄。
struct LexiconRelease: Identifiable, Equatable {
    /// 版本，也是文件名的一部分。换版本就是换文件名：键盘在新版本下完之前继续用旧版本
    let version: String
    let downloadURL: URL
    /// 下载的压缩包的字节数和 SHA-256
    let bytes: Int64
    let sha256: String?
    /// 解压后（键盘读的文件）的字节数和 SHA-256
    let installedBytes: Int64
    let installedSHA256: String?
    /// 词表来源，显示在 App 里
    let sources: String

    var id: String { "lexicon-\(version)" }
    var fileName: String { "\(LexiconCatalog.filePrefix)\(version).bin" }
    var localURL: URL? { AppGroup.lexiconDirectory?.appendingPathComponent(fileName) }

    /// 换一个下载地址（调试时从本机下载用）
    func with(downloadURL: URL) -> LexiconRelease {
        LexiconRelease(version: version, downloadURL: downloadURL, bytes: bytes, sha256: sha256,
                       installedBytes: installedBytes, installedSHA256: installedSHA256, sources: sources)
    }
}

enum LexiconCatalog {
    static let filePrefix = "merged_lexicon-"

    /// 这一版 App 要用的词库。2026.11.12.2 取代 2026.11.12：编译规则改成可复现（同分词条按固定顺序排），
    /// 用发布页上的源码包能编出逐字节相同的文件
    static let current = LexiconRelease(
        version: "2026.11.12.2",
        downloadURL: URL(string: "https://github.com/hellomyworkid-tech/rust-ai-pinyin-ime/releases/download/lexicon-2026.11.12.2/merged_lexicon-2026.11.12.2.bin.xz")!,
        bytes: 11_511_512,
        sha256: "ff916e0e5ea0e8aedfd4186e14f5f1173f860edf0ccfd4bf905f2026141e2cb3",
        installedBytes: 57_956_668,
        installedSHA256: "70da48fd0d01f04c4b109187aa5d21006ee69305cbdd3e3d7b5090c7e01d582f",
        sources: "雾凇拼音 rime-ice（基础词库、8105 字表、英文词库，2026-10-01 版），GPL-3.0"
    )

    /// 键盘该用的完整词库：当前版本装好了就用它；没有就用目录里装好的别的版本
    /// （App 升级后新版本还没下完时，旧版本仍然可用）。大小不对的文件不用（.part 等半成品名字也不对）
    static func installedLexiconURL(in directory: URL? = AppGroup.lexiconDirectory,
                                    current: LexiconRelease = current,
                                    fileManager: FileManager = .default) -> URL? {
        guard let directory else { return nil }
        let currentURL = directory.appendingPathComponent(current.fileName)
        if fileSize(currentURL, fileManager) == current.installedBytes { return currentURL }
        guard let names = try? fileManager.contentsOfDirectory(atPath: directory.path) else { return nil }
        // 别的版本：只认 merged_lexicon-<版本>.bin，取版本号最大的（逐段按数字比：2026.11.12.2 比 2026.11.12 新）
        let others = names
            .filter { $0 != current.fileName }
            .compactMap { name in version(ofFile: name).map { (name, $0) } }
            .sorted { $0.1.lexicographicallyPrecedes($1.1) }
        return others.last.map { directory.appendingPathComponent($0.0) }
    }

    /// merged_lexicon-<版本>.bin 里的版本号，按点分成数字；不是这个格式的文件名返回 nil
    static func version(ofFile name: String) -> [Int]? {
        guard name.hasPrefix(filePrefix), name.hasSuffix(".bin") else { return nil }
        let text = name.dropFirst(filePrefix.count).dropLast(".bin".count)
        let parts = text.split(separator: ".", omittingEmptySubsequences: false).map { Int($0) }
        guard !parts.isEmpty, parts.allSatisfy({ $0 != nil }) else { return nil }
        return parts.compactMap { $0 }
    }

    /// 当前版本装好之后，删掉目录里别的版本（键盘若还映射着旧文件，删掉名字不影响它，重建引擎时换新的）。
    /// 只删装好的 .bin；解压中的 .part 归下载管理器管（它启动时清理留下的半成品）
    static func removeOtherVersions(in directory: URL? = AppGroup.lexiconDirectory,
                                    keeping current: LexiconRelease = current,
                                    fileManager: FileManager = .default) {
        guard let directory,
              let names = try? fileManager.contentsOfDirectory(atPath: directory.path) else { return }
        for name in names where name.hasPrefix(filePrefix) && name.hasSuffix(".bin") && name != current.fileName {
            try? fileManager.removeItem(at: directory.appendingPathComponent(name))
        }
    }

    private static func fileSize(_ url: URL, _ fileManager: FileManager) -> Int64? {
        (try? fileManager.attributesOfItem(atPath: url.path)[.size] as? NSNumber)?.int64Value
    }
}
