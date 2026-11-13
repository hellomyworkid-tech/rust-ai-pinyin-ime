import Foundation

/// 可下载的本地模型。App 下载到 AppGroup.modelsDirectory，键盘用 mmap 只读加载。
struct LocalModel: Identifiable, Equatable {
    let id: String
    let title: String
    let fileName: String
    let downloadURL: URL
    /// 文件字节数（用于进度与校验）
    let bytes: Int64
    /// 键盘加载这个模型、打过分之后的总内存（MiB，含键盘本身；上限 177）。
    /// iPhone 16 Pro Max / iOS 27.2 上用 UITests/KeyboardMemoryTests 实测，见 README「真机评测」
    let residentMiB: Int
    let note: String
    /// 文件的 SHA-256（十六进制小写）。下载完先核对它再交给键盘；nil 表示不核对
    var sha256: String? = nil

    var localURL: URL? { AppGroup.modelsDirectory?.appendingPathComponent(fileName) }

    var isDownloaded: Bool {
        guard let url = localURL,
              let size = try? url.resourceValues(forKeys: [.fileSizeKey]).fileSize else { return false }
        return Int64(size) == bytes
    }
}

enum ModelCatalog {
    /// 默认推荐 Qwen2.5-0.5B。两个模型在手机上（iPhone 16 Pro Max，iOS 27.2，2026-11-11）准确率相当：
    /// 第一次见到的 389 条中文用例 373 对 Qwen3 的 377，习惯词测试 58 对 54（共 63），
    /// 教会的词换个上文仍在首位 29/37 对 25/34、反过来 33/37 对 30/34（只算经过改选的组，Mac 上测），
    /// 会话模拟只差 1–2 条；等 AI 的时间也相当（短上文中位约 60–130 ms，长上文约 290 ms）。
    /// 差别在内存：键盘总内存 67 MB 对 93 MB（上限 177 MB，11-12 重测）。多出的 26 MB 基本就是 KV 缓存
    /// （上下文 256 个 token，每个 token 12 KiB 对 112 KiB），Qwen2.5 留给键盘的余量更大。
    /// Qwen3 的文件小 95 MB。
    static let all: [LocalModel] = [
        LocalModel(
            id: "qwen2.5-0.5b-q4km",
            title: "通义千问 2.5 · 0.5B",
            fileName: "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            downloadURL: URL(string: "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf")!,
            bytes: 491_400_032,
            residentMiB: 67,
            note: "推荐。键盘内存占用最小",
            // Hugging Face 公布的文件哈希（x-linked-etag），与本机下载的文件一致（2026-11-11 核对）
            sha256: "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db"
        ),
        LocalModel(
            id: "qwen3-0.6b-q4km",
            title: "通义千问 3 · 0.6B",
            fileName: "Qwen3-0.6B-Q4_K_M.gguf",
            downloadURL: URL(string: "https://huggingface.co/unsloth/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q4_K_M.gguf")!,
            bytes: 396_705_472,
            residentMiB: 93,
            note: "文件更小，准确率相当，但键盘内存多约 25 MB",
            sha256: "ac2d97712095a558e31573f62f466a3f9d93990898b0ec79d7c974c1780d524a"
        ),
    ]

    static func find(_ id: String?) -> LocalModel? { all.first { $0.id == id } }
}
