import Foundation

/// 暂存未完成的拼音组合快照（切应用、截图、收起键盘或临时切到别的输入框后，回到原输入框时恢复继续选字）。
///
/// 匹配策略：
/// 1. 优先按 `UITextDocumentProxy.documentIdentifier` 精确匹配；
/// 2. SwiftUI `TextField`、部分网页或跨应用恢复时，同一输入框重新聚焦会分配新的 `documentIdentifier`，
///    此时回退匹配最近一条 `FieldTraits`（键盘类型与回车键类型）一致且光标前上文后缀一致的快照；
///    同一页面从输入框 A 直接点到另一个空白中文输入框 B 时，通过 `excludingFallbackID` 排除刚离开的 A，
///    避免把 A 的未完成拼音带进 B，等点回 A 时再恢复。
final class PendingCompositionStore {
    struct Entry: Codable, Equatable {
        let documentID: String
        let snapshot: String
        let keyboardTypeRaw: Int
        let returnKeyTypeRaw: Int
        let contextTail: String
        let savedAt: Date
    }

    /// 超过有效期的未完成组合自动丢弃，避免很久以前留下的半截拼音突然冒出来
    static let defaultTTL: TimeInterval = 15 * 60
    /// 光标前没有上文（空白输入框）时，回退匹配只认这么近的快照：空白输入框之间无法区分，
    /// 放宽了会把一个 App 里没打完的拼音带进另一个 App 的空白输入框
    static let emptyContextFallbackWindow: TimeInterval = 2 * 60
    /// 最多保留最近几个输入框的快照，文件始终很小
    static let defaultMaxEntries = 8
    private static let contextTailLimit = 16

    private let fileURL: URL?
    private let ttl: TimeInterval
    private let maxEntries: Int
    private var entries: [String: Entry] = [:]
    private var loadedFromDisk = false

    init(fileURL: URL? = nil, ttl: TimeInterval = PendingCompositionStore.defaultTTL,
         maxEntries: Int = PendingCompositionStore.defaultMaxEntries) {
        self.fileURL = fileURL
        self.ttl = ttl
        self.maxEntries = max(1, maxEntries)
    }

    /// 快速判断是否可能存在未完成快照：无文件时直接返回 false，冷启动无需触发宿主进程 `documentContextBeforeInput` XPC
    var mayHaveEntries: Bool {
        if loadedFromDisk && entries.isEmpty {
            guard let fileURL else { return false }
            return FileManager.default.fileExists(atPath: fileURL.path)
        }
        if !entries.isEmpty { return true }
        guard let fileURL else { return false }
        return FileManager.default.fileExists(atPath: fileURL.path)
    }

    /// 保存某个输入框当前的未完成组合快照
    func save(_ snapshot: String, for documentID: UUID, traits: FieldTraits = FieldTraits(),
              contextBefore: String? = nil, now: Date = Date()) {
        guard !snapshot.isEmpty else {
            remove(for: documentID)
            return
        }
        reloadFromDiskIfAvailable()
        pruneExpired(now: now)
        let key = documentID.uuidString
        let tail = String((contextBefore ?? "").suffix(Self.contextTailLimit))
        entries[key] = Entry(
            documentID: key,
            snapshot: snapshot,
            keyboardTypeRaw: traits.keyboardType.rawValue,
            returnKeyTypeRaw: traits.returnKeyType.rawValue,
            contextTail: tail,
            savedAt: now
        )
        trimToMaxEntries()
        persistToDisk()
    }

    /// 取出并移除与当前输入框匹配的暂存快照；不存在或已过期时返回 nil。
    /// `contextBefore` 按需惰性求值：精确命中同一 `documentIdentifier` 或全部过期时，不触发宿主 `documentContextBeforeInput` XPC
    func take(for documentID: UUID, traits: FieldTraits = FieldTraits(),
              contextBefore: @autoclosure () -> String? = nil, excludingFallbackID: UUID? = nil,
              now: Date = Date()) -> String? {
        reloadFromDiskIfAvailable()
        let pruned = pruneExpired(now: now)
        let exactKey = documentID.uuidString

        if let exact = entries.removeValue(forKey: exactKey) {
            persistToDisk()
            return exact.snapshot
        }

        guard !entries.isEmpty else {
            if pruned {
                persistToDisk()
            }
            return nil
        }

        let excludedKey = excludingFallbackID?.uuidString
        let currentContext = contextBefore() ?? ""
        let fallbackKey = entries
            .filter { key, entry in
                guard key != excludedKey else { return false }
                guard entry.keyboardTypeRaw == traits.keyboardType.rawValue,
                      entry.returnKeyTypeRaw == traits.returnKeyType.rawValue else {
                    return false
                }
                if entry.contextTail.isEmpty {
                    return currentContext.isEmpty
                        && now.timeIntervalSince(entry.savedAt) <= Self.emptyContextFallbackWindow
                }
                return currentContext.hasSuffix(entry.contextTail)
            }
            .max { $0.value.savedAt < $1.value.savedAt }?
            .key

        if let fallbackKey, let matched = entries.removeValue(forKey: fallbackKey) {
            persistToDisk()
            return matched.snapshot
        }

        if pruned {
            persistToDisk()
        }
        return nil
    }

    /// 清掉某个输入框暂存的快照（组合已上屏或删空时调用；没有记录时不产生磁盘写入）。
    /// 每次都重读文件：别的宿主进程里的键盘可能已经取走或改过记录，不能用内存里的旧副本写回去
    func remove(for documentID: UUID) {
        guard let fileURL, FileManager.default.fileExists(atPath: fileURL.path) else {
            loadedFromDisk = true
            entries.removeValue(forKey: documentID.uuidString)
            return
        }
        reloadFromDiskIfAvailable()
        guard entries.removeValue(forKey: documentID.uuidString) != nil else { return }
        persistToDisk()
    }

    /// 丢掉过期的快照（键盘启动时调用）：键盘很久不再打开时，残留的未完成输入也不会一直留在磁盘上
    func purgeExpired(now: Date = Date()) {
        guard let fileURL, FileManager.default.fileExists(atPath: fileURL.path) else { return }
        reloadFromDiskIfAvailable()
        if pruneExpired(now: now) {
            persistToDisk()
        }
    }

    // MARK: - 内部

    @discardableResult
    private func pruneExpired(now: Date) -> Bool {
        let before = entries.count
        entries = entries.filter { now.timeIntervalSince($0.value.savedAt) <= ttl }
        return entries.count != before
    }

    private func trimToMaxEntries() {
        guard entries.count > maxEntries else { return }
        let sortedKeys = entries
            .sorted { $0.value.savedAt > $1.value.savedAt }
            .prefix(maxEntries)
            .map(\.key)
        let keep = Set(sortedKeys)
        entries = entries.filter { keep.contains($0.key) }
    }

    private func reloadFromDiskIfAvailable() {
        loadedFromDisk = true
        guard let fileURL,
              let data = try? Data(contentsOf: fileURL),
              let decoded = try? JSONDecoder().decode([String: Entry].self, from: data) else {
            if let fileURL, !FileManager.default.fileExists(atPath: fileURL.path) {
                entries = [:]
            }
            return
        }
        entries = decoded
    }

    private func persistToDisk() {
        guard var fileURL else { return }
        let fm = FileManager.default
        if entries.isEmpty {
            try? fm.removeItem(at: fileURL)
            return
        }
        let dir = fileURL.deletingLastPathComponent()
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        guard let data = try? JSONEncoder().encode(entries) else { return }
        // 未上屏的输入属于隐私：设备锁定后不可读，也不进 iCloud / 电脑备份
        guard (try? data.write(to: fileURL, options: [.atomic, .completeFileProtectionUnlessOpen])) != nil else {
            return
        }
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try? fileURL.setResourceValues(values)
    }
}
