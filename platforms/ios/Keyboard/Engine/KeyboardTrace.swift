import Foundation
import os

/// 键盘生命周期与本地模型的诊断记录，写进系统统一日志
/// （subsystem `com.zengji.aipinyin.keyboard`，category `lifecycle`）。
///
/// 只记录事件名、耗时、内存与状态，**从不记录用户输入的任何内容**。用 notice 级别：会持久化到设备日志，
/// 出现「刚呼出键盘就被切回自带输入法」之类的问题后，可以把当时的记录拉回来对照系统的终止原因：
///
/// ```text
/// sudo log collect --device-udid <UDID> --last 30m --output keyboard.logarchive
/// log show keyboard.logarchive --predicate 'subsystem == "com.zengji.aipinyin.keyboard"
///     OR (eventMessage CONTAINS "AIPinyinKeyboard")' --info
/// ```
///
/// 每条记录末尾附上当时的 `fp`（phys_footprint，系统判定是否超限的口径）与 `avail`（离本进程上限还剩多少），单位 MiB。
enum KeyboardTrace {
    static let logger = Logger(subsystem: "com.zengji.aipinyin.keyboard", category: "lifecycle")

    /// 进程里第一次用到它的时刻（键盘扩展进程启动后，第一个控制器的 loadView 会先碰到它）
    private static let processStart = now
    private static var controllersCreated = 0

    /// 单调时钟（秒），只用来算耗时
    static var now: TimeInterval { ProcessInfo.processInfo.systemUptime }

    static func event(_ name: String, _ detail: @autoclosure () -> String = "") {
        let detail = detail()
        let memory = memorySummary
        logger.notice("\(name, privacy: .public) \(detail, privacy: .public) \(memory, privacy: .public)")
    }

    /// 新建了一个键盘控制器。进程里的第一个说明这次是冷启动（扩展进程刚被拉起）
    static func controllerCreated() {
        _ = processStart
        controllersCreated += 1
        event("controller.created", "n=\(controllersCreated) processAge=\(ms(since: processStart))ms")
    }

    /// 毫秒级耗时
    static func ms(since start: TimeInterval) -> Int {
        Int(((now - start) * 1000).rounded())
    }

    static var memorySummary: String {
        let megabyte = 1024.0 * 1024.0
        let footprint = Double(RustEngine.memoryFootprint) / megabyte
        #if targetEnvironment(simulator)
        return String(format: "fp=%.1f", footprint)
        #else
        let available = Double(max(0, os_proc_available_memory())) / megabyte
        return String(format: "fp=%.1f avail=%.1f", footprint, available)
        #endif
    }
}
