import Darwin
import Foundation
import os

@_silgen_name("aipinyin_dyld_init_epoch_us")
private func aipinyin_dyld_init_epoch_us() -> Int64

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
    /// 内核 `p_starttime` 记录的进程真实诞生时刻（含 amfid / dyld / `_NSExtensionMain` 耗时）
    private static let kernelBirthDate: Date? = queryKernelProcessBirthDate()
    /// C `__attribute__((constructor))` 在 `dyld` 完成镜像加载时记录的时刻（用来精准切分 pre-main 与 XPC 握手耗时）
    private static let dyldInitDate: Date? = {
        let us = aipinyin_dyld_init_epoch_us()
        guard us > 0 else { return nil }
        return Date(timeIntervalSince1970: Double(us) / 1_000_000.0)
    }()
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

    /// 内核 `p_starttime` 到 C constructor（`dyld` 完成加载）的耗时（毫秒）
    static var preMainMs: Int? {
        guard let birth = kernelBirthDate, let dyld = dyldInitDate else { return nil }
        return max(0, Int((dyld.timeIntervalSince(birth) * 1000).rounded()))
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
        let spawnMs = sinceProcessSpawnMs
        let breakdown: String
        if let pre = preMainMs, let dyld = dyldInitDate {
            let extMs = max(0, Int((Date().timeIntervalSince(dyld) * 1000).rounded()))
            breakdown = "sinceSpawn=\(spawnMs)ms preMain=\(pre)ms extMain=\(extMs)ms"
        } else {
            breakdown = "sinceSpawn=\(spawnMs)ms"
        }
        event("controller.created",
              "n=\(controllersCreated) phase=\(phase) \(breakdown) swiftAge=\(ms(since: firstTouchUptime))ms")
    }

    /// 毫秒级耗时
    static func ms(since start: TimeInterval) -> Int {
        Int(((now - start) * 1000).rounded())
    }

    /// 单次键盘呼出会话内的匿名按键解码耗时与异步 AI 等待耗时统计（环形定长样本，绝不记录任何输入文本、拼音或候选词）
    struct KeystrokeLatencyStats {
        static let maxSamples = 512

        private(set) var totalKeystrokes = 0
        private(set) var totalAIWaits = 0
        private var decodeSamplesMs: [Double] = []
        private var aiWaitSamplesMs: [Double] = []

        mutating func recordDecode(ms: Double) {
            guard ms.isFinite else { return }
            let clamped = max(0, ms)
            totalKeystrokes += 1
            if decodeSamplesMs.count < Self.maxSamples {
                decodeSamplesMs.append(clamped)
            } else {
                decodeSamplesMs[(totalKeystrokes - 1) % Self.maxSamples] = clamped
            }
        }

        mutating func recordAIWait(ms: Double) {
            guard ms.isFinite else { return }
            let clamped = max(0, ms)
            totalAIWaits += 1
            if aiWaitSamplesMs.count < Self.maxSamples {
                aiWaitSamplesMs.append(clamped)
            } else {
                aiWaitSamplesMs[(totalAIWaits - 1) % Self.maxSamples] = clamped
            }
        }

        /// 生成聚合耗时摘要（如 `keys=12 decode=p50:0.9/p95:2.1/max:3.4ms aiWait=n:5,p50:38/p95:82/max:105ms`）；
        /// 本次会话无中文解码按键且无 AI 等待时返回 nil
        func summary() -> String? {
            guard totalKeystrokes > 0 || totalAIWaits > 0 else { return nil }
            var parts: [String] = []
            if let d = Self.percentiles(decodeSamplesMs) {
                parts.append(String(format: "keys=%d decode=p50:%.1f/p95:%.1f/max:%.1fms", totalKeystrokes, d.p50, d.p95, d.max))
            }
            if let a = Self.percentiles(aiWaitSamplesMs) {
                parts.append(String(format: "aiWait=n:%d,p50:%.0f/p95:%.0f/max:%.0fms", totalAIWaits, a.p50, a.p95, a.max))
            }
            return parts.isEmpty ? nil : parts.joined(separator: " ")
        }

        mutating func reset() {
            totalKeystrokes = 0
            totalAIWaits = 0
            decodeSamplesMs.removeAll(keepingCapacity: true)
            aiWaitSamplesMs.removeAll(keepingCapacity: true)
        }

        private static func percentiles(_ samples: [Double]) -> (p50: Double, p95: Double, max: Double)? {
            guard !samples.isEmpty else { return nil }
            let sorted = samples.sorted()
            let last = sorted.count - 1
            let p50Idx = max(0, min(last, Int(ceil(Double(sorted.count) * 0.50)) - 1))
            let p95Idx = max(0, min(last, Int(ceil(Double(sorted.count) * 0.95)) - 1))
            return (sorted[p50Idx], sorted[p95Idx], sorted[last])
        }
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
