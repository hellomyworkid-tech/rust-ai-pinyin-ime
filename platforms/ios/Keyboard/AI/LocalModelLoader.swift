import Foundation
import os

/// 协调同一键盘扩展进程内多个 `KeyboardViewController` 的本地模型驻留。
///
/// iOS 会为不同宿主应用在同一个键盘扩展进程（共享约 177MB 内存上限）里同时保留多个 `KeyboardViewController`：
/// 例如先在「AI 拼音」App 的「试一试」框里加载了模型（占 67–93MB），再切到搜索 / 微信 / 浏览器时，系统会在同一进程里
/// 新建第二个控制器，而前一个控制器尚未销毁。若各自独立判断剩余内存、各自加载模型，第二个控制器就会因为前一个占着内存
/// 而误判「内存不足，本地模型没有加载」。
///
/// 同进程的所有控制器共用这里的串行队列与登记表（状态只在主线程读写）：
/// - 新控制器请求同一个模型时，先复用进程内已加载的模型实例（零额外内存、毫秒内完成），再从旧控制器卸下引用；
/// - 新控制器请求别的模型、关闭本地模型或收到内存警告时，先把旧控制器的模型一并卸掉，再测内存或加载。
final class LocalModelCoordinator {
    static let shared: LocalModelCoordinator = {
        let coordinator = LocalModelCoordinator(
            queue: DispatchQueue(label: "com.zengji.aipinyin.local-model", qos: .utility),
            trimContext: { RustEngine.trimLocalModelContext() },
            releaseCaches: { RustEngine.releaseMemoryCaches() },
            isModelCached: { RustEngine.isLocalModelCached(at: $0) }
        )
        coordinator.installMemoryPressureMonitor()
        return coordinator
    }()

    let queue: DispatchQueue
    private let trimContext: () -> Void
    private let releaseCaches: () -> Void
    private let isModelCached: (URL?) -> Bool
    private var memoryPressureSource: DispatchSourceMemoryPressure?
    private var visibleControllers: Set<ObjectIdentifier> = []

    private final class Entry {
        weak var loader: LocalModelLoader?
        weak var engine: IMEEngine?

        init(loader: LocalModelLoader, engine: IMEEngine) {
            self.loader = loader
            self.engine = engine
        }
    }

    private var entries: [Entry] = []

    init(queue: DispatchQueue = DispatchQueue(label: "com.zengji.aipinyin.local-model", qos: .utility),
         trimContext: @escaping () -> Void = {},
         releaseCaches: @escaping () -> Void = {},
         isModelCached: @escaping (URL?) -> Bool = { _ in false }) {
        self.queue = queue
        self.trimContext = trimContext
        self.releaseCaches = releaseCaches
        self.isModelCached = isModelCached
    }

    deinit {
        memoryPressureSource?.cancel()
    }

    private func installMemoryPressureMonitor() {
        let source = DispatchSource.makeMemoryPressureSource(eventMask: [.warning, .critical], queue: .main)
        source.setEventHandler { [weak self] in
            self?.evictAllForMemoryWarning()
        }
        source.resume()
        memoryPressureSource = source
    }

    /// 某个键盘控制器即将出现或恢复显示
    func controllerWillAppear(_ id: ObjectIdentifier) {
        visibleControllers.insert(id)
    }

    /// 某个键盘控制器消失或宿主切后台。进程内已无可见键盘时，立刻（不用定时器：扩展进程收起后很快就被挂起，
    /// 延迟任务会拖到下次呼出时才执行）在模型队列上只释放推理上下文（KV 缓存与计算缓冲），保留模型本体：
    /// - 挂起期间常驻内存只剩模型本体（真机实测 Qwen2.5 模型本体仅占约 10 MiB）；
    /// - 即使约 1 秒后系统销毁了该控制器，Rust 层 `SHARED_LLM` 仍保留模型本体，下次呼出新控制器几毫秒内即可直接复用；
    /// - 真正卸载整个模型只在系统内存警告 / 内存压力、关闭本地模型或更换模型时发生。
    func controllerDidDisappear(_ id: ObjectIdentifier) {
        guard visibleControllers.remove(id) != nil, visibleControllers.isEmpty else { return }
        let trim = trimContext
        queue.async {
            let start = KeyboardTrace.now
            trim()
            KeyboardTrace.event("model.contextTrimmed", "\(KeyboardTrace.ms(since: start))ms")
        }
    }

    /// 内核发出内存压力信号时，无条件卸掉进程内全部本地模型并清空共享缓存
    func evictAllForMemoryWarning() {
        let engines = evictPeersForMemoryWarning(excluding: nil, keepingEngine: nil)
        KeyboardTrace.event("memory.pressure", "unloading=\(engines.count)")
        let release = releaseCaches
        queue.async {
            for engine in engines where engine.isLocalModelLoaded {
                engine.unloadLocalModel()
            }
            release()
        }
    }

    func hasResidentModel(url: URL) -> Bool {
        if isModelCached(url) {
            return true
        }
        let targetURL = url.standardizedFileURL
        return entries.contains { entry in
            guard let loader = entry.loader,
                  loader.state == .loading || loader.state == .loaded,
                  loader.loadedURL?.standardizedFileURL == targetURL else { return false }
            return entry.engine?.isLocalModelLoaded == true
        }
    }

    func hasAnyCachedModel() -> Bool {
        isModelCached(nil)
    }

    func register(loader: LocalModelLoader, engine: IMEEngine) {
        entries.removeAll { $0.loader == nil && $0.engine == nil }
        if let existing = entries.first(where: { $0.loader === loader }) {
            existing.engine = engine
        } else {
            entries.append(Entry(loader: loader, engine: engine))
        }
    }

    struct Claim {
        var peerEngines: [IMEEngine] = []
        var sameModelResident = false
        var hadDifferentCachedModel = false
    }

    /// 为当前控制器接管本地模型：直接以 Rust 层 `SHARED_LLM` 与同进程活跃引擎的真实状态为准，不维护易失真的镜像字段
    func claimForLoad(by requester: LocalModelLoader, engine: IMEEngine, url: URL) -> Claim {
        register(loader: requester, engine: engine)
        var peerEngines: [IMEEngine] = []
        let targetURL = url.standardizedFileURL
        let cachedExact = isModelCached(url)
        var sameModelResident = cachedExact
        let hadDifferentCachedModel = !cachedExact && isModelCached(nil)
        for entry in entries where entry.loader !== requester {
            let otherLoader = entry.loader
            let otherActive = otherLoader?.state == .loading || otherLoader?.state == .loaded
            if otherActive,
               otherLoader?.loadedURL?.standardizedFileURL == targetURL,
               entry.engine?.isLocalModelLoaded == true {
                sameModelResident = true
            }
            if otherActive {
                otherLoader?.yieldToPeer()
            }
            if let otherEngine = entry.engine,
               otherEngine !== engine,
               (otherActive || otherEngine.isLocalModelLoaded),
               !peerEngines.contains(where: { $0 === otherEngine }) {
                peerEngines.append(otherEngine)
            }
        }
        return Claim(
            peerEngines: peerEngines,
            sameModelResident: sameModelResident,
            hadDifferentCachedModel: hadDifferentCachedModel
        )
    }

    /// 关闭本地模型时，把同进程其他控制器上的本地模型一并卸掉
    func evictPeers(excluding requester: LocalModelLoader?, keepingEngine: IMEEngine? = nil) -> [IMEEngine] {
        entries.removeAll { $0.loader == nil && $0.engine == nil }
        var peerEngines: [IMEEngine] = []
        for entry in entries where entry.loader !== requester {
            let otherLoader = entry.loader
            let otherActive = otherLoader?.state == .loading || otherLoader?.state == .loaded
            if otherActive {
                otherLoader?.yieldToPeer()
            }
            if let otherEngine = entry.engine,
               otherEngine !== keepingEngine,
               (otherActive || otherEngine.isLocalModelLoaded),
               !peerEngines.contains(where: { $0 === otherEngine }) {
                peerEngines.append(otherEngine)
            }
        }
        return peerEngines
    }

    /// 收到系统内存警告时，把同进程其他控制器上的本地模型一并卸掉，并把原持有者标成 .unloadedForMemory
    func evictPeersForMemoryWarning(excluding requester: LocalModelLoader?, keepingEngine: IMEEngine? = nil) -> [IMEEngine] {
        entries.removeAll { $0.loader == nil && $0.engine == nil }
        var peerEngines: [IMEEngine] = []
        for entry in entries where entry.loader !== requester {
            let otherLoader = entry.loader
            let otherActive = otherLoader?.state == .loading || otherLoader?.state == .loaded
            if otherActive {
                otherLoader?.markUnloadedForMemory()
            }
            if let otherEngine = entry.engine,
               otherEngine !== keepingEngine,
               (otherActive || otherEngine.isLocalModelLoaded),
               !peerEngines.contains(where: { $0 === otherEngine }) {
                peerEngines.append(otherEngine)
            }
        }
        return peerEngines
    }

    fileprivate func releaseProcessCaches() {
        releaseCaches()
    }
}

/// 本地 GGUF 模型（llama.cpp，在键盘进程内用 mmap 从 App Group 只读加载）的装载与卸载。
///
/// 键盘扩展超过 177MB 就会被系统杀掉，模型常驻要 60–95MB，所以：
/// - 只在首帧之后、后台队列里加载；
/// - 换模型或跨控制器接力时，在串行队列上复用同一模型或先卸掉旧模型，再看 os_proc_available_memory()，低于门槛就不加载：
///   两份模型不会同时驻留，也不会因为旧模型还占着内存而误报「内存不足」；
/// - 收到内存警告立即卸载，本次键盘显示期间不再重试；
/// - 卸载和内存警告只要引擎里真有模型就会卸，不看自己记的状态（加载失败等情况下也不会把模型留在引擎里）。
/// 状态只在主线程读写；加载、卸载在同一个串行队列上排队，先后顺序有保证。
final class LocalModelLoader {
    /// 默认剩余可用内存门槛（目录外模型或测试用；ModelCatalog 里的模型按各自的 minimumAvailableMiB 计算）
    static let minimumAvailableBytes: UInt64 = 95 * 1024 * 1024

    static func minimumAvailableBytes(for url: URL) -> UInt64 {
        if let model = ModelCatalog.all.first(where: { $0.fileName == url.lastPathComponent }) {
            return UInt64(model.minimumAvailableMiB) * 1024 * 1024
        }
        return minimumAvailableBytes
    }

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

    private let coordinator: LocalModelCoordinator?
    private let queue: DispatchQueue
    private let availableMemory: () -> UInt64
    private let fileExists: (URL) -> Bool
    private(set) var loadedURL: URL?
    /// 每次加载 / 卸载加一，过期的加载结果据此丢弃
    private var generation = 0

    init(coordinator: LocalModelCoordinator? = nil,
         queue: DispatchQueue? = nil,
         availableMemory: @escaping () -> UInt64 = LocalModelLoader.systemAvailableMemory,
         fileExists: @escaping (URL) -> Bool = { FileManager.default.fileExists(atPath: $0.path) }) {
        self.coordinator = coordinator
        self.queue = queue ?? coordinator?.queue ?? DispatchQueue(label: "com.zengji.aipinyin.local-model", qos: .utility)
        self.availableMemory = availableMemory
        self.fileExists = fileExists
    }

    /// 进程内是否已经常驻同一个本地模型（本引擎已加载/加载中，或同进程其他控制器已加载可零耗时复用）
    func hasResidentModel(at url: URL, engine: IMEEngine) -> Bool {
        let target = url.standardizedFileURL
        if (state == .loading || state == .loaded), loadedURL?.standardizedFileURL == target {
            return true
        }
        return coordinator?.hasResidentModel(url: target) ?? false
    }

    func hasModelFile(at url: URL) -> Bool {
        fileExists(url)
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

    /// 键盘重新出现：清掉上次内存警告或内存不足的记录，允许再试
    func keyboardWillAppear() {
        if state == .unloadedForMemory || state == .skippedLowMemory { state = .idle }
    }

    /// 让位给同进程里刚激活的另一个控制器：清掉本控制器的加载记录，切回原宿主应用时再按需接回
    fileprivate func yieldToPeer() {
        generation += 1
        loadedURL = nil
        if state == .loading || state == .loaded {
            state = .idle
        }
    }

    /// 系统内存警告由同进程其他控制器先收到并触发统一卸载时，把原本持有模型的控制器也记成 .unloadedForMemory
    fileprivate func markUnloadedForMemory() {
        generation += 1
        loadedURL = nil
        state = .unloadedForMemory
    }

    private enum Outcome {
        case loaded, failed, lowMemory
    }

    /// 按需加载。url 为 nil 表示设置里还没选模型
    func load(_ url: URL?, into engine: IMEEngine) {
        guard state != .unloadedForMemory else { return }
        if state == .loading || state == .loaded, loadedURL?.standardizedFileURL == url?.standardizedFileURL {
            coordinator?.register(loader: self, engine: engine)
            return
        }
        guard let url, fileExists(url) else {
            unload(into: engine)
            state = .missing
            return
        }
        let claim = coordinator?.claimForLoad(by: self, engine: engine, url: url) ?? .init()
        let requiredBytes = Self.minimumAvailableBytes(for: url)
        // 引擎、同进程其他控制器以及进程级 SHARED_LLM 里都没有常驻模型时，现在量到的就是加载前的内存：不够就直接报
        if state != .loading,
           !engine.isLocalModelLoaded,
           !claim.sameModelResident,
           !claim.hadDifferentCachedModel,
           claim.peerEngines.isEmpty,
           availableMemory() < requiredBytes {
            KeyboardTrace.event("model.skippedLowMemory", "model=\(url.lastPathComponent) required=\(requiredBytes >> 20)")
            state = .skippedLowMemory
            return
        }
        generation += 1
        let current = generation
        loadedURL = url
        state = .loading
        let availableMemory = self.availableMemory
        let peerEngines = claim.peerEngines
        let sameModelResident = claim.sameModelResident
        let hadDifferentCachedModel = claim.hadDifferentCachedModel
        let coordinator = self.coordinator
        let modelName = url.lastPathComponent
        KeyboardTrace.event("model.loadQueued", "model=\(modelName) reuse=\(sameModelResident)")
        queue.async { [weak self, weak engine] in
            let start = KeyboardTrace.now
            let outcome: Outcome
            if let engine {
                if sameModelResident {
                    // 同进程旧控制器或进程级 SHARED_LLM 已常驻同一模型：直接装到新引擎上（零额外物理内存），再卸掉旧引擎引用
                    if engine.isLocalModelLoaded { engine.unloadLocalModel() }
                    let reused = (try? engine.loadLocalModel(at: url)) != nil
                    for peer in peerEngines where peer.isLocalModelLoaded { peer.unloadLocalModel() }
                    if reused {
                        outcome = .loaded
                    } else if availableMemory() < requiredBytes {
                        outcome = .lowMemory
                    } else {
                        outcome = (try? engine.loadLocalModel(at: url)) != nil ? .loaded : .failed
                    }
                } else {
                    // 换模型或首次加载：先卸掉旧的（含同进程其他控制器的旧模型及残留进程缓存）再量内存
                    for peer in peerEngines where peer.isLocalModelLoaded { peer.unloadLocalModel() }
                    if engine.isLocalModelLoaded { engine.unloadLocalModel() }
                    if hadDifferentCachedModel {
                        coordinator?.releaseProcessCaches()
                    }
                    if availableMemory() < requiredBytes {
                        outcome = .lowMemory
                    } else {
                        // 失败时引擎里不会留着模型（引擎先卸载旧的再加载）
                        outcome = (try? engine.loadLocalModel(at: url)) != nil ? .loaded : .failed
                    }
                }
            } else {
                for peer in peerEngines where peer.isLocalModelLoaded { peer.unloadLocalModel() }
                outcome = .failed
            }
            KeyboardTrace.event("model.loadFinished", "model=\(modelName) outcome=\(outcome) \(KeyboardTrace.ms(since: start))ms")
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

    /// 设置里关掉了本地模型（或换成了别的 AI 模式）：连同已销毁控制器留在 SHARED_LLM 里的缓存一并清空
    func unload(into engine: IMEEngine) {
        coordinator?.register(loader: self, engine: engine)
        let peerEngines = coordinator?.evictPeers(excluding: self, keepingEngine: engine) ?? []
        let active = state == .loading || state == .loaded
        let hasSharedCache = coordinator?.hasAnyCachedModel() ?? false
        guard active || engine.isLocalModelLoaded || !peerEngines.isEmpty || hasSharedCache else { return }
        enqueueUnload(engine, extraEngines: peerEngines, releaseCaches: hasSharedCache)
        if active { state = .idle }
    }

    /// 内存警告：立刻卸载（即便本控制器 engine 尚未创建，也要卸掉同进程其他控制器和静态缓存）
    func didReceiveMemoryWarning(engine: IMEEngine?) {
        KeyboardTrace.event("memory.warning", "state=\(state)")
        if let engine {
            coordinator?.register(loader: self, engine: engine)
        }
        let peerEngines = coordinator?.evictPeersForMemoryWarning(excluding: self, keepingEngine: engine) ?? []
        let selfHasModel = state == .loading || state == .loaded || (engine?.isLocalModelLoaded == true)
        if selfHasModel || !peerEngines.isEmpty {
            enqueueUnload(engine, extraEngines: peerEngines, releaseCaches: true)
            if selfHasModel {
                state = .unloadedForMemory
            }
        } else if let coordinator {
            queue.async { coordinator.releaseProcessCaches() }
        }
    }

    private func enqueueUnload(_ engine: IMEEngine?, extraEngines: [IMEEngine] = [], releaseCaches: Bool = false) {
        generation += 1
        loadedURL = nil
        let coordinator = self.coordinator
        // 和加载排在同一个队列上：正在加载的话，等它加载完再卸载
        queue.async { [weak engine] in
            for peer in extraEngines where peer.isLocalModelLoaded {
                peer.unloadLocalModel()
            }
            engine?.unloadLocalModel()
            if releaseCaches {
                coordinator?.releaseProcessCaches()
            }
        }
    }
}
