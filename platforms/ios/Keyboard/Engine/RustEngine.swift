import Foundation
import ImeFFI

/// 真正的输入法引擎：Rust 的 ime-session 状态机，经 C 接口（crates/ime-ffi）调用。
///
/// - 引擎内部自己加锁，这里的方法在主线程调用；loadLocalModel / unloadLocalModel 可以在后台队列调用。
/// - 引擎的回调发生在它的后台线程上，这里一律转回主线程再交给界面。
/// - 每次调用返回一段 JSON，由 IMEEngineJSON 解码（格式与 Tests/Fixtures 一致，只是候选更精简）。
final class RustEngine: IMEEngine {
    enum Failure: Error, LocalizedError {
        case creationFailed(String)
        case localModelFailed(String)

        var errorDescription: String? {
            switch self {
            case .creationFailed(let message): return "引擎创建失败：\(message)"
            case .localModelFailed(let message): return "本地模型加载失败：\(message)"
            }
        }
    }

    /// 回调用的中转对象。引擎线程只拿它的指针，真正访问 engine 都在主线程
    private final class CallbackBox {
        weak var engine: RustEngine?
    }

    private let handle: OpaquePointer
    private let box = CallbackBox()

    var onAsyncUpdate: (() -> Void)?
    var onChoiceCancel: ((UInt64) -> Void)?
    /// 设成非 nil 时在引擎里装上「宿主选词器」，引擎才会发请求；设回 nil 卸载
    var onChoiceRequest: ((IMEChoiceRequest) -> Void)? {
        didSet {
            let wanted = onChoiceRequest != nil
            if wanted != (oldValue != nil) { registerChooser(wanted) }
        }
    }

    /// - Parameters:
    ///   - lexiconURL: AIMELX03 词库文件。只读 mmap，不占键盘的内存配额
    ///   - userDictionaryURL: 个人词频文件，必须在键盘自己能写的目录里；nil 表示只留在内存
    init(lexiconURL: URL, userDictionaryURL: URL?) throws {
        // 头文件和静态库都来自 scripts/build-rust.sh 打的同一个 xcframework；对不上说明打包出了错
        guard ime_ffi_abi_version() == IME_FFI_ABI_VERSION else {
            throw Failure.creationFailed("C 接口版本不符：库是 \(ime_ffi_abi_version())，头文件是 \(IME_FFI_ABI_VERSION)")
        }
        var errorMessage: UnsafeMutablePointer<CChar>?
        let created: OpaquePointer? = lexiconURL.path.withCString { lexicon in
            if let userDictionaryURL {
                return userDictionaryURL.path.withCString { ime_engine_new(lexicon, $0, &errorMessage) }
            }
            return ime_engine_new(lexicon, nil, &errorMessage)
        }
        guard let created else {
            let message = errorMessage.map { String(cString: $0) } ?? "未知错误"
            ime_string_free(errorMessage)
            throw Failure.creationFailed(message)
        }
        handle = created
        box.engine = self
        ime_engine_set_ai_notify(handle, { user in
            guard let user else { return }
            let box = Unmanaged<CallbackBox>.fromOpaque(user).takeUnretainedValue()
            DispatchQueue.main.async { box.engine?.onAsyncUpdate?() }
        }, Unmanaged.passUnretained(box).toOpaque())
    }

    deinit {
        // 取消回调的调用会等正在执行的回调结束，之后引擎线程不会再碰 box
        ime_engine_set_ai_notify(handle, nil, nil)
        ime_engine_set_chooser(handle, nil, nil)
        ime_engine_free(handle)
    }

    private func registerChooser(_ install: Bool) {
        guard install else {
            ime_engine_set_chooser(handle, nil, nil)
            return
        }
        ime_engine_set_chooser(handle, { user, id, json in
            guard let user else { return }
            let box = Unmanaged<CallbackBox>.fromOpaque(user).takeUnretainedValue()
            guard let json else {
                // json 为 nil 是「编号 id 的请求作废了」（输入结束、被新请求取代、换了后端）：
                // 转给 onChoiceCancel，选词器据此中止还在跑的那次推理，不必等它答完或超时
                DispatchQueue.main.async { box.engine?.onChoiceCancel?(id) }
                return
            }
            // json 只在回调期间有效，先拷出来
            let data = Data(bytes: json, count: strlen(json))
            DispatchQueue.main.async {
                guard let engine = box.engine,
                      let request = try? IMEEngineJSON.decodeChoiceRequest(data) else { return }
                engine.onChoiceRequest?(request)
            }
        }, Unmanaged.passUnretained(box).toOpaque())
    }

    // MARK: - IMEEngine

    func handle(_ key: IMEKey) -> IMEResult {
        switch key {
        case .character(let character):
            let scalars = character.unicodeScalars
            guard scalars.count == 1, let scalar = scalars.first else { return .passThrough }
            return result(ime_engine_key(handle, Int32(IME_KEY_CHAR), scalar.value))
        case .space: return send(IME_KEY_SPACE)
        case .enter: return send(IME_KEY_ENTER)
        case .backspace: return send(IME_KEY_BACKSPACE)
        case .toggleChineseEnglish: return send(IME_KEY_TOGGLE_ENGLISH)
        case .escape: return send(IME_KEY_ESCAPE)
        case .pageUp: return send(IME_KEY_PAGE_UP)
        case .pageDown: return send(IME_KEY_PAGE_DOWN)
        case .cursorLeft: return send(IME_KEY_CURSOR_LEFT)
        case .cursorRight: return send(IME_KEY_CURSOR_RIGHT)
        }
    }

    func selectCandidate(at globalIndex: Int) -> IMEResult {
        guard globalIndex >= 0 else { return .update(currentView()) }
        return result(ime_engine_select_candidate(handle, UInt32(clamping: globalIndex)))
    }

    func candidates(from offset: Int, count: Int) -> [IMECandidate] {
        guard offset >= 0, count > 0,
              let data = take(ime_engine_candidates(handle, UInt32(clamping: offset), UInt32(clamping: count))) else {
            return []
        }
        do {
            return try IMEEngineJSON.decodeCandidateRange(data).candidates
        } catch {
            assertionFailure("引擎返回的候选无法解码：\(error)")
            return []
        }
    }

    func tapSyllable(at index: Int) -> IMEResult {
        guard index >= 0 else { return .update(currentView()) }
        return result(ime_engine_tap_syllable(handle, UInt32(clamping: index)))
    }

    func commitFullSentence() -> IMEResult {
        result(ime_engine_commit_full_sentence(handle))
    }

    func currentView() -> IMEView {
        view(ime_engine_view(handle))
    }

    func refreshAfterAI() -> IMEView {
        view(ime_engine_refresh_after_ai(handle))
    }

    func setContext(_ textBeforeCursor: String?) {
        ime_engine_set_context(handle, textBeforeCursor)
    }

    func reset() {
        ime_engine_reset(handle)
    }

    var isComposing: Bool { ime_engine_is_composing(handle) }
    var isEnglishMode: Bool { ime_engine_is_english(handle) }

    func deliverChoice(id: UInt64, text: String?) {
        // 采纳时引擎会触发 onAsyncUpdate；过期、超时、不在候选里的答案引擎自己丢弃
        _ = ime_engine_deliver_choice(handle, id, text)
    }

    func apply(_ settings: IMESettings) {
        let f = settings.fuzzy
        let json = """
        {"fuzzy":{"z_zh":\(f.zZh),"c_ch":\(f.cCh),"s_sh":\(f.sSh),"n_l":\(f.nL),"f_h":\(f.fH),\
        "in_ing":\(f.inIng),"en_eng":\(f.enEng),"an_ang":\(f.anAng),"typo_correction":\(f.typoCorrection)},\
        "emoji":\(settings.emojiCandidates)}
        """
        let status = ime_engine_configure(handle, json)
        assert(status == 0, "引擎不接受设置：\(json)")
    }

    @discardableResult
    func reloadUserDictionaryIfChanged() -> Bool {
        // 1 = 重新读入，0 = 没变或没启用持久化，-1 = 读取失败（引擎保留内存里的词库）
        ime_engine_reload_user_dict(handle) == 1
    }

    /// 这个构建是否带本地模型支持（Rust 库编译时是否启用了 llama）
    static var supportsLocalModel: Bool { ime_llm_supported() }

    /// 在调用线程上加载（几十到几百毫秒），不占引擎的锁；调用方放到后台队列。
    /// 已经装有模型时引擎先卸载旧的再加载，加载失败之后引擎里没有模型
    func loadLocalModel(at url: URL) throws {
        var errorMessage: UnsafeMutablePointer<CChar>?
        let status = url.path.withCString { ime_engine_load_llm(handle, $0, 0, &errorMessage) }
        guard status == 0 else {
            let message = errorMessage.map { String(cString: $0) } ?? "未知错误"
            ime_string_free(errorMessage)
            throw Failure.localModelFailed(message)
        }
    }

    func unloadLocalModel() {
        ime_engine_unload_llm(handle)
    }

    var isLocalModelLoaded: Bool { ime_engine_llm_loaded(handle) }

    /// 最近一次按键是否还在等本地模型 / 宿主选词的结果。结果被采纳（随即 onAsyncUpdate）、本地模型打分失败、
    /// 宿主答复 nil 或答复不在候选里、请求作废（新的按键不需要结果、reset、上屏、加载 / 卸载模型；
    /// 等的是宿主选词时，装卸选词器也算）之后变回 false；宿主选词请求发出后超过 4 秒还没有答复也变回 false。
    /// 同一输入之后再重排（refreshAfterAI、移动光标）会重新发出请求，又变回 true。
    /// 超过 4 秒才送达的答复一律丢弃：这次请求还登记着时得到 IME_CHOICE_TIMED_OUT；同一输入重新发出了请求、
    /// 输入变了 / 结束了（或者上面其他作废的情况）之后，旧请求作废（引擎发出作废通知，转给 onChoiceCancel），
    /// 此后再送达的答复得到 IME_CHOICE_STALE。细节见 ime_ffi.h 的 ime_engine_ai_pending。
    /// 界面用不着它（等 onAsyncUpdate 即可）；评测和测试用它判断什么时候读结果
    var aiPending: Bool { ime_engine_ai_pending(handle) }

    /// 把一段设置 JSON 直接交给引擎（格式见 ime_ffi.h 的 ime_engine_configure）。
    /// apply(_:) 覆盖不到的开关用它，例如评测时切换个人记忆的处理策略。引擎不接受时返回 false
    @discardableResult
    func configure(_ json: String) -> Bool {
        ime_engine_configure(handle, json) == 0
    }

    // MARK: - 诊断

    /// 键盘进程当前的内存占用（字节），也就是系统判定是否超限的那个数
    static var memoryFootprint: UInt64 { ime_phys_footprint() }

    // MARK: - 内部

    private func send(_ kind: Int) -> IMEResult {
        result(ime_engine_key(handle, Int32(kind), 0))
    }

    /// 把引擎返回的 C 字符串包成 Data（不拷贝），用完由引擎释放
    private func take(_ pointer: UnsafeMutablePointer<CChar>?) -> Data? {
        guard let pointer else { return nil }
        return Data(bytesNoCopy: pointer, count: strlen(pointer), deallocator: .custom { bytes, _ in
            ime_string_free(bytes.assumingMemoryBound(to: CChar.self))
        })
    }

    private func result(_ pointer: UnsafeMutablePointer<CChar>?) -> IMEResult {
        guard let data = take(pointer) else { return .passThrough }
        do {
            return try IMEEngineJSON.decodeResult(data)
        } catch {
            assertionFailure("引擎返回的 JSON 无法解码：\(error)")
            return .passThrough
        }
    }

    private func view(_ pointer: UnsafeMutablePointer<CChar>?) -> IMEView {
        guard let data = take(pointer) else { return .idle }
        do {
            return try IMEEngineJSON.decodeView(data)
        } catch {
            assertionFailure("引擎返回的视图无法解码：\(error)")
            return .idle
        }
    }
}
