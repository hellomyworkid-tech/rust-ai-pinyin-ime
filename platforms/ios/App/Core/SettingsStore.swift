import Foundation
import Combine

/// 设置的唯一写入口。每次改动立刻通过 IMESettings.save() 写进 App Group 的 settings.json，
/// 键盘下次出现时读到。写失败不吞掉，留在 saveError 里让界面提示。
@MainActor
final class SettingsStore: ObservableObject {
    @Published private(set) var settings: IMESettings
    @Published var saveError: String?

    private let persist: (IMESettings) throws -> Void

    init(initial: IMESettings = IMESettings.load(),
         persist: @escaping (IMESettings) throws -> Void = { try $0.save() }) {
        settings = initial
        self.persist = persist
    }

    /// 改设置。没有变化就不写盘。
    func update(_ change: (inout IMESettings) -> Void) {
        var next = settings
        change(&next)
        guard next != settings else { return }
        settings = next
        do {
            try persist(next)
            saveError = nil
        } catch {
            saveError = "设置没有保存成功：\(error.localizedDescription)"
        }
    }

    /// App 回到前台时重读一次设置（开启完全访问后，用户可能在键盘快捷面板里改过开关）
    func reload(using loader: () -> IMESettings = { IMESettings.load() }) {
        let latest = loader()
        if latest != settings {
            settings = latest
        }
    }
}
