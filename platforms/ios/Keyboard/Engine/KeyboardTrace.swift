import Darwin
import Foundation
import os

/// 键盘生命周期与本地模型的诊断记录：
/// 1. 写进系统统一日志（subsystem `com.zengji.aipinyin.keyboard`，category `lifecycle`）；
/// 2. 异步追加到键盘容器内的滚动诊断文件 `Library/Application Support/AIPinyin/trace.log`（超过 64KB 自动轮转为 `trace.prev.log`），
///    无需 Mac root 权限即可用 `devicectl` 秒级拉回真机呼出各阶段毫秒级耗时：
///
/// ```text
/// xcrun devicectl device copy from --device <UDID> \
///     --domain-type appDataContainer --domain-identifier com.zengji.aipinyin.keyboard \
///     --source "Library/Application Support/AIPinyin/trace.log" --destination trace.log
/// ```
///
/// 只记录事件名、阶段耗时、内存与状态，**从不记录用户输入的任何字符、拼音、候选词或上文**。
enum KeyboardTrace {
    static let logger = Logger(subsystem: "com.zengji.aipinyin.keyboard", category: "lifecycle")
    static let defaultMaxLogBytes = 64 * 1024

    /// 进程内第一次碰到 `KeyboardTrace` 的单调时钟时刻
    private static let firstTouchUptime = now
    /// 内核 `p_starttime` 记录的进程真实诞生时刻（含 dyld、静态构造与 `_NSExtensionMain` 耗时）
    private static let kernelBirthDate: Date? = queryKernelProcessBirthDate()
    private static let pid = ProcessInfo.processInfo.processIdentifier
    private static var controllersCreated = 0

    private static let fileQueue = DispatchQueue(label: "com.zengji.aipinyin.trace", qos: .background)

    /// 单调时钟（秒），只用来算阶段耗时
    static var now: TimeInterval { ProcessInfo.processInfo.systemUptime }

    /// 距离内核创建本扩展进程过去了多少毫秒（冷启动时反映 pre-main / dyld / XPC 握手总耗时）
    static var sinceProcessSpawnMs: Int {
        if let birth = kernelBirthDate {
            return max(0, Int((Date().timeIntervalSince(birth) * 1000).rounded()))
        }
        return ms(since: firstTouchUptime)
    }

    static func event(_ name: String, _ detail: @autoclosure () -> String = "") {
        let detailText = detail()
        let memory = memorySummary
        let wallNow = Date()
        let processID = pid
        logger.notice("\(name, privacy: .public) \(detailText, privacy: .public) \(memory, privacy: .public)")

        fileQueue.async {
            guard let url = EngineFactory.traceLogURL else { return }
            let stamp = formatTimestamp(wallNow)
            let body = detailText.isEmpty ? "\(name) \(memory)" : "\(name) \(detailText) \(memory)"
            let line = "\(stamp) [p\(processID)] \(body)\n"
            appendRecord(line, maxBytes: defaultMaxLogBytes, to: url)
        }
    }

    /// 新建了一个键盘控制器。进程里的第一个说明这次是冷启动（扩展进程刚被拉起）
    static func controllerCreated(phase: String = "loadView") {
        _ = firstTouchUptime
        controllersCreated += 1
        event("controller.created",
              "n=\(controllersCreated) phase=\(phase) sinceSpawn=\(sinceProcessSpawnMs)ms swiftAge=\(ms(since: firstTouchUptime))ms")
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

    // MARK: - 滚动日志文件（上限 maxBytes，超出时轮转为 .prev.log）

    static func appendRecord(_ line: String, maxBytes: Int = defaultMaxLogBytes, to url: URL) {
        guard let data = line.data(using: .utf8), !data.isEmpty else { return }
        let fm = FileManager.default
        let dir = url.deletingLastPathComponent()
        if !fm.fileExists(atPath: dir.path) {
            try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        }

        if let size = (try? fm.attributesOfItem(atPath: url.path)[.size] as? NSNumber)?.intValue,
           size + data.count > max(256, maxBytes) {
            let base = url.deletingPathExtension().lastPathComponent
            let prevURL = dir.appendingPathComponent("\(base).prev.log")
            try? fm.removeItem(at: prevURL)
            try? fm.moveItem(at: url, to: prevURL)
        }

        if fm.fileExists(atPath: url.path), let handle = try? FileHandle(forWritingTo: url) {
            defer { try? handle.close() }
            _ = try? handle.seekToEnd()
            try? handle.write(contentsOf: data)
        } else {
            var target = url
            try? data.write(to: target, options: .atomic)
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try? target.setResourceValues(values)
        }
    }

    private static func formatTimestamp(_ date: Date) -> String {
        var tv = timeval()
        let seconds = date.timeIntervalSince1970
        let whole = floor(seconds)
        tv.tv_sec = Int(whole)
        tv.tv_usec = Int32(((seconds - whole) * 1000).rounded())
        if tv.tv_usec >= 1000 {
            tv.tv_sec += 1
            tv.tv_usec -= 1000
        }
        var tmVal = tm()
        localtime_r(&tv.tv_sec, &tmVal)
        return String(
            format: "%04d-%02d-%02d %02d:%02d:%02d.%03d",
            Int(tmVal.tm_year) + 1900,
            Int(tmVal.tm_mon) + 1,
            Int(tmVal.tm_mday),
            Int(tmVal.tm_hour),
            Int(tmVal.tm_min),
            Int(tmVal.tm_sec),
            Int(tv.tv_usec)
        )
    }

    private static func queryKernelProcessBirthDate() -> Date? {
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        let rc = sysctl(&mib, UInt32(mib.count), &info, &size, nil, 0)
        guard rc == 0, size > 0 else { return nil }
        let st = info.kp_proc.p_starttime
        guard st.tv_sec > 0 else { return nil }
        let epoch = TimeInterval(st.tv_sec) + TimeInterval(st.tv_usec) / 1_000_000.0
        return Date(timeIntervalSince1970: epoch)
    }
}
