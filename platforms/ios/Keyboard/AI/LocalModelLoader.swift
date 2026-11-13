import Foundation
import os

/// 本地 GGUF 模型（llama.cpp，在键盘进程内用 mmap 从 App Group 只读加载）的装载与卸载。
///
/// 键盘扩展超过 177MB 就会被系统杀掉，模型常驻要 60–120MB，所以：
/// - 只在首帧之后、后台队列里加载；
/// - 换模型时先卸掉旧的，再在加载前一刻（队列上）看 os_proc_available_memory()，低于 minimumAvailableBytes
///   就不加载：两份模型不会同时驻留，也不会因为旧模型还占着内存而误报「内存不足」；
/// - 收到内存警告立即卸载，本次键盘显示期间不再重试；
/// - 卸载和内存警告只要引擎里真有模型就会卸，不看自己记的状态（加载失败等情况下也不会把模型留在引擎里）。
/// 状态只在主线程读写；加载、卸载在同一个串行队列上排队，先后顺序有保证。
final class LocalModelLoader {
    /// 剩余可用内存低于这个值就不加载（起始值，按真机实测再调）
    static let minimumAvailableBytes: UInt64 = 110 * 1024 * 1024

    enum State: Equatable {
        case idle
        /// 设置里选的模型还没下载
        case missing
        case loading
        case loaded
        case failed
        /// 剩余内存不够，没有加载
        case skippedLowMemory
        /// 因内存警告卸载
        case unloadedForMemory
    }

    private(set) var state: State = .idle {
        didSet { if state != oldValue { onStateChange?(state) } }
    }
    var onStateChange: ((State) -> Void)?

    private let queue: DispatchQueue
    private let availableMemory: () -> UInt64
    private let fileExists: (URL) -> Bool
    private var loadedURL: URL?
    /// 每次加载 / 卸载加一，过期的加载结果据此丢弃
    private var generation = 0

    init(queue: DispatchQueue = DispatchQueue(label: "com.zengji.aipinyin.local-model", qos: .utility),
         availableMemory: @escaping () -> UInt64 = LocalModelLoader.systemAvailableMemory,
         fileExists: @escaping (URL) -> Bool = { FileManager.default.fileExists(atPath: $0.path) }) {
        self.queue = queue
        self.availableMemory = availableMemory
        self.fileExists = fileExists
    }

    /// 离系统的内存上限还有多少
    static func systemAvailableMemory() -> UInt64 {
        #if targetEnvironment(simulator)
        // 模拟器里键盘扩展没有内存上限，os_proc_available_memory() 恒为 0；当作不受限，好在模拟器里试本地模型
        return .max
        #else
        return UInt64(max(0, os_proc_available_memory()))
        #endif
    }

    /// 键盘重新出现：清掉上次内存警告的记录，允许再试
    func keyboardWillAppear() {
        if state == .unloadedForMemory || state == .skippedLowMemory { state = .idle }
    }

    private enum Outcome {
        case loaded, failed, lowMemory
    }

    /// 按需加载。url 为 nil 表示设置里还没选模型
    func load(_ url: URL?, into engine: IMEEngine) {
        guard state != .unloadedForMemory else { return }
        if state == .loading || state == .loaded, loadedURL == url { return }
        guard let url, fileExists(url) else {
            unload(into: engine)
            state = .missing
            return
        }
        // 引擎里没有模型、也没有排着的加载时，现在量到的就是加载前的内存：不够就直接报，不必先显示「正在加载」
        if state != .loading, !engine.isLocalModelLoaded, availableMemory() < Self.minimumAvailableBytes {
            state = .skippedLowMemory
            return
        }
        generation += 1
        let current = generation
        loadedURL = url
        state = .loading
        let availableMemory = self.availableMemory
        queue.async { [weak self, weak engine] in
            let outcome: Outcome
            if let engine {
                // 换模型：先卸掉旧的再量内存。旧模型还在时量，剩余内存总是不够（两份模型也放不下）
                if engine.isLocalModelLoaded { engine.unloadLocalModel() }
                if availableMemory() < Self.minimumAvailableBytes {
                    outcome = .lowMemory
                } else {
                    // 失败时引擎里不会留着模型（引擎先卸载旧的再加载）
                    outcome = (try? engine.loadLocalModel(at: url)) != nil ? .loaded : .failed
                }
            } else {
                outcome = .failed
            }
            DispatchQueue.main.async {
                guard let self, self.generation == current else { return }
                switch outcome {
                case .loaded:
                    self.state = .loaded
                case .failed:
                    self.loadedURL = nil
                    self.state = .failed
                case .lowMemory:
                    self.loadedURL = nil
                    self.state = .skippedLowMemory
                }
            }
        }
    }

    /// 换了引擎（词库换了）：加载记录作废，排着的加载结果也丢掉，新引擎接上时 load 会重新加载。
    /// 旧引擎上有模型（或正在加载）时，在串行队列上先把它卸掉：这个块强引用着旧引擎，卸完才放手。
    /// 之后排进来的新引擎的加载在它后面，量到的是卸掉以后的内存，两份模型不会同时驻留
    func engineReplaced(old: IMEEngine?) {
        let active = state == .loading || state == .loaded
        generation += 1
        loadedURL = nil
        if let old, active || old.isLocalModelLoaded {
            queue.async { old.unloadLocalModel() }
        }
        if active { state = .idle }
    }

    /// 设置里关掉了本地模型（或换成了别的 AI 模式）
    func unload(into engine: IMEEngine) {
        let active = state == .loading || state == .loaded
        guard active || engine.isLocalModelLoaded else { return }
        enqueueUnload(engine)
        if active { state = .idle }
    }

    /// 内存警告：立刻卸载
    func didReceiveMemoryWarning(engine: IMEEngine) {
        guard state == .loading || state == .loaded || engine.isLocalModelLoaded else { return }
        enqueueUnload(engine)
        state = .unloadedForMemory
    }

    private func enqueueUnload(_ engine: IMEEngine) {
        generation += 1
        loadedURL = nil
        // 和加载排在同一个队列上：正在加载的话，等它加载完再卸载
        queue.async { [weak engine] in
            engine?.unloadLocalModel()
        }
    }
}
