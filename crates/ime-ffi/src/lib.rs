//! `ime-ffi`: 输入法引擎的 C 接口（给 iOS 键盘扩展等非 Rust 宿主用）
//!
//! 结构：
//! - [`ImeEngine`] 是安全的 Rust 封装：一把锁护住 `ime_session::KeySession`，所有方法都接受 `&self`，
//!   可以在任意线程调用。结果以 JSON 字符串返回（格式见 `view.rs` 与 `include/ime_ffi.h`）。
//! - `extern "C"` 函数只是对它的薄封装，另外负责指针检查和拦截 panic（panic 不能越过 FFI 边界）。
//!   唯一的例外是 `ime_engine_free`：它销毁引擎，不能与同一引擎上的任何其他调用并发。
//!
//! 经由本封装使用时引擎不发起网络请求。调度器里 Windows 版用的 HTTP 路径只在「大模型重排」开关
//! （`cloud_client.config.enabled`）打开且没有进程内后端时才走；本封装只在装有本地模型或宿主选词器时
//! 才打开这个开关（见 `sync_llm_switch`），而装有它们时调度器不走 HTTP。
//! 环境变量：创建引擎时 `OpenAiCompatibleConfig::default()` 会读 `IME_OPENAI_API_BASE`、
//! `IME_OPENAI_API_KEY`、`IME_OPENAI_MODEL` 作为 HTTP 路径的默认配置，按上面所说这里用不到它们。
//! 文件只有三处：只读 mmap 的词库、（可选的）个人词库 JSON（后台线程读写，见 `ime_core`），
//! 以及（可选的）只读 mmap 的本地模型。
//!
//! # 触屏宿主与分页
//!
//! `KeySession` 的翻页状态是给桌面候选窗用的（数字键按页内位置选词）。触屏键盘的候选栏是一条可以一直
//! 滚动的列表，高亮的始终是全部候选里的第 0 个，所以这里约定引擎停在第 0 页：
//! 「加载更多」用不改状态的 [`ImeEngine::candidates`]；空格（[`IME_KEY_SPACE`]）和
//! [`ImeEngine::refresh_after_ai`] 都先把翻页位置归零。`IME_KEY_PAGE_UP` / `IME_KEY_PAGE_DOWN` 仍然可用，
//! 但只影响视图里返回哪一页。
//!
//! # AI 选词
//!
//! 规则重排始终开启，在按键路径上同步完成。除此之外可以再接一个「大模型」后端，结果异步到达：
//! - 本地模型（[`ImeEngine::load_llm`]）：llama.cpp 在本进程内给候选打分，需要 `llama` 特性；
//! - 宿主选词器（[`ImeEngine::set_chooser`]）：引擎把上文、拼音和候选交给宿主（iOS 上是 Apple 的端侧模型），
//!   宿主选好后用 [`ImeEngine::deliver_choice`] 送回。
//! 两者都装上时只用本地模型。结果到达时，当前输入若还在等它，就触发 [`ImeEngine::set_notify`] 注册的回调；
//! 输入已经变了或结束了则只记进缓存，不通知。

mod view;

use ime_ai::{
    ChooserDelivery, ChooserEvent, ChooserHandle, ChooserMemoryPolicy, DualModeAiOrchestrator,
    HostChooser, OpenAiCompatibleConfig,
};
use ime_core::{AppScene, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use ime_llm::{LlmOptions, LlmScorer};
use ime_session::{KeyAction, KeySession, VirtualKey};
use serde::Deserialize;
use std::ffi::{c_char, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 接口版本。2：新增 `ime_engine_candidates`、`ime_engine_reload_user_dict`；空格与
/// `ime_engine_refresh_after_ai` 先回到第 0 页；正在输入时整句上屏 / 空格 / 光标键不再出现
/// 「什么也没做」或 pass；`ime_engine_load_llm` 先卸载已有模型；拼音串有长度上限
pub const IME_FFI_ABI_VERSION: i32 = 2;

pub const IME_KEY_CHAR: i32 = 0;
pub const IME_KEY_SPACE: i32 = 1;
pub const IME_KEY_ENTER: i32 = 2;
pub const IME_KEY_BACKSPACE: i32 = 3;
pub const IME_KEY_TOGGLE_ENGLISH: i32 = 4;
pub const IME_KEY_ESCAPE: i32 = 5;
pub const IME_KEY_PAGE_UP: i32 = 6;
pub const IME_KEY_PAGE_DOWN: i32 = 7;
pub const IME_KEY_CURSOR_LEFT: i32 = 8;
pub const IME_KEY_CURSOR_RIGHT: i32 = 9;

/// 触屏键盘的候选栏可以横向滚动，一页多给一些
pub const DEFAULT_PAGE_SIZE: usize = 30;

/// [`ImeEngine::candidates`] 一次最多返回的候选数
pub const MAX_CANDIDATES_PER_CALL: usize = 200;

/// 拼音串的长度上限（字节）。到了上限，再输入的字母和分词符被吃掉、视图不变。
/// 每次按键都要把整串重新解码：实测 80 个字母约 5 ms/键，120 个约 12 ms/键、瞬时内存 26 MiB，
/// 240 个约 38 ms/键、66 MiB——正常输入到不了这么长，上限只是不让乱按拖垮键盘进程
pub const MAX_COMPOSITION_BYTES: usize = 100;

/// `load_llm` 替换模型时，等旧模型在途的打分放手的最长时间
const LLM_RELEASE_WAIT: Duration = Duration::from_secs(2);

/// [`ImeEngine::deliver_choice`] 的返回值
pub const IME_CHOICE_ACCEPTED: i32 = 0;
pub const IME_CHOICE_NO_ANSWER: i32 = 1;
pub const IME_CHOICE_IMPLAUSIBLE: i32 = 2;
pub const IME_CHOICE_STALE: i32 = 3;
pub const IME_CHOICE_TIMED_OUT: i32 = 4;

type NotifyFn = Arc<dyn Fn() + Send + Sync>;

/// 宿主选词器收到的事件
pub enum ChoiceEvent<'a> {
    /// 请从 `candidates` 里选一个，用 [`ImeEngine::deliver_choice`] 送回
    Request {
        id: u64,
        context: &'a str,
        pinyin: &'a str,
        candidates: &'a [String],
    },
    /// 编号为 `id` 的请求已经作废，可以中止对应的计算
    Cancel { id: u64 },
}

type ChooserFn = Arc<dyn Fn(ChoiceEvent<'_>) + Send + Sync>;

/// 宿主注册的回调。调用回调时一直持有对应的锁，所以「换掉回调」的调用返回之后，旧回调一定不会再被调用
#[derive(Default)]
struct Callbacks {
    notify: Mutex<Option<NotifyFn>>,
    /// 选词回调和它的安装编号。调度器里每个 `HostChooser` 只认自己那次安装的编号：
    /// 已经被换掉的选词器迟到的事件不会落到新回调头上
    chooser: Mutex<Option<(u64, ChooserFn)>>,
    next_chooser_installation: AtomicU64,
}

impl Callbacks {
    fn fire_notify(&self) {
        let guard = lock_ignoring_poison(&self.notify);
        if let Some(cb) = guard.as_ref() {
            cb();
        }
    }

    /// 把 `installation` 那次安装的选词器发出的事件交给宿主；回调已被卸载或换成别的安装时丢弃
    fn fire_chooser(&self, installation: u64, event: &ChooserEvent) {
        let guard = lock_ignoring_poison(&self.chooser);
        let Some((current, cb)) = guard.as_ref() else {
            return;
        };
        if *current != installation {
            return;
        }
        match event {
            ChooserEvent::Request(request) => cb(ChoiceEvent::Request {
                id: request.id,
                context: &request.context,
                pinyin: &request.pinyin,
                candidates: &request.candidates,
            }),
            ChooserEvent::Cancel { id } => cb(ChoiceEvent::Cancel { id: *id }),
        }
    }
}

fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `ime_engine_configure` 接受的设置。字段都可省略，省略的保持不变
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsIn {
    fuzzy: Option<FuzzyIn>,
    page_size: Option<usize>,
    scene: Option<AppScene>,
    /// 上文相关的纠正记忆（默认开）
    context_memory: Option<bool>,
    /// 个人记忆首选是否连本地模型也挡住（默认否；评测对照用）
    memory_blocks_model: Option<bool>,
    /// 宿主选词器遇到个人记忆首选时：blocked | contested | always | taught（iOS 默认 taught）
    chooser_memory: Option<ChooserMemoryPolicy>,
    /// 常用词候选后面跟上对应的表情（默认开）
    emoji: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FuzzyIn {
    z_zh: Option<bool>,
    c_ch: Option<bool>,
    s_sh: Option<bool>,
    n_l: Option<bool>,
    f_h: Option<bool>,
    in_ing: Option<bool>,
    en_eng: Option<bool>,
    an_ang: Option<bool>,
    typo_correction: Option<bool>,
}

/// 输入法引擎：词库 + 解码器 + 按键状态机 + AI 重排调度
pub struct ImeEngine {
    session: Mutex<KeySession>,
    callbacks: Arc<Callbacks>,
    /// 送回宿主选词结果用，不经过 `session` 的锁
    chooser_handle: ChooserHandle,
    /// 镜像 `KeySession` 里的两个状态，查询时不用拿锁
    composing: AtomicBool,
    english: AtomicBool,
    /// 下面两个标志与调度器里实际装着的后端的关系：标志为真时后端一定已经装上。
    /// 安装时先装后端再置标志，卸载时先清标志（并关掉大模型开关）再动调度器，
    /// 所以中途无论在哪里 panic，都不会出现「开关开着却没有后端」
    llm_loaded: AtomicBool,
    chooser_installed: AtomicBool,
    /// 已安装的本地模型，与交给调度器的是同一个 `Arc`。这里留一份，是为了卸载 / 替换时
    /// 最后一个引用在 `session` 的锁之外释放（释放模型要几到十几毫秒，不能让按键等它）
    llm: Mutex<Option<Arc<LlmScorer>>>,
    /// 同一时刻只让一个 `load_llm` 进行，免得两个线程各自载入一份模型
    llm_loading: Mutex<()>,
}

impl ImeEngine {
    /// 打开词库并创建引擎。`user_dict_path` 为 `None` 时个人词频只留在内存里
    pub fn open(lexicon_path: &Path, user_dict_path: Option<&Path>) -> Result<Self, String> {
        let lexicon = CompiledLexicon::load_mmap(lexicon_path)
            .map_err(|e| format!("无法加载词库 {}: {e}", lexicon_path.display()))?;
        spawn_page_warmup(lexicon.clone());

        let mut core = ImeCoreEngine::new(lexicon);
        if let Some(path) = user_dict_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            core.enable_persistence(path.to_path_buf());
        }

        let ai = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
        let callbacks = Arc::new(Callbacks::default());
        let for_ai = Arc::clone(&callbacks);
        ai.set_async_notify_callback(Arc::new(move || for_ai.fire_notify()));

        let chooser_handle = ai.chooser_handle();

        let mut session = KeySession::new(core, ai);
        session.set_page_size(DEFAULT_PAGE_SIZE);
        // 触屏键盘没有 Tab 键，续写建议目前也没有界面
        session.ghost_text_enabled = false;
        // 用户在某段上文下主动改选过的词，下次同样的上文直接排首位（Windows 版默认不开，这里打开）
        session.context_memory_enabled = true;
        // 像系统键盘那样，常用词候选后面跟上对应的表情（打 mao，「猫」后面是 🐈 🐱）
        session.emoji_candidates_enabled = true;
        // 触屏的音节选字：点音节选完字回到整句，不自动跳到别的音节；点着选定的音节按退格撤销那里的选字
        session.touch_syllable_editing = true;
        // 宿主选词器（Apple 端侧模型）给不出分数，没法和个人记忆一起裁决：用户主动改选过的词不让它换掉，
        // 只是空格接受养成的记忆照样请它裁决（取值依据见 ChooserMemoryPolicy 的文档）
        session.ai.chooser_memory_policy = ChooserMemoryPolicy::Taught;

        Ok(Self {
            session: Mutex::new(session),
            callbacks,
            chooser_handle,
            composing: AtomicBool::new(false),
            english: AtomicBool::new(false),
            llm_loaded: AtomicBool::new(false),
            chooser_installed: AtomicBool::new(false),
            llm: Mutex::new(None),
            llm_loading: Mutex::new(()),
        })
    }

    /// 拿到会话。上一次调用若在持锁期间 panic，状态可能不一致：丢掉当前输入，
    /// 把镜像标志和大模型开关按现状重新对齐，再继续
    fn session(&self) -> MutexGuard<'_, KeySession> {
        match self.session.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.cancel_composition();
                self.sync_flags(&guard);
                self.sync_llm_switch(&mut guard);
                self.session.clear_poison();
                guard
            }
        }
    }

    fn sync_flags(&self, session: &KeySession) {
        self.composing.store(session.is_composing(), Ordering::Release);
        self.english
            .store(session.english_passthrough_mode, Ordering::Release);
    }

    fn finish(&self, session: &KeySession, action: KeyAction) -> String {
        self.sync_flags(session);
        if !session.is_composing() && self.llm_backend_active() {
            // 输入已经结束（上屏或取消），还在算的结果没人要了
            session.ai.cancel_pending_llm_work();
        }
        match &action {
            KeyAction::EatAndCommit { .. } => {
                view::action_json(&action, &session.build_view_model())
            }
            // 另外两种情况用不到「之后的视图」
            _ => view::action_json(&action, &Default::default()),
        }
    }

    /// 处理一次按键，返回信封 JSON
    pub fn key(&self, kind: i32, value: u32) -> String {
        let mut session = self.session();
        let action = match kind {
            IME_KEY_CHAR => match char::from_u32(value) {
                Some(ch) => Self::type_char(&mut session, ch),
                None => KeyAction::PassThrough,
            },
            IME_KEY_SPACE => {
                // 触屏宿主翻页只是为了多取候选，界面上高亮的始终是全部候选里的第 0 个：
                // 空格一律提交它，而不是「引擎停着的那一页」的第一个
                session.current_page = 0;
                session.selected_candidate_index = 0;
                if session.is_composing() && session.current_candidates.is_empty() {
                    // 没有候选可提交（见 `commit_full_sentence`）：退回原样上屏，正在输入的引擎一定能上屏
                    session.process_key(VirtualKey::Enter)
                } else {
                    session.process_key(VirtualKey::Space)
                }
            }
            IME_KEY_ENTER => session.process_key(VirtualKey::Enter),
            IME_KEY_BACKSPACE => session.process_key(VirtualKey::Backspace),
            IME_KEY_TOGGLE_ENGLISH => match session.process_key(VirtualKey::Shift) {
                // 空闲时切换：状态机返回「放行」，对宿主来说按键已经处理完了
                KeyAction::PassThrough => KeyAction::EatAndUpdateUi(session.build_view_model()),
                other => other,
            },
            IME_KEY_ESCAPE => session.process_key(VirtualKey::Escape),
            IME_KEY_PAGE_UP => session.process_key(VirtualKey::PageUp),
            IME_KEY_PAGE_DOWN => session.process_key(VirtualKey::PageDown),
            IME_KEY_CURSOR_LEFT => Self::move_cursor(&mut session, VirtualKey::Left),
            // 状态机里 → 是逐字母移动、Ctrl+→ 才是按音节；触屏上按音节移动更合用
            IME_KEY_CURSOR_RIGHT => Self::move_cursor(&mut session, VirtualKey::CtrlRight),
            _ => KeyAction::PassThrough,
        };
        self.finish(&session, action)
    }

    /// 光标键：正在输入时一定由引擎吃掉。状态机在没有音节可移动时（解码不出任何候选）返回「放行」，
    /// 宿主若把这个键再交给输入框，光标就会跑到还没上屏的拼音外面去
    fn move_cursor(session: &mut KeySession, key: VirtualKey) -> KeyAction {
        match session.process_key(key) {
            KeyAction::PassThrough if session.is_composing() => {
                KeyAction::EatAndUpdateUi(session.build_view_model())
            }
            other => other,
        }
    }

    fn type_char(session: &mut KeySession, ch: char) -> KeyAction {
        if session.english_passthrough_mode {
            return KeyAction::PassThrough;
        }
        if ch.is_ascii_alphabetic() {
            // 空闲时的大写字母直接上屏（英文缩写、人名首字母）；输入中途的大写保留给解码器，
            // 它用大小写判断中英混输的词边界（如 wozaiyongRust）
            if ch.is_ascii_uppercase() && !session.is_composing() {
                return KeyAction::PassThrough;
            }
            return Self::extend_composition(session, ch);
        }
        // 单引号是手动分词符，只在输入中途有意义
        if ch == '\'' && session.is_composing() {
            return Self::extend_composition(session, ch);
        }
        // 标点和数字由宿主处理：状态机会把它们并进拼音串
        KeyAction::PassThrough
    }

    /// 把字母或分词符并进拼音串；已到 [`MAX_COMPOSITION_BYTES`] 时吃掉按键、视图不变
    fn extend_composition(session: &mut KeySession, ch: char) -> KeyAction {
        if session.input_buffer.len() >= MAX_COMPOSITION_BYTES {
            return KeyAction::EatAndUpdateUi(session.build_view_model());
        }
        session.process_key(VirtualKey::Char(ch))
    }

    /// 点选候选（全部候选里的下标）
    pub fn select_candidate(&self, global_index: usize) -> String {
        let mut session = self.session();
        let action = session.select_candidate(global_index);
        self.finish(&session, action)
    }

    /// 点击预编辑栏里的音节；再点一次同一个音节回到整句
    pub fn tap_syllable(&self, index: usize) -> String {
        let mut session = self.session();
        if !session.is_composing() {
            return self.finish(&session, KeyAction::PassThrough);
        }
        if session.active_syllable_index() == Some(index) {
            session.return_cursor_to_end();
        } else {
            session.click_pinyin_syllable(index, None);
        }
        let view = session.build_view_model();
        self.finish(&session, KeyAction::EatAndUpdateUi(view))
    }

    /// 把当前整句预览上屏。没有输入时放行；正在输入时一定上屏：
    /// 解码不出整句时（在已选定的多音节片段里再次改选之后可能出现「没有任何候选」），
    /// 退回「已选定的词 + 剩余字母原样上屏」，与回车相同
    pub fn commit_full_sentence(&self) -> String {
        let mut session = self.session();
        if !session.is_composing() {
            return self.finish(&session, KeyAction::PassThrough);
        }
        let action = if session.last_full_sentence.is_empty() {
            session.process_key(VirtualKey::Enter)
        } else {
            session.commit_full_sentence()
        };
        self.finish(&session, action)
    }

    /// 当前视图（不带信封）
    pub fn view(&self) -> String {
        let session = self.session();
        view::view_json(&session.build_view_model())
    }

    /// 全部候选里的 `[offset, offset + count)`，不改变引擎的分页状态。`count` 最多
    /// [`MAX_CANDIDATES_PER_CALL`]。格式见 `include/ime_ffi.h`；没有输入时 `total` 为 0
    pub fn candidates(&self, offset: usize, count: usize) -> String {
        let session = self.session();
        let all: &[ime_core::Candidate] = if session.is_composing() {
            &session.current_candidates
        } else {
            &[]
        };
        view::candidates_json(all, offset, count.min(MAX_CANDIDATES_PER_CALL))
    }

    /// 异步 AI 结果到达后重排候选，返回新视图。引擎先回到第 0 页：状态机在「用户正在翻页」时
    /// 不重排（桌面上免得数字键选错词），而触屏宿主的翻页只是多取了候选，结果必须照常应用
    pub fn refresh_after_ai(&self) -> String {
        let mut session = self.session();
        session.current_page = 0;
        session.selected_candidate_index = 0;
        let vm = session.refresh_candidates_after_ai();
        self.sync_flags(&session);
        view::view_json(&vm)
    }

    /// 个人词库文件若被别的键盘进程改过（修改时间或大小与本引擎上次读写时不同）就重新读入。
    /// 返回是否重新读入了；没有变化或没有启用持久化时为 `Ok(false)`；读取失败时返回错误，
    /// 内存里的词库保持不变。正在输入时会按新词库重算候选。
    ///
    /// 在调用线程上做文件 I/O：通常只是一次 `stat`；真要重新读入时，20,000 条记录的词库约 15 ms
    pub fn reload_user_dict(&self) -> Result<bool, String> {
        let mut session = self.session();
        let reloaded = session
            .core
            .reload_user_dict_if_changed()
            .map_err(|e| format!("个人词库读取失败: {e}"))?;
        if reloaded {
            session.refresh_candidates();
            self.sync_flags(&session);
        }
        Ok(reloaded)
    }

    /// 宿主输入框里光标前的文本；`None` 表示拿不到
    pub fn set_context(&self, text_before_cursor: Option<&str>) {
        let mut session = self.session();
        match text_before_cursor {
            Some(text) => session.set_context_before_cursor(text),
            None => session.clear_context_before_cursor(),
        }
    }

    /// 放弃当前输入，不上屏
    pub fn reset(&self) {
        let mut session = self.session();
        session.cancel_composition();
        if self.llm_backend_active() {
            session.ai.cancel_pending_llm_work();
        }
        self.sync_flags(&session);
    }

    pub fn is_composing(&self) -> bool {
        // 上一次调用 panic 过：下一次拿会话时当前输入会被丢掉（见 `session`），这里已经不算在输入
        !self.session.is_poisoned() && self.composing.load(Ordering::Acquire)
    }

    pub fn is_english(&self) -> bool {
        self.english.load(Ordering::Acquire)
    }

    /// 应用设置（模糊音、页大小、场景），格式见 `include/ime_ffi.h`
    pub fn configure(&self, settings_json: &str) -> Result<(), String> {
        let settings: SettingsIn =
            serde_json::from_str(settings_json).map_err(|e| format!("设置 JSON 无法解析: {e}"))?;
        let mut session = self.session();
        if let Some(fuzzy) = settings.fuzzy {
            let config = &mut session.core.fuzzy_config;
            let mut changed = false;
            let mut set = |slot: &mut bool, value: Option<bool>| {
                if let Some(v) = value {
                    changed |= *slot != v;
                    *slot = v;
                }
            };
            set(&mut config.z_zh, fuzzy.z_zh);
            set(&mut config.c_ch, fuzzy.c_ch);
            set(&mut config.s_sh, fuzzy.s_sh);
            set(&mut config.n_l, fuzzy.n_l);
            set(&mut config.f_h, fuzzy.f_h);
            set(&mut config.in_ing, fuzzy.in_ing);
            set(&mut config.en_eng, fuzzy.en_eng);
            set(&mut config.an_ang, fuzzy.an_ang);
            set(&mut config.typo_correction, fuzzy.typo_correction);
            if changed {
                session.refresh_candidates();
            }
        }
        if let Some(page_size) = settings.page_size {
            session.set_page_size(page_size);
        }
        if let Some(scene) = settings.scene {
            session.set_scene(scene);
        }
        let mut policy_changed = false;
        if let Some(enabled) = settings.context_memory {
            policy_changed |= session.context_memory_enabled != enabled;
            session.context_memory_enabled = enabled;
        }
        if let Some(blocks) = settings.memory_blocks_model {
            policy_changed |= session.ai.memory_blocks_scorer != blocks;
            session.ai.memory_blocks_scorer = blocks;
        }
        if let Some(policy) = settings.chooser_memory {
            policy_changed |= session.ai.chooser_memory_policy != policy;
            session.ai.chooser_memory_policy = policy;
        }
        if let Some(enabled) = settings.emoji {
            policy_changed |= session.emoji_candidates_enabled != enabled;
            session.emoji_candidates_enabled = enabled;
        }
        if policy_changed {
            session.refresh_candidates();
        }
        self.sync_flags(&session);
        Ok(())
    }

    /// 注册「异步 AI 结果就绪」回调。本地模型的结果在引擎的后台线程上触发它，
    /// 宿主选词的结果在调用 [`Self::deliver_choice`] 的线程上触发它（宿主不应依赖具体是哪个线程）。
    /// 返回之后旧回调不会再被调用
    pub fn set_notify(&self, callback: Option<NotifyFn>) {
        *lock_ignoring_poison(&self.callbacks.notify) = callback;
    }

    // ------------------------------------------------------------------
    // AI 后端
    // ------------------------------------------------------------------

    fn llm_backend_active(&self) -> bool {
        self.llm_loaded.load(Ordering::Acquire) || self.chooser_installed.load(Ordering::Acquire)
    }

    /// 调度器只在「大模型重排」开关打开时才用打分器 / 选词器；两者都没装时必须关掉，
    /// 否则它会退回 Windows 版的 HTTP 路径（去连本机的 llama-server）
    fn sync_llm_switch(&self, session: &mut KeySession) {
        session.ai.cloud_client.config.enabled = self.llm_backend_active();
    }

    /// 这个构建是否包含进程内本地模型支持（`llama` 特性）
    pub fn llm_supported() -> bool {
        LlmScorer::is_supported()
    }

    /// 加载本地 GGUF 模型并用它给候选打分。模型文件只读 mmap，权重不占进程内存配额。
    /// 加载在调用线程上进行（几十到几百毫秒），期间不占用引擎的锁，按键不受影响。
    /// `threads` 为 0 表示自动（性能核数量，最多 4）。
    ///
    /// 已经装有模型时先把它卸载并释放，再加载新的，两份模型不会同时驻留（以前是先加载后替换：
    /// Mac 上实测切换瞬间约 134 MiB，单个模型约 66 MiB）。因此加载失败之后引擎里没有模型，
    /// 不会留着旧的。旧模型若正在打分，最多等 2 秒让那次打分注意到取消并放手。
    /// 并发的 `load_llm` 排队执行，最后完成的那个生效
    pub fn load_llm(&self, model_path: &Path, threads: u32) -> Result<(), String> {
        let _loading = lock_ignoring_poison(&self.llm_loading);
        // 没装模型时不去动调度器：否则当前输入正在等的宿主选词结果到达时就不再通知了
        let installed = lock_ignoring_poison(&self.llm).is_some();
        if let Some(old) = installed.then(|| self.detach_llm()).flatten() {
            // 在途的打分自己还持有一个引用。等到这里是最后一个引用再释放，
            // 下面开始加载时旧模型就已经不占内存了
            let deadline = Instant::now() + LLM_RELEASE_WAIT;
            while Arc::strong_count(&old) > 1 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            drop(old);
        }
        let options = LlmOptions {
            threads: threads as _,
            ..LlmOptions::default()
        };
        let scorer = Arc::new(LlmScorer::load(model_path, options).map_err(|e| e.to_string())?);
        let mut session = self.session();
        *lock_ignoring_poison(&self.llm) = Some(Arc::clone(&scorer));
        session.ai.set_candidate_scorer(Some(scorer));
        self.llm_loaded.store(true, Ordering::Release);
        self.sync_llm_switch(&mut session);
        Ok(())
    }

    /// 把本地模型从调度器里拿掉（在途的打分被取消）。返回这里留着的那个引用，
    /// 调用方在本函数返回之后（`session` 的锁已经放开）再释放它
    fn detach_llm(&self) -> Option<Arc<LlmScorer>> {
        let mut session = self.session();
        // 先清标志、关开关，再动调度器（见 `llm_loaded` 字段的说明）
        self.llm_loaded.store(false, Ordering::Release);
        self.sync_llm_switch(&mut session);
        let detached = lock_ignoring_poison(&self.llm).take();
        session.ai.set_candidate_scorer(None);
        detached
    }

    /// 卸载本地模型（正在进行的打分会被取消）。
    ///
    /// 释放的是模型的上下文和各种缓冲区，在本函数里、引擎的锁之外完成（几到十几毫秒，按键不用等它）；
    /// 若此刻正有一次打分在进行，要等那次打分注意到取消后才在后台线程上释放，本函数不等它
    /// （Mac 上实测卸载返回时 84–88 MiB，0.5–2 秒后回落）。
    /// phys_footprint 不会回到加载前的水平：Mac 上实测基线 3–6 MiB，加载后约 63 MiB，
    /// 第一次卸载后约 17 MiB，反复加载 / 卸载几轮后稳定在约 41–46 MiB。留下的是分配器没有还给系统的内存，
    /// 下次加载会复用，不会无限增长。权重本身是只读 mmap，从来不计入 phys_footprint
    pub fn unload_llm(&self) {
        let detached = self.detach_llm();
        drop(detached);
    }

    pub fn is_llm_loaded(&self) -> bool {
        self.llm_loaded.load(Ordering::Acquire)
    }

    /// 最近一次按键是否还在等本地模型 / 宿主选词的结果。结果被采纳（随即通知）、打分失败、宿主答复没有结果
    /// 或答复不在候选里、请求作废（新的按键不需要结果、输入结束、装卸模型；等的是宿主选词时装卸选词器也算）
    /// 之后变回 `false`；宿主选词请求超过等待时限（4 秒）还没有答复时也变回 `false`。同一输入之后再重排
    /// （如 [`Self::refresh_after_ai`]）会重新发出请求，又变回 `true`。
    ///
    /// 超时之后再送达的答复一律丢弃：这次请求还登记着时得到 `IME_CHOICE_TIMED_OUT`；同一输入重新发出了请求、
    /// 输入变了 / 结束了（或者上面其他作废的情况）之后，旧请求作废，宿主收到它的作废通知（[`ChoiceEvent::Cancel`]），
    /// 此后再送达的答复得到 `IME_CHOICE_STALE`。
    /// 评测工具和测试用它来等结果；界面不需要它，等回调即可
    pub fn ai_result_pending(&self) -> bool {
        self.session().ai.inprocess_result_pending()
    }

    /// 安装（`Some`）或卸载（`None`）宿主选词器。回调在引擎的后台线程或调用本接口的线程上执行，
    /// 应当立刻返回。返回之后旧回调不会再被调用。
    ///
    /// 卸载或替换时，等待中的请求作废，收到 `Cancel` 的是旧回调（在本函数返回之前、调用本函数的线程上）；
    /// 新回调只会收到安装之后发出的请求。并发的 `set_chooser` 互不交错
    pub fn set_chooser(&self, callback: Option<ChooserFn>) {
        // 整个过程都在 `session` 的锁里：回调槽和调度器里的选词器一起换
        let mut session = self.session();
        // 先清标志、关开关，再动调度器（见 `chooser_installed` 字段的说明）
        self.chooser_installed.store(false, Ordering::Release);
        self.sync_llm_switch(&mut session);
        // 等待中的请求在这里作废。回调槽里还是旧回调，所以收到 Cancel 的是它
        session.ai.set_host_chooser(None);
        let Some(callback) = callback else {
            *lock_ignoring_poison(&self.callbacks.chooser) = None;
            return;
        };
        let installation = self
            .callbacks
            .next_chooser_installation
            .fetch_add(1, Ordering::Relaxed);
        *lock_ignoring_poison(&self.callbacks.chooser) = Some((installation, callback));
        let callbacks = Arc::clone(&self.callbacks);
        session.ai.set_host_chooser(Some(HostChooser::new(
            "host",
            "系统AI",
            Arc::new(move |event: ChooserEvent| callbacks.fire_chooser(installation, &event)),
        )));
        self.chooser_installed.store(true, Ordering::Release);
        self.sync_llm_switch(&mut session);
    }

    /// 宿主送回选词结果。`text` 为 `None` 表示没有结果（超时、模型不可用、被新请求取代）。
    /// 返回 `IME_CHOICE_*`。`IME_CHOICE_ACCEPTED` 表示结果已记下（写入缓存）：当前输入若还在等它，
    /// 就触发刷新回调，候选在随后的 [`Self::refresh_after_ai`] 里重排；输入已经变了或结束了则不通知，
    /// 结果留在缓存里，下次遇到同样的上文和拼音时直接用上
    pub fn deliver_choice(&self, id: u64, text: Option<&str>) -> i32 {
        match self.chooser_handle.deliver(id, text) {
            ChooserDelivery::Accepted => IME_CHOICE_ACCEPTED,
            ChooserDelivery::NoAnswer => IME_CHOICE_NO_ANSWER,
            ChooserDelivery::Implausible => IME_CHOICE_IMPLAUSIBLE,
            ChooserDelivery::Stale => IME_CHOICE_STALE,
            ChooserDelivery::TimedOut => IME_CHOICE_TIMED_OUT,
        }
    }
}

/// 在后台把词库文件的页读进内存，免得头几次按键各自去等缺页。
/// 这些是只读文件页，不计入进程的内存配额，内存紧张时系统可以随时丢弃
fn spawn_page_warmup(lexicon: CompiledLexicon) {
    let _ = std::thread::Builder::new()
        .name("ime-lexicon-warmup".to_string())
        .spawn(move || {
            const PAGE: usize = 4096;
            let bytes = lexicon.as_bytes();
            let mut acc = 0u8;
            let mut offset = 0usize;
            while offset < bytes.len() {
                acc = acc.wrapping_add(bytes[offset]);
                offset += PAGE;
            }
            std::hint::black_box(acc);
        });
}

/// 当前进程的 phys_footprint（字节）。iOS 用它判定键盘扩展是否超出内存限制
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub fn phys_footprint_bytes() -> u64 {
    // 只取 task_vm_info 的前 19 个 64 位字，其中第 18 个（下标）是 phys_footprint：
    // 5 个 mach_vm_size_t/integer_t 混排字段之后的布局在 <mach/task_info.h> 里是稳定的
    #[repr(C)]
    #[derive(Default)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }
    extern "C" {
        static mach_task_self_: u32;
        fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }
    const TASK_VM_INFO: u32 = 22;
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<i32>()) as u32;
    // SAFETY: info 是足够大的可写缓冲区，count 以 32 位字为单位给出它的大小；内核只写不超过 count 的部分
    let kr = unsafe {
        task_info(
            mach_task_self_,
            TASK_VM_INFO,
            (&mut info as *mut TaskVmInfo).cast::<i32>(),
            &mut count,
        )
    };
    if kr == 0 {
        info.phys_footprint
    } else {
        0
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub fn phys_footprint_bytes() -> u64 {
    0
}

// ---------------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------------

/// C 侧看到的不透明类型
#[allow(non_camel_case_types)]
pub type ime_engine = ImeEngine;

#[allow(non_camel_case_types)]
pub type ime_notify_callback = Option<unsafe extern "C" fn(user: *mut c_void)>;

fn into_c_string(text: String) -> *mut c_char {
    // JSON 里的 NUL 会被 serde 转义成 \u0000，这里只是防御
    let bytes: Vec<u8> = text.into_bytes().into_iter().filter(|&b| b != 0).collect();
    match CString::new(bytes) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `ptr` 必须是 NULL 或指向以 NUL 结尾的字符串
unsafe fn borrow_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 调用方保证 ptr 指向以 NUL 结尾的字符串
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

/// 调用引擎方法并返回 JSON；engine 为 NULL 返回 NULL，方法 panic 时返回 `fallback`
///
/// # Safety
/// `engine` 必须是 NULL 或 `ime_engine_new` 返回且尚未释放的指针
unsafe fn call_json(
    engine: *const ImeEngine,
    fallback: fn() -> String,
    f: impl FnOnce(&ImeEngine) -> String,
) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let json = catch_unwind(AssertUnwindSafe(|| f(engine))).unwrap_or_else(|_| fallback());
    into_c_string(json)
}

fn pass_json() -> String {
    view::PASS_JSON.to_string()
}

#[no_mangle]
pub extern "C" fn ime_ffi_abi_version() -> i32 {
    IME_FFI_ABI_VERSION
}

/// # Safety
/// `lexicon_path` 必须是以 NUL 结尾的 UTF-8 路径；`user_dict_path` 可为 NULL，否则同样必须是
/// 以 NUL 结尾的 UTF-8 路径（不是 UTF-8 时报错，不会悄悄当成 NULL）；`error_out` 可为 NULL，否则必须可写
#[no_mangle]
pub unsafe extern "C" fn ime_engine_new(
    lexicon_path: *const c_char,
    user_dict_path: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut ime_engine {
    if !error_out.is_null() {
        // SAFETY: 调用方保证 error_out 可写
        unsafe { *error_out = std::ptr::null_mut() };
    }
    let result = catch_unwind(|| {
        // SAFETY: 调用方保证两个路径指针符合约定
        let lexicon = unsafe { borrow_str(lexicon_path) }
            .ok_or_else(|| "lexicon_path 为空或不是 UTF-8".to_string())?;
        let user_dict = if user_dict_path.is_null() {
            None
        } else {
            // SAFETY: 同上
            let path = unsafe { borrow_str(user_dict_path) }
                .ok_or_else(|| "user_dict_path 不是 UTF-8".to_string())?;
            Some(Path::new(path))
        };
        ImeEngine::open(Path::new(lexicon), user_dict)
    })
    .unwrap_or_else(|_| Err("创建引擎时发生内部错误".to_string()));
    match result {
        Ok(engine) => Box::into_raw(Box::new(engine)),
        Err(message) => {
            if !error_out.is_null() {
                // SAFETY: 同上
                unsafe { *error_out = into_c_string(message) };
            }
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// `engine` 必须是 NULL 或 `ime_engine_new` 返回且尚未释放的指针；释放后不得再使用。
/// 与其他函数不同，本函数不能与同一引擎上的任何其他调用并发：调用方保证此刻没有别的线程正在使用这个引擎
#[no_mangle]
pub unsafe extern "C" fn ime_engine_free(engine: *mut ime_engine) {
    if engine.is_null() {
        return;
    }
    // SAFETY: 调用方保证指针来自 ime_engine_new 且只释放一次
    let engine = unsafe { Box::from_raw(engine) };
    engine.set_notify(None);
    let _ = catch_unwind(AssertUnwindSafe(|| engine.set_chooser(None)));
    let _ = catch_unwind(AssertUnwindSafe(move || drop(engine)));
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_key(
    engine: *mut ime_engine,
    kind: i32,
    value: u32,
) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, pass_json, |e| e.key(kind, value)) }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_select_candidate(
    engine: *mut ime_engine,
    global_index: u32,
) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, pass_json, |e| e.select_candidate(global_index as usize)) }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_tap_syllable(
    engine: *mut ime_engine,
    index: u32,
) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, pass_json, |e| e.tap_syllable(index as usize)) }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_commit_full_sentence(engine: *mut ime_engine) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, pass_json, |e| e.commit_full_sentence()) }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_view(engine: *mut ime_engine) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, view::idle_view_json, |e| e.view()) }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_candidates(
    engine: *mut ime_engine,
    offset: u32,
    count: u32,
) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe {
        call_json(engine, view::no_candidates_json, |e| {
            e.candidates(offset as usize, count as usize)
        })
    }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_refresh_after_ai(engine: *mut ime_engine) -> *mut c_char {
    // SAFETY: 调用方保证指针有效
    unsafe { call_json(engine, view::idle_view_json, |e| e.refresh_after_ai()) }
}

/// 返回 1 = 重新读入了，0 = 没有变化或没有启用持久化，-1 = 读取失败（或 `engine` 为 NULL）
///
/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_reload_user_dict(engine: *mut ime_engine) -> i32 {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return -1;
    };
    match catch_unwind(AssertUnwindSafe(|| engine.reload_user_dict())) {
        Ok(Ok(true)) => 1,
        Ok(Ok(false)) => 0,
        _ => -1,
    }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针；`text_before_cursor` 必须是 NULL 或以 NUL 结尾的字符串
#[no_mangle]
pub unsafe extern "C" fn ime_engine_set_context(
    engine: *mut ime_engine,
    text_before_cursor: *const c_char,
) {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    // 不是合法 UTF-8 的上文按「拿不到」处理
    let text = unsafe { borrow_str(text_before_cursor) };
    let _ = catch_unwind(AssertUnwindSafe(|| engine.set_context(text)));
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_reset(engine: *mut ime_engine) {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| engine.reset()));
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_is_composing(engine: *const ime_engine) -> bool {
    // SAFETY: 调用方保证指针有效
    unsafe { engine.as_ref() }.is_some_and(ImeEngine::is_composing)
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_is_english(engine: *const ime_engine) -> bool {
    // SAFETY: 调用方保证指针有效
    unsafe { engine.as_ref() }.is_some_and(ImeEngine::is_english)
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_ai_pending(engine: *const ime_engine) -> bool {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return false;
    };
    catch_unwind(AssertUnwindSafe(|| engine.ai_result_pending())).unwrap_or(false)
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针；`settings_json` 必须是 NULL 或以 NUL 结尾的字符串
#[no_mangle]
pub unsafe extern "C" fn ime_engine_configure(
    engine: *mut ime_engine,
    settings_json: *const c_char,
) -> i32 {
    // SAFETY: 调用方保证指针有效
    let (Some(engine), Some(json)) = (unsafe { engine.as_ref() }, unsafe {
        borrow_str(settings_json)
    }) else {
        return -1;
    };
    match catch_unwind(AssertUnwindSafe(|| engine.configure(json))) {
        Ok(Ok(())) => 0,
        _ => -1,
    }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针。`callback` 可能在任意线程被调用，`user` 原样传回；
/// 调用方保证在取消回调（传 NULL）或释放引擎之前 `user` 一直有效
#[no_mangle]
pub unsafe extern "C" fn ime_engine_set_ai_notify(
    engine: *mut ime_engine,
    callback: ime_notify_callback,
    user: *mut c_void,
) {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let wrapped: Option<NotifyFn> = callback.map(|cb| {
        let user = user as usize;
        // SAFETY: 调用方保证回调与 user 在取消之前有效，且回调可以在任意线程调用
        Arc::new(move || unsafe { cb(user as *mut c_void) }) as NotifyFn
    });
    engine.set_notify(wrapped);
}

#[allow(non_camel_case_types)]
pub type ime_chooser_callback =
    Option<unsafe extern "C" fn(user: *mut c_void, id: u64, request_json: *const c_char)>;

#[derive(serde::Serialize)]
struct ChoiceRequestOut<'a> {
    id: u64,
    context: &'a str,
    pinyin: &'a str,
    candidates: &'a [String],
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针。`callback` 可能在任意线程被调用，`user` 原样传回；
/// 调用方保证在取消回调（传 NULL）或释放引擎之前 `user` 一直有效。
/// 回调的 `request_json` 只在回调期间有效；为 NULL 表示编号 `id` 的请求已作废
#[no_mangle]
pub unsafe extern "C" fn ime_engine_set_chooser(
    engine: *mut ime_engine,
    callback: ime_chooser_callback,
    user: *mut c_void,
) {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let wrapped: Option<ChooserFn> = callback.map(|cb| {
        let user = user as usize;
        Arc::new(move |event: ChoiceEvent<'_>| match event {
            ChoiceEvent::Request {
                id,
                context,
                pinyin,
                candidates,
            } => {
                let request = ChoiceRequestOut {
                    id,
                    context,
                    pinyin,
                    candidates,
                };
                let Ok(json) = serde_json::to_string(&request) else {
                    return;
                };
                let Ok(json) = CString::new(json) else {
                    return;
                };
                // SAFETY: 调用方保证回调与 user 在取消之前有效；json 在调用期间有效
                unsafe { cb(user as *mut c_void, id, json.as_ptr()) }
            }
            // SAFETY: 同上
            ChoiceEvent::Cancel { id } => unsafe { cb(user as *mut c_void, id, std::ptr::null()) },
        }) as ChooserFn
    });
    let _ = catch_unwind(AssertUnwindSafe(|| engine.set_chooser(wrapped)));
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针；`text` 必须是 NULL 或以 NUL 结尾的字符串
#[no_mangle]
pub unsafe extern "C" fn ime_engine_deliver_choice(
    engine: *mut ime_engine,
    id: u64,
    text: *const c_char,
) -> i32 {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return IME_CHOICE_STALE;
    };
    let text = unsafe { borrow_str(text) };
    catch_unwind(AssertUnwindSafe(|| engine.deliver_choice(id, text))).unwrap_or(IME_CHOICE_STALE)
}

#[no_mangle]
pub extern "C" fn ime_llm_supported() -> bool {
    ImeEngine::llm_supported()
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针；`model_path` 必须是以 NUL 结尾的 UTF-8 路径；
/// `error_out` 可为 NULL，否则必须可写
#[no_mangle]
pub unsafe extern "C" fn ime_engine_load_llm(
    engine: *mut ime_engine,
    model_path: *const c_char,
    threads: i32,
    error_out: *mut *mut c_char,
) -> i32 {
    if !error_out.is_null() {
        // SAFETY: 调用方保证 error_out 可写
        unsafe { *error_out = std::ptr::null_mut() };
    }
    // SAFETY: 调用方保证指针有效
    let result = match (unsafe { engine.as_ref() }, unsafe { borrow_str(model_path) }) {
        (Some(engine), Some(path)) => catch_unwind(AssertUnwindSafe(|| {
            engine.load_llm(Path::new(path), threads.max(0) as u32)
        }))
        .unwrap_or_else(|_| Err("加载模型时发生内部错误".to_string())),
        _ => Err("engine 或 model_path 为空".to_string()),
    };
    match result {
        Ok(()) => 0,
        Err(message) => {
            if !error_out.is_null() {
                // SAFETY: 同上
                unsafe { *error_out = into_c_string(message) };
            }
            -1
        }
    }
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_unload_llm(engine: *mut ime_engine) {
    // SAFETY: 调用方保证指针有效
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| engine.unload_llm()));
}

/// # Safety
/// `engine` 必须是 NULL 或有效的引擎指针
#[no_mangle]
pub unsafe extern "C" fn ime_engine_llm_loaded(engine: *const ime_engine) -> bool {
    // SAFETY: 调用方保证指针有效
    unsafe { engine.as_ref() }.is_some_and(ImeEngine::is_llm_loaded)
}

#[no_mangle]
pub extern "C" fn ime_phys_footprint() -> u64 {
    phys_footprint_bytes()
}

/// # Safety
/// `string` 必须是 NULL 或本库返回且尚未释放的字符串
#[no_mangle]
pub unsafe extern "C" fn ime_string_free(string: *mut c_char) {
    if !string.is_null() {
        // SAFETY: 调用方保证指针来自本库的 CString::into_raw
        drop(unsafe { CString::from_raw(string) });
    }
}

#[cfg(test)]
mod tests {
    //! 需要看到引擎内部状态的测试（大模型开关、回调槽）。其余测试在 `tests/engine.rs`

    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;

    fn engine() -> ImeEngine {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        CompiledLexicon::load_or_build_default(root.clone()).expect("lexicon");
        ImeEngine::open(&root.join("data/compiled/merged_lexicon.bin"), None).expect("open engine")
    }

    fn type_letters(engine: &ImeEngine, letters: &str) {
        for ch in letters.chars() {
            engine.key(IME_KEY_CHAR, ch as u32);
        }
    }

    /// 「大模型重排」开关：开着却没有进程内后端时，调度器会走 Windows 版的 HTTP 路径
    fn llm_switch(engine: &ImeEngine) -> bool {
        engine.session().ai.cloud_client.config.enabled
    }

    #[test]
    fn llm_switch_follows_the_installed_backends() {
        let engine = engine();
        assert!(!llm_switch(&engine));
        engine.set_chooser(Some(Arc::new(|_| {})));
        assert!(llm_switch(&engine));
        // 替换：仍然开着
        engine.set_chooser(Some(Arc::new(|_| {})));
        assert!(llm_switch(&engine));
        // 没装模型时卸载模型、加载失败：选词器还在，开关不变
        engine.unload_llm();
        assert!(llm_switch(&engine));
        assert!(engine.load_llm(Path::new("/no/such/model.gguf"), 0).is_err());
        assert!(llm_switch(&engine));
        engine.set_chooser(None);
        assert!(!llm_switch(&engine));
        assert!(lock_ignoring_poison(&engine.callbacks.chooser).is_none());
    }

    #[test]
    fn a_panicking_chooser_cannot_leave_the_llm_switch_on() {
        // 评审的复现：选词回调在收到 Cancel 时 panic（C 回调做不到，Rust 调用方可以）。
        // 以前 set_chooser(None) 先动调度器、后清标志：panic 之后调度器里已经没有选词器，
        // 标志和开关却还开着，之后每次重排都去连本机的 llama-server
        let engine = engine();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        engine.set_chooser(Some(Arc::new(move |event: ChoiceEvent<'_>| match event {
            ChoiceEvent::Request { pinyin, .. } => {
                let _ = tx.lock().unwrap().send(pinyin.to_string());
            }
            ChoiceEvent::Cancel { .. } => panic!("chooser callback panics on Cancel"),
        })));
        engine.set_context(Some("我对天发誓这是我的"));
        type_letters(&engine, "shiyan");
        // 等整串的请求发出：此后卸载选词器一定会发 Cancel
        loop {
            let pinyin = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("chooser request");
            if pinyin == "shiyan" {
                break;
            }
        }
        assert!(engine.is_composing());
        assert!(llm_switch(&engine));

        // 回调的 panic 目前会从 set_chooser 里穿出来；调度器哪天自己接住它，下面的结论也一样成立
        let panicked = catch_unwind(AssertUnwindSafe(|| engine.set_chooser(None))).is_err();
        if panicked {
            // 不用等下一次按键：这次输入已经算丢掉了
            assert!(!engine.is_composing());
        }
        assert!(!engine.chooser_installed.load(Ordering::Acquire));
        assert!(!llm_switch(&engine), "no backend is installed, so the switch must be off");
        if panicked {
            assert!(!engine.composing.load(Ordering::Acquire), "flags are re-synchronised on recovery");
        }
        engine.reset();

        // 引擎照常可用，仍然不会打开开关
        type_letters(&engine, "nihao");
        assert!(engine.is_composing());
        assert!(!llm_switch(&engine));
        let commit = engine.key(IME_KEY_SPACE, 0);
        assert!(commit.contains(r#""action":"commit""#), "{commit}");
        // 再装一个正常的选词器也没问题
        engine.set_chooser(Some(Arc::new(|_| {})));
        assert!(llm_switch(&engine));
        engine.set_chooser(None);
        assert!(!llm_switch(&engine));
    }

    #[test]
    fn poison_recovery_resynchronises_flags_and_the_llm_switch() {
        let engine = engine();
        type_letters(&engine, "nihao");
        assert!(engine.is_composing());
        // 模拟「持锁期间 panic，留下不一致的状态」：开关被打开而没有后端
        let poisoned = catch_unwind(AssertUnwindSafe(|| {
            let mut session = engine.session();
            session.ai.cloud_client.config.enabled = true;
            session.english_passthrough_mode = true;
            panic!("panic while holding the session lock");
        }));
        assert!(poisoned.is_err());
        assert!(engine.session.is_poisoned());
        assert!(!engine.is_composing(), "a poisoned session no longer counts as composing");

        // 任何一次拿会话都会恢复：丢掉输入、对齐标志、关掉没有后端的开关
        let view = engine.view();
        assert!(view.contains(r#""visible":false"#), "{view}");
        assert!(!engine.session.is_poisoned());
        assert!(!engine.is_composing());
        assert!(engine.is_english(), "mirrors the session after recovery");
        assert!(!llm_switch(&engine));
    }

    #[test]
    fn concurrent_set_chooser_calls_leave_a_consistent_state() {
        // 以前回调槽在 session 的锁外面写：一个线程安装、另一个线程卸载交错时，
        // 可能留下「调度器里有选词器、标志为真、回调槽却是空的」，每个请求都白等 4 秒
        let engine = Arc::new(engine());
        for _ in 0..20 {
            let installer = {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        engine.set_chooser(Some(Arc::new(|_| {})));
                    }
                })
            };
            let remover = {
                let engine = Arc::clone(&engine);
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        engine.set_chooser(None);
                    }
                })
            };
            installer.join().unwrap();
            remover.join().unwrap();
            let installed = engine.chooser_installed.load(Ordering::Acquire);
            let slot_filled = lock_ignoring_poison(&engine.callbacks.chooser).is_some();
            assert_eq!(installed, slot_filled);
            assert_eq!(llm_switch(&engine), installed);
        }
        // 最后装上的一定能收到请求
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        engine.set_chooser(Some(Arc::new(move |event: ChoiceEvent<'_>| {
            if let ChoiceEvent::Request { id, .. } = event {
                let _ = tx.lock().unwrap().send(id);
            }
        })));
        engine.set_context(Some("我对天发誓这是我的"));
        type_letters(&engine, "shiyan");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the installed chooser receives requests");
        engine.reset();
        engine.set_chooser(None);
    }

    #[test]
    fn events_from_a_replaced_chooser_installation_are_dropped() {
        let callbacks = Callbacks::default();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        *lock_ignoring_poison(&callbacks.chooser) = Some((
            7,
            Arc::new(move |event: ChoiceEvent<'_>| {
                if let ChoiceEvent::Cancel { id } = event {
                    let _ = tx.lock().unwrap().send(id);
                }
            }),
        ));
        // 旧安装（编号 6）迟到的事件不会落到现在的回调头上
        callbacks.fire_chooser(6, &ChooserEvent::Cancel { id: 1 });
        assert!(rx.try_recv().is_err());
        callbacks.fire_chooser(7, &ChooserEvent::Cancel { id: 2 });
        assert_eq!(rx.try_recv(), Ok(2));
        *lock_ignoring_poison(&callbacks.chooser) = None;
        callbacks.fire_chooser(7, &ChooserEvent::Cancel { id: 3 });
        assert!(rx.try_recv().is_err());
    }
}
