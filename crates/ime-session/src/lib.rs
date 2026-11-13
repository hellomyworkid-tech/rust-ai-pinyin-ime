//! `ime-session`: 与平台无关的输入法按键状态机（从 `ime-tsf-win` 抽出，供 Windows TSF 与 iOS 键盘共用）
//!
//! 功能：
//! 1. **按键会话 (`KeySession`)**：
//!    处理 `A-Z`、数字键 `1-9` 选词、`Space` 首选上屏、`Enter` 原样字母上屏、`Tab` 采纳 AI Ghost Text 或切换英文候选、`Backspace` 退格、`Shift` 中英状态切换，
//!    以及拼音光标按音节移动、句中选词锁定、翻页与 AI 重排调用。
//! 2. **场景感知 (`detect_scene_from_process`)**：根据前台进程名识别应用场景（iOS 没有进程名，可直接用 `KeySession::set_scene`）。
//! 3. **候选框视图模型 (`CandidateWindowViewModel`)**：分页候选、Preedit 拼音串与 AI 语境提示标签。
//! 4. **触屏键盘入口**：按下标选词、设置页大小、由宿主注入光标前文本、直接设置场景、取消组合与查询组合状态。

use ime_ai::{DualModeAiOrchestrator, OpenAiCompatibleConfig, UserMemory};

pub mod emoji;
use ime_core::user_dict::UserWordRecord;
use ime_core::{AppScene, Candidate, FuzzyPinyinConfig, ImeCoreEngine, SegmentSpan};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// 输入法全局持久化设置（保存于 `%LOCALAPPDATA%\RustAiPinyinIME\ime_config.json`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImePersistentConfig {
    #[serde(default)]
    pub ai_config: OpenAiCompatibleConfig,
    #[serde(default)]
    pub fuzzy_config: FuzzyPinyinConfig,
}

impl ImePersistentConfig {
    pub fn default_config_path() -> PathBuf {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            let dir = PathBuf::from(local_app_data).join("RustAiPinyinIME");
            let _ = std::fs::create_dir_all(&dir);
            return dir.join("ime_config.json");
        }
        PathBuf::from("ime_config.json")
    }

    pub fn load_or_default() -> Self {
        Self::load_from(&Self::default_config_path())
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::default_config_path())
    }

    /// 从指定路径读取配置（文件缺失或损坏时返回默认值）。iOS 等没有 `%LOCALAPPDATA%` 的平台
    /// 由宿主传入 App Group 容器内的路径
    pub fn load_from(path: &std::path::Path) -> Self {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(cfg) = serde_json::from_str::<Self>(&content) {
                return cfg;
            }
        }
        Self::default()
    }

    /// 原子地保存配置到指定路径（先写临时文件再改名，自动创建上级目录）
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

/// 虚拟按键枚举（对应 Win32 `WM_KEYDOWN` / `ITfKeyEventSink::OnKeyDown` 的 `WPARAM`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKey {
    Char(char),
    Digit(u8),
    Space,
    Enter,
    Backspace,
    Delete,
    Escape,
    Tab,
    Shift,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    CtrlLeft,
    CtrlRight,
    Home,
    End,
    Other(u32),
}

impl VirtualKey {
    /// 从 Windows 虚拟键码 `vk_code` 与 Shift 状态解析
    pub fn from_win32_vk(vk: u32, shift_pressed: bool) -> Self {
        Self::from_win32_vk_ex(vk, shift_pressed, false)
    }

    /// 从 Windows 虚拟键码 `vk_code`、Shift 与 Ctrl 状态解析
    pub fn from_win32_vk_ex(vk: u32, shift_pressed: bool, ctrl_pressed: bool) -> Self {
        match vk {
            0x41..=0x5A => {
                let base = (vk as u8 - 0x41) + if shift_pressed { b'A' } else { b'a' };
                Self::Char(base as char)
            }
            0xDE if !shift_pressed => Self::Char('\''), // 单引号手动分词符
            0x31..=0x39 if !shift_pressed => Self::Digit((vk - 0x30) as u8),
            0x20 => Self::Space,
            0x0D => Self::Enter,
            0x08 => Self::Backspace,
            0x2E => Self::Delete,
            0x1B => Self::Escape,
            0x09 => Self::Tab,
            0x10 | 0xA0 | 0xA1 => Self::Shift,
            0x21 | 0xBD => Self::PageUp,   // PageUp or '-'
            0x22 | 0xBB => Self::PageDown, // PageDown or '='
            0x26 => Self::Up,              // Up Arrow
            0x28 => Self::Down,            // Down Arrow
            0x25 if ctrl_pressed => Self::CtrlLeft,
            0x27 if ctrl_pressed => Self::CtrlRight,
            0x25 => Self::Left,  // Left Arrow
            0x27 => Self::Right, // Right Arrow
            0x24 => Self::Home,  // Home
            0x23 => Self::End,   // End
            other => Self::Other(other),
        }
    }
}

/// 候选框悬浮窗渲染所需的完整视图数据
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CandidateWindowViewModel {
    pub visible: bool,
    pub preedit_text: String,
    pub page_index: usize,
    pub page_size: usize,
    pub total_candidates: usize,
    pub visible_candidates: Vec<Candidate>,
    /// 当前页内通过上下键高亮的候选项下标 (`0..visible_candidates.len()`)
    pub selected_candidate_index: usize,
    /// 是否通过按上方向键 `↑` 选中了第 2 行的「整句」预览项（选中时按空格直接将整句上屏）
    #[serde(default)]
    pub full_sentence_selected: bool,
    pub active_scene_label: String,
    pub ai_semantic_hint: String,
    pub ghost_text_preview: Option<String>,
    /// 实时整句合成预览（即使光标移动到句中某个音节，也实时显示当前整句结果）
    pub composed_full_sentence: String,
    /// 整句预览中按「是否已手动选定锁定」切分的片段列表 `(segment_text, is_pinned)`，用于 UI 高亮已选定词
    #[serde(default)]
    pub full_sentence_parts: Vec<(String, bool)>,
    /// 首选整句拆解出的逐个拼音音节/英文词区间（用于在预编辑栏绘制音节与接收鼠标点击）
    pub syllable_spans: Vec<SegmentSpan>,
    /// 首选整句的分词块列表
    pub top_segments: Vec<SegmentSpan>,
    /// 当前拼音编辑光标在 `input_buffer` 中的精确字节偏移 (`0..=input_buffer.len()`)
    pub cursor_byte_pos: usize,
    /// 当前光标所在的活跃音节下标（`Some(idx)` 时预编辑栏高亮该音节，且候选列表展示该音节对应的选词）
    pub active_syllable_index: Option<usize>,
}

/// TSF 按键处理后的动作响应（对应 `ITfKeyEventSink` 的 `*pfEaten` 与 `ITfComposition` 提交）
#[derive(Debug, Clone, PartialEq)]
pub enum KeyAction {
    /// 拦截按键（`*pfEaten = TRUE`），更新预编辑串与悬浮候选窗
    EatAndUpdateUi(CandidateWindowViewModel),
    /// 拦截按键（`*pfEaten = TRUE`），将最终选定文本写入目标应用程序文档并关闭候选窗
    EatAndCommit {
        committed_text: String,
        ghost_text: Option<String>,
    },
    /// 放行按键（`*pfEaten = FALSE`），交由目标应用程序直接处理
    PassThrough,
}

/// 根据前台进程可执行文件名自动识别应用场景（L3 场景感知）
pub fn detect_scene_from_process(exe_name: &str) -> AppScene {
    let lower = exe_name.trim().to_ascii_lowercase();
    if [
        "code.exe",
        "cursor.exe",
        "rustrover64.exe",
        "idea64.exe",
        "pycharm64.exe",
        "devenv.exe",
        "windowsterminal.exe",
        "powershell.exe",
        "cmd.exe",
        "nvim.exe",
        "wezterm-gui.exe",
    ]
    .iter()
    .any(|&p| lower.contains(p))
    {
        AppScene::CodeEditor
    } else if [
        "wechat.exe",
        "weixin.exe",
        "qq.exe",
        "feishu.exe",
        "lark.exe",
        "telegram.exe",
        "discord.exe",
        "slack.exe",
    ]
    .iter()
    .any(|&p| lower.contains(p))
    {
        AppScene::Chat
    } else if [
        "texstudio.exe",
        "overleaf",
        "matlab.exe",
        "mathematica.exe",
        "zotero.exe",
    ]
    .iter()
    .any(|&p| lower.contains(p))
    {
        AppScene::Academic
    } else if ["winword.exe", "wps.exe", "notion.exe", "obsidian.exe"]
        .iter()
        .any(|&p| lower.contains(p))
    {
        AppScene::Document
    } else {
        AppScene::General
    }
}

/// 上屏历史 / 宿主上文保留的最大字符数
const HISTORY_MAX_CHARS: usize = 64;

/// 交给 AI 调度器的某个候选文本的个人记忆：记忆强度，以及这个词有没有主动改选过
/// （调度器据此给主动改选过的词乘 [`ime_ai::EXPLICIT_CHOICE_PRIOR_FACTOR`]，为什么区分见
/// `ime_ai::choose_with_user_priors` 的说明）。
///
/// 先查键入编码下的记录。简拼输入（如 `wzj`）时，解码器还会把用户用全拼输入过的词（`wenzhaiji` 下的「文摘集」）
/// 召回到首位，那条记录存在全拼编码下，用键入编码查不到。`jianpin` 是 [`ime_core::UserDict::jianpin_matches`]
/// 对键入编码的结果（记忆强度已按时间衰减，简拼前缀匹配再乘 0.85；主动改选次数原样保留）：
/// 键入编码下的那条与这个词的所有简拼匹配记录里，取先验（[`UserMemory::prior`]）最强的那条。
///
/// 不能每个词只看简拼召回里排在最前的一条（[`ime_core::UserDict::top_jianpin_records`] 就是这样去重的）：
/// 在简拼下空格上屏过几次之后，键入编码自己的那条记录推导出的简拼也是 `wzj`，记忆强度更高、排在前面，
/// 会把用户用全拼主动改选的那条连同主动改选的倍数一起挡掉。
///
/// 返回的是先验最强的那一条记录，[`ime_ai::ChooserMemoryPolicy::Contested`] 和 `Taught` 判断「近期用过」「教过」时
/// 读的也是这一条的记忆强度和主动改选标记：一条很久以前主动改选的记录（先验 3 × 0.5）赢过一条近期空格上屏的记录（1.3）时，这个词在 Contested
/// 的计数里按 0.5 算、不算近期用过。只有简拼输入、而且两条记录恰好落在这个强度区间时才会这样，影响的只是要不要
/// 问 Apple 端侧模型，所以没有为此再拆一个字段
fn user_memory(
    user_dict: &ime_core::UserDict,
    code: &str,
    text: &str,
    jianpin: &[UserWordRecord],
) -> UserMemory {
    let direct = UserMemory {
        boost: user_dict.user_boost(code, text),
        explicit: user_dict.explicit_choice_count(code, text) > 0,
    };
    jianpin
        .iter()
        .filter(|r| r.text == text)
        .map(|r| UserMemory {
            boost: r.dynamic_score,
            explicit: r.explicit_count > 0,
        })
        .fold(direct, |best, m| if m.prior() > best.prior() { m } else { best })
}

/// 去掉全部空白字符（比较整句与词块时忽略中英混排的显示用空格）
fn strip_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// TSF 文本输入会话状态机（遵循主流输入法标准逻辑：拼音光标移动 + 光标处直接增删改拼音 + 光标音节联动选词）
pub struct KeySession {
    pub core: ImeCoreEngine,
    pub ai: DualModeAiOrchestrator,
    pub english_passthrough_mode: bool,
    pub input_buffer: String,
    /// 当前拼音光标在 `input_buffer` 中的字节位置 (`0..=input_buffer.len()`)
    pub cursor_pos: usize,
    /// 是否处于单音节光标聚焦选词状态（移动左右键或点击音节时激活，回到句尾时归零）
    pub cursor_syllable_mode: bool,
    /// 当前光标是否因选定前缀词而自动推进至下一个未锁定音节（若是，继续输入新拼音字母时应追加到句尾，而非插入在光标处）
    pub cursor_auto_advanced: bool,
    /// 最近一次操作是否为在句中直接修改拼音字母（若是，按空格可直接将修正后的整句上屏）
    pub last_op_was_pinyin_edit: bool,
    /// 本轮输入是否因光标跳到非句首音节选词而自动锁定了光标前未手动选择的默认前缀
    pub had_auto_prefix_lock: bool,
    pub current_candidates: Vec<Candidate>,
    pub current_page: usize,
    pub page_size: usize,
    /// 当前页面中由上下方向键选中的高亮候选词下标 (`0..page_size`)
    pub selected_candidate_index: usize,
    /// 当前是否通过按上方向键 `↑` 将焦点移到了「整句」行（为 `true` 时按空格直接整句上屏，按 `↓` 回到候选词列表）
    pub full_sentence_selected: bool,
    pub committed_history: String,
    /// 宿主提供的光标前文本（如 iOS 的 `documentContextBeforeInput`）。为 `Some` 时取代 `committed_history`
    /// 作为解码与 AI 重排的上文；为 `None`（Windows 默认）时沿用会话自己的上屏历史。见 `set_context_before_cursor`
    pub host_context: Option<String>,
    pub pending_ghost_text: Option<String>,
    pub active_scene: AppScene,
    pub last_ai_hint: String,
    /// 用户在句中选词锁定的片段：`start_byte -> (end_byte, replacement_text)`
    pub pinned_segments: HashMap<usize, (usize, String)>,
    /// 本轮输入第一次锁定词块时用户看到的首选整句，用来判断含锁定词块的上屏算不算主动改选
    /// （与它相同说明用户只是逐段接受了显示的结果）。没有锁定词块时为空；锁定之后又改了拼音也清空
    /// （这句话对应的是改之前的拼音），此后含锁定词块的上屏照旧按主动改选算
    pub sentence_before_pins: String,
    /// 当前首选整句合成文本
    pub last_full_sentence: String,
    /// 当前首选整句的词级片段列表
    pub last_full_segments: Vec<SegmentSpan>,
    /// 当前首选整句的逐音节区间列表
    pub last_syllable_spans: Vec<SegmentSpan>,
    /// 是否启用 AI Ghost Text 续写（按 Tab 采纳）。全局版默认关闭，避免在真实应用中劫持 Tab 键
    pub ghost_text_enabled: bool,
    /// 是否启用「上文相关的纠正记忆」：用户在某段上文下没要首选、改选了别的整词时，记住这次选择，
    /// 以后在同样的上文末尾输入同样的拼音就直接把它排在首位（标签 `[记忆·上文]`，大模型不再改动它）。
    /// 默认关闭（Windows 版行为不变），iOS 键盘打开
    pub context_memory_enabled: bool,
    /// 是否在常用词候选后面跟上对应的表情（见 [`emoji`]）。默认关闭（Windows 版行为不变），iOS 键盘打开
    pub emoji_candidates_enabled: bool,
    /// 触屏的音节选字方式（iOS 键盘打开，Windows 版默认关闭）。用户点了某个音节、在那里选了字之后：
    /// - 回到整句，不再自动跳到别的未选音节；由用户看整句候选决定上屏，还是接着点别的音节。
    ///   选到了句尾、而且整句都已选定时照常直接上屏；
    /// - 点着一个已选定的音节按退格，撤销这个音节上的选字（光标留在这个音节上，可以重选）。
    /// 关闭时（键盘的逐段选词）：选完跳到第一个未选音节；退格撤销紧挨在光标左边的选字
    pub touch_syllable_editing: bool,
}

impl KeySession {
    pub fn new(core: ImeCoreEngine, ai: DualModeAiOrchestrator) -> Self {
        Self {
            core,
            ai,
            english_passthrough_mode: false,
            input_buffer: String::new(),
            cursor_pos: 0,
            cursor_syllable_mode: false,
            cursor_auto_advanced: false,
            last_op_was_pinyin_edit: false,
            had_auto_prefix_lock: false,
            current_candidates: Vec::new(),
            current_page: 0,
            page_size: 5,
            selected_candidate_index: 0,
            full_sentence_selected: false,
            committed_history: String::new(),
            host_context: None,
            pending_ghost_text: None,
            active_scene: AppScene::General,
            last_ai_hint: String::new(),
            pinned_segments: HashMap::new(),
            sentence_before_pins: String::new(),
            last_full_sentence: String::new(),
            last_full_segments: Vec::new(),
            last_syllable_spans: Vec::new(),
            ghost_text_enabled: true,
            context_memory_enabled: false,
            emoji_candidates_enabled: false,
            touch_syllable_editing: false,
        }
    }

    /// 当焦点窗口切换时更新当前进程场景（例如在 IDE 中自动调整英文权重与空格策略）
    /// 同时清空上文语境，避免把微信里的聊天内容带进 VS Code 的选词判断
    pub fn on_foreground_process_changed(&mut self, process_name: &str) {
        self.active_scene = detect_scene_from_process(process_name);
        self.committed_history.clear();
        self.host_context = None;
        self.pending_ghost_text = None;
    }

    // ------------------------------------------------------------------
    // 触屏键盘（iOS）入口：均为对既有状态的薄封装，不改变按键路径的行为
    // ------------------------------------------------------------------

    /// 当前正在组合（输入缓冲非空）。便宜的 O(1) 查询，宿主据此决定是否显示候选栏、是否拦截退格等
    pub fn is_composing(&self) -> bool {
        !self.input_buffer.is_empty()
    }

    /// 取消当前组合：清空拼音缓冲、锁定词块、候选与翻页状态，并丢弃待采纳的 Ghost Text。
    /// 不上屏任何内容；不改变中英模式、场景与上文
    pub fn cancel_composition(&mut self) {
        self.reset_input_state();
        self.pending_ghost_text = None;
    }

    /// 直接设置应用场景（iOS 没有前台进程名，由宿主按输入框类型等自行决定）。
    /// 与 `on_foreground_process_changed` 不同，不清空上文；组合中会立即按新场景重算候选
    pub fn set_scene(&mut self, scene: AppScene) {
        if self.active_scene == scene {
            return;
        }
        self.active_scene = scene;
        if !self.input_buffer.is_empty() {
            self.recompute_candidates();
        }
    }

    /// 设置每页候选数（至少为 1）。组合中会重新分页，并尽量保持当前高亮的候选仍然是高亮项
    pub fn set_page_size(&mut self, page_size: usize) {
        let page_size = page_size.max(1);
        if page_size == self.page_size {
            return;
        }
        let global = self.current_page * self.page_size + self.selected_candidate_index;
        self.page_size = page_size;
        self.current_page = global / page_size;
        self.selected_candidate_index = global % page_size;
    }

    /// 由宿主提供光标前文本作为解码与 AI 重排的上文（取末尾 64 个字符，与上屏历史的长度上限一致）。
    /// 一旦设置，即使为空串也会取代 `committed_history`（文本框确实是空的）；
    /// 之后每次上屏的文字会同步追加到它后面，直到宿主再次调用本方法或 `clear_context_before_cursor`。
    /// 不会立即重算当前候选；需要时调用 `refresh_candidates`
    pub fn set_context_before_cursor(&mut self, text: &str) {
        self.host_context = Some(Self::tail_chars(text, HISTORY_MAX_CHARS));
    }

    /// 撤销宿主提供的上文，回到使用会话自己的上屏历史
    pub fn clear_context_before_cursor(&mut self) {
        self.host_context = None;
    }

    /// 解码与重排实际使用的上文：宿主提供的光标前文本优先，否则是会话自己的上屏历史
    pub fn context_before_cursor(&self) -> &str {
        self.host_context
            .as_deref()
            .unwrap_or(&self.committed_history)
    }

    /// 选定当前页内第 `visible_idx` 个候选（从 0 起，与数字键 `1-9` 等价，但不限于 9 个）。
    /// 无组合时放行；下标超出当前页时只刷新视图，不做任何改动
    pub fn select_visible_candidate(&mut self, visible_idx: usize) -> KeyAction {
        if self.input_buffer.is_empty() {
            return KeyAction::PassThrough;
        }
        self.full_sentence_selected = false;
        if visible_idx >= self.visible_candidate_count() {
            return KeyAction::EatAndUpdateUi(self.build_view_model());
        }
        self.commit_candidate_at_visible_index(visible_idx)
    }

    /// 选定候选列表中第 `global_idx` 个候选（跨页的全局下标，从 0 起）。
    /// 无组合时放行；下标越界时只刷新视图
    pub fn select_candidate(&mut self, global_idx: usize) -> KeyAction {
        if self.input_buffer.is_empty() {
            return KeyAction::PassThrough;
        }
        self.full_sentence_selected = false;
        self.commit_candidate_global_index(global_idx)
    }

    /// 取字符串末尾至多 `max_chars` 个字符
    fn tail_chars(text: &str, max_chars: usize) -> String {
        let total = text.chars().count();
        if total <= max_chars {
            return text.to_string();
        }
        text.chars().skip(total - max_chars).collect()
    }

    fn predict_ghost(&self) -> Option<String> {
        if !self.ghost_text_enabled {
            return None;
        }
        self.ai
            .local_engine
            .predict_next_ghost_text(self.context_before_cursor(), self.active_scene)
    }

    /// 计算当前光标 `cursor_pos` 对应的活跃音节下标
    /// 规则：光标位于某音节「前端或内部」时归属该音节（`|shi`、`s|hi`、`sh|i` 均高亮 `shi`），
    /// 与左方向键「按音节跳到音节最前端」配合：按 ← 跳到 `|shi` 即显示 `shi` 起始的候选词；
    /// 光标处于整串末尾（音节模式下）时归属最后一个音节。
    pub fn active_syllable_index(&self) -> Option<usize> {
        if self.input_buffer.is_empty() || self.last_syllable_spans.is_empty() {
            return None;
        }
        if self.last_op_was_pinyin_edit {
            if self.pinned_segments.is_empty() {
                return None;
            }
            let unpinned_start = self.first_unpinned_offset();
            if unpinned_start >= self.input_buffer.len() {
                return None;
            }
            return self
                .last_syllable_spans
                .iter()
                .position(|s| s.start <= unpinned_start && unpinned_start < s.end)
                .or_else(|| {
                    self.last_syllable_spans
                        .iter()
                        .position(|s| s.start >= unpinned_start)
                });
        }
        if !self.cursor_syllable_mode && self.cursor_pos >= self.input_buffer.len() {
            return None;
        }
        let pos = self.cursor_pos;
        if let Some(idx) = self
            .last_syllable_spans
            .iter()
            .position(|s| s.start <= pos && pos < s.end)
        {
            return Some(idx);
        }
        // 光标落在音节分隔符 `'` 上：归属其后的音节；落在末尾：归属最后一个音节
        self.last_syllable_spans
            .iter()
            .position(|s| s.start >= pos)
            .or(Some(self.last_syllable_spans.len() - 1))
    }

    /// 触屏模式下，当前高亮的是用户自己点的音节（不是选完字自动跳过去的，也不是改拼音后推出来的）
    fn on_tapped_syllable(&self) -> bool {
        self.touch_syllable_editing
            && self.cursor_syllable_mode
            && !self.cursor_auto_advanced
            && !self.last_op_was_pinyin_edit
            && self.active_syllable_index().is_some()
    }

    /// 光标左侧最近的音节起点（用于 ← 键按整音节跳跃）
    fn previous_syllable_start(&self, pos: usize) -> Option<usize> {
        self.last_syllable_spans
            .iter()
            .map(|s| s.start)
            .filter(|&s| s < pos)
            .max()
    }

    /// 鼠标点击预编辑栏中的第 `syl_idx` 个拼音音节（可选传入音节内部子偏移 `inner_byte_offset`）
    pub fn click_pinyin_syllable(&mut self, syl_idx: usize, inner_byte_offset: Option<usize>) {
        let Some(span) = self.last_syllable_spans.get(syl_idx).cloned() else {
            return;
        };
        let already_on_this_syl = self.active_syllable_index() == Some(syl_idx);
        let is_last = syl_idx + 1 == self.last_syllable_spans.len();
        if already_on_this_syl {
            if let Some(rel) = inner_byte_offset {
                // 再次点击同一音节：把光标放到点击处的字母间，仍停留在该音节内
                let max_pos = if is_last { span.end } else { span.end.saturating_sub(1) };
                self.cursor_pos = (span.start + rel).clamp(span.start, max_pos.max(span.start));
            } else {
                self.cursor_pos = span.start;
            }
        } else {
            // 首次点击某音节：光标置于该音节最前端（如 `|zuo`），展示 `zuo` 起始的候选词
            self.cursor_pos = span.start;
        }
        self.cursor_syllable_mode = true;
        self.cursor_auto_advanced = false;
        self.last_op_was_pinyin_edit = false;
        self.had_auto_prefix_lock = false;
        self.full_sentence_selected = false;
        self.current_page = 0;
        self.selected_candidate_index = 0;
        self.recompute_candidates();
    }

    /// 鼠标点击回到整句模式（或按 End 键跳回拼音末尾）
    pub fn return_cursor_to_end(&mut self) {
        self.cursor_pos = self.input_buffer.len();
        self.cursor_syllable_mode = false;
        self.cursor_auto_advanced = false;
        self.last_op_was_pinyin_edit = false;
        self.had_auto_prefix_lock = false;
        self.full_sentence_selected = false;
        self.current_page = 0;
        self.selected_candidate_index = 0;
        self.recompute_candidates();
    }

    /// 返回从 `from_pos` 起第一个未被 `pinned_segments` 覆盖的字节位置（若直至句尾均已锁定则返回 `self.input_buffer.len()`）
    pub fn next_unpinned_offset_from(&self, from_pos: usize) -> usize {
        let n = self.input_buffer.len();
        let mut pos = from_pos.min(n);
        while let Some((&_s, &(e, _))) = self
            .pinned_segments
            .iter()
            .find(|(&s, &(e, _))| s <= pos && pos < e)
        {
            if e > pos {
                pos = e.min(n);
            } else {
                break;
            }
        }
        while pos < n && self.input_buffer.as_bytes()[pos] == b'\'' {
            pos += 1;
        }
        pos
    }

    /// 返回从句首 0 起连续已锁定前缀之后的第一个未锁定字节位置
    pub fn first_unpinned_offset(&self) -> usize {
        self.next_unpinned_offset_from(0)
    }

    /// 将已手动锁定的中文片段与剩余原始英文字母拼接（用于按 Enter 或 Shift 直接上屏时保留已锁定的词块）
    fn format_raw_with_pins(&self) -> String {
        if self.pinned_segments.is_empty() {
            return self.input_buffer.clone();
        }
        let n = self.input_buffer.len();
        let mut pins: Vec<(usize, usize, &str)> = self
            .pinned_segments
            .iter()
            .filter_map(|(&s, (e, txt))| {
                let s = s.min(n);
                let e = (*e).min(n);
                (s < e).then_some((s, e, txt.as_str()))
            })
            .collect();
        pins.sort_by_key(|&(s, _, _)| s);
        let mut out = String::new();
        let mut pos = 0usize;
        for (s, e, txt) in pins {
            if s > pos {
                out.push_str(&self.input_buffer[pos..s]);
            }
            if s >= pos {
                out.push_str(txt);
                pos = e;
            }
        }
        if pos < n {
            out.push_str(&self.input_buffer[pos..n]);
        }
        out
    }

    /// 当在位置 `pos` 插入 `delta` 字节时，平移 `pos` 之后的已锁定分词块，并清除被插入点切开的锁定块
    fn shift_pins_on_insert(&mut self, pos: usize, delta: usize) {
        // 拼音改了：锁定之前记下的整句对应的是改之前的拼音，不能再拿来比较
        self.sentence_before_pins.clear();
        let mut next = HashMap::new();
        for (&s, &(e, ref txt)) in &self.pinned_segments {
            if e <= pos {
                next.insert(s, (e, txt.clone()));
            } else if s >= pos {
                next.insert(s + delta, (e + delta, txt.clone()));
            }
            // 若 s < pos && e > pos，说明用户在锁定词内部插入了新拼音字母，自动解除该词锁定以便重新解码
        }
        self.pinned_segments = next;
    }

    /// 当删除位置 `del_idx` 处的 1 个字节时，平移其后的已锁定分词块，并解除包含被删字母的锁定块
    fn shift_pins_on_delete(&mut self, del_idx: usize) {
        // 同 `shift_pins_on_insert`：拼音改了，锁定之前记下的整句作废
        self.sentence_before_pins.clear();
        let mut next = HashMap::new();
        for (&s, &(e, ref txt)) in &self.pinned_segments {
            if e <= del_idx {
                next.insert(s, (e, txt.clone()));
            } else if s > del_idx {
                next.insert(s - 1, (e - 1, txt.clone()));
            }
        }
        self.pinned_segments = next;
    }

    fn reset_input_state(&mut self) {
        // 每次输入结束（上屏、取消、删空）都经过这里：把这次输入期间学到的东西交给个人词库的写盘线程。
        // 学习接口本身只改内存，所以一次输入不管学了几条，至多在这里取一份快照；没有改动时是空操作
        self.core.flush_user_dict();
        self.input_buffer.clear();
        self.cursor_pos = 0;
        self.cursor_syllable_mode = false;
        self.cursor_auto_advanced = false;
        self.last_op_was_pinyin_edit = false;
        self.had_auto_prefix_lock = false;
        self.full_sentence_selected = false;
        self.current_candidates.clear();
        self.pinned_segments.clear();
        self.sentence_before_pins.clear();
        self.last_full_sentence.clear();
        self.last_full_segments.clear();
        self.last_syllable_spans.clear();
        self.current_page = 0;
        self.selected_candidate_index = 0;
    }

    fn visible_candidate_count(&self) -> usize {
        let start = self.current_page * self.page_size;
        let end = (start + self.page_size).min(self.current_candidates.len());
        end.saturating_sub(start)
    }

    /// 处理单次按键事件
    pub fn process_key(&mut self, key: VirtualKey) -> KeyAction {
        match key {
            VirtualKey::Shift => {
                // Shift 键切换中/英模式；若当前有未上屏编码，直接将已锁定中文+剩余原样字母上屏并切英文
                if !self.input_buffer.is_empty() {
                    let raw = self.format_raw_with_pins();
                    self.reset_input_state();
                    self.english_passthrough_mode = true;
                    return KeyAction::EatAndCommit {
                        committed_text: raw,
                        ghost_text: None,
                    };
                }
                self.english_passthrough_mode = !self.english_passthrough_mode;
                KeyAction::PassThrough
            }
            VirtualKey::Char(ch) => {
                if self.english_passthrough_mode {
                    return KeyAction::PassThrough;
                }
                self.pending_ghost_text = None;
                self.full_sentence_selected = false;
                // 若光标是因选定前缀词（如在 `wolaiceshi` 中选定「我来」）而自动推进到下一个未锁定音节 `|ce`，
                // 此时用户继续敲击新拼音字母（如 `yixia`）应当追加在整串末尾（变成 `wolaiceshiyixia`），
                // 而保持「我来」锁定与 `|ce` 焦点不变！
                if self.cursor_auto_advanced {
                    let end_pos = self.input_buffer.len();
                    self.input_buffer.insert(end_pos, ch);
                    self.shift_pins_on_insert(end_pos, ch.len_utf8());
                    self.last_op_was_pinyin_edit = false;
                    self.current_page = 0;
                    self.selected_candidate_index = 0;
                    self.recompute_candidates();
                    return KeyAction::EatAndUpdateUi(self.build_view_model());
                }
                let pos = self.cursor_pos.min(self.input_buffer.len());
                let was_in_middle = pos < self.input_buffer.len();
                self.input_buffer.insert(pos, ch);
                self.shift_pins_on_insert(pos, ch.len_utf8());
                self.cursor_pos = pos + ch.len_utf8();
                if self.cursor_pos >= self.input_buffer.len() && !was_in_middle {
                    self.cursor_syllable_mode = false;
                    self.last_op_was_pinyin_edit = false;
                } else {
                    self.last_op_was_pinyin_edit = true;
                }
                self.current_page = 0;
                self.selected_candidate_index = 0;
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Backspace => {
                if self.input_buffer.is_empty() {
                    self.pending_ghost_text = None;
                    return KeyAction::PassThrough;
                }
                self.full_sentence_selected = false;
                // 触屏：点着一个已选定的音节按退格，撤销这个音节所在词块的选字，光标留在这个音节上
                if self.on_tapped_syllable() {
                    let syllable = self
                        .active_syllable_index()
                        .and_then(|i| self.last_syllable_spans.get(i).cloned());
                    let covering = syllable.and_then(|syl| {
                        self.pinned_segments
                            .iter()
                            .find(|(&s, &(e, _))| s <= syl.start && syl.start < e)
                            .map(|(&s, _)| (s, syl.start))
                    });
                    if let Some((pin_start, syllable_start)) = covering {
                        self.pinned_segments.remove(&pin_start);
                        if self.pinned_segments.is_empty() {
                            self.sentence_before_pins.clear();
                        }
                        self.cursor_pos = syllable_start;
                        self.current_page = 0;
                        self.selected_candidate_index = 0;
                        self.recompute_candidates();
                        return KeyAction::EatAndUpdateUi(self.build_view_model());
                    }
                }
                let pos = self.cursor_pos.min(self.input_buffer.len());
                // 若当前光标左侧紧挨着一个已手动选定锁定的分词块（如选定「我来」后光标位于 `|ce`），
                // 或光标因先选后方音节而自动跳回句首 0（`cursor_auto_advanced`）时，
                // 按退格键首先解除最近锁定词块并退回其起点（若从前缀锁定解除后已无锁定词则回到句尾）
                if let Some((&pin_start, _)) = self
                    .pinned_segments
                    .iter()
                    .find(|(_, &(e, _))| e == pos)
                    .or_else(|| {
                        if pos == 0 && self.cursor_auto_advanced {
                            self.pinned_segments.iter().max_by_key(|(&s, _)| s)
                        } else {
                            None
                        }
                    })
                {
                    let pin_start = pin_start;
                    self.pinned_segments.remove(&pin_start);
                    if self.pinned_segments.is_empty() {
                        // 锁定全部撤销：下次锁定时重新记下那时看到的整句
                        self.sentence_before_pins.clear();
                        if pos == 0 && pin_start > 0 {
                            self.cursor_pos = pin_start;
                            self.cursor_syllable_mode = true;
                        } else {
                            self.cursor_pos = self.input_buffer.len();
                            self.cursor_syllable_mode = false;
                        }
                        self.cursor_auto_advanced = false;
                    } else {
                        self.cursor_pos = pin_start;
                        self.cursor_syllable_mode = true;
                        self.cursor_auto_advanced = true;
                    }
                    self.last_op_was_pinyin_edit = false;
                    self.current_page = 0;
                    self.selected_candidate_index = 0;
                    self.recompute_candidates();
                    return KeyAction::EatAndUpdateUi(self.build_view_model());
                }
                if pos == 0 {
                    // 光标已在最左端 0，无左侧字符可删
                    return KeyAction::EatAndUpdateUi(self.build_view_model());
                }
                let was_in_middle = pos < self.input_buffer.len() || self.cursor_syllable_mode;
                let del_idx = pos - 1;
                self.input_buffer.remove(del_idx);
                self.shift_pins_on_delete(del_idx);
                self.cursor_pos = del_idx;
                self.current_page = 0;
                self.selected_candidate_index = 0;
                if self.input_buffer.is_empty() {
                    self.reset_input_state();
                    return KeyAction::EatAndUpdateUi(self.build_view_model());
                }
                if self.cursor_pos >= self.input_buffer.len() && !was_in_middle {
                    self.cursor_syllable_mode = false;
                    self.last_op_was_pinyin_edit = false;
                } else {
                    self.last_op_was_pinyin_edit = true;
                }
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Delete => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.full_sentence_selected = false;
                let pos = self.cursor_pos.min(self.input_buffer.len());
                if pos < self.input_buffer.len() {
                    self.input_buffer.remove(pos);
                    self.shift_pins_on_delete(pos);
                    self.last_op_was_pinyin_edit = true;
                    self.current_page = 0;
                    self.selected_candidate_index = 0;
                    if self.input_buffer.is_empty() {
                        self.reset_input_state();
                        return KeyAction::EatAndUpdateUi(self.build_view_model());
                    }
                    self.recompute_candidates();
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Escape => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.reset_input_state();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Up => {
                if self.input_buffer.is_empty() || self.current_candidates.is_empty() {
                    return KeyAction::PassThrough;
                }
                if self.full_sentence_selected {
                    // 已在「整句」行时保持不动
                } else if self.selected_candidate_index > 0 {
                    self.selected_candidate_index -= 1;
                } else if self.current_page > 0 {
                    self.current_page -= 1;
                    let cnt = self.visible_candidate_count();
                    self.selected_candidate_index = cnt.saturating_sub(1);
                } else if !self.last_full_sentence.is_empty() {
                    // 当处于第 1 页第 1 个候选词时，按上方向键 `↑` 向上跳选到正上方的「整句」行，按空格即可整句上屏！
                    self.full_sentence_selected = true;
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Down => {
                if self.input_buffer.is_empty() || self.current_candidates.is_empty() {
                    return KeyAction::PassThrough;
                }
                if self.full_sentence_selected {
                    // 从「整句」行按 `↓` 回到下方第 1 个候选词
                    self.full_sentence_selected = false;
                    self.selected_candidate_index = 0;
                    return KeyAction::EatAndUpdateUi(self.build_view_model());
                }
                let cnt = self.visible_candidate_count();
                if self.selected_candidate_index + 1 < cnt {
                    self.selected_candidate_index += 1;
                } else {
                    let max_page =
                        self.current_candidates.len().saturating_sub(1) / self.page_size;
                    if self.current_page < max_page {
                        self.current_page += 1;
                        self.selected_candidate_index = 0;
                    }
                    // 已在最后一项时保持不动
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Left | VirtualKey::CtrlLeft => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                // ← 键按整个音节向前跳，并总是停在音节最前端（如 `wo|shi` → `wo|shi` 的 `|shi` → `|wo`）
                let pos = if !self.cursor_syllable_mode && self.cursor_pos >= self.input_buffer.len() {
                    self.input_buffer.len()
                } else {
                    self.cursor_pos.min(self.input_buffer.len())
                };
                let target = if self.last_syllable_spans.is_empty() {
                    Some(pos.saturating_sub(1))
                } else {
                    self.previous_syllable_start(pos).or(Some(0))
                };
                if let Some(t) = target {
                    self.cursor_pos = t;
                }
                self.cursor_syllable_mode = true;
                self.cursor_auto_advanced = false;
                self.last_op_was_pinyin_edit = false;
                self.full_sentence_selected = false;
                self.current_page = 0;
                self.selected_candidate_index = 0;
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Right => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                // → 键逐个字母向后移动，便于在音节内部精确改错
                if self.cursor_pos < self.input_buffer.len() {
                    self.cursor_pos += 1;
                }
                if self.cursor_pos >= self.input_buffer.len() {
                    // 光标回到拼音最右端末尾，恢复整句首选模式
                    self.cursor_syllable_mode = false;
                } else {
                    self.cursor_syllable_mode = true;
                }
                self.last_op_was_pinyin_edit = false;
                self.cursor_auto_advanced = false;
                self.full_sentence_selected = false;
                self.current_page = 0;
                self.selected_candidate_index = 0;
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::CtrlRight => {
                if self.input_buffer.is_empty() || self.last_syllable_spans.is_empty() {
                    return KeyAction::PassThrough;
                }
                // Ctrl + → 跳到下一个音节的最前端；已在最后一个音节时回到整句末尾
                match self.active_syllable_index() {
                    Some(idx) if idx + 1 < self.last_syllable_spans.len() => {
                        self.cursor_pos = self.last_syllable_spans[idx + 1].start;
                        self.cursor_syllable_mode = true;
                    }
                    _ => {
                        self.cursor_pos = self.input_buffer.len();
                        self.cursor_syllable_mode = false;
                    }
                }
                self.cursor_auto_advanced = false;
                self.last_op_was_pinyin_edit = false;
                self.full_sentence_selected = false;
                self.current_page = 0;
                self.selected_candidate_index = 0;
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Home => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.cursor_pos = self
                    .last_syllable_spans
                    .first()
                    .map(|s| s.start)
                    .unwrap_or(0);
                self.cursor_syllable_mode = true;
                self.cursor_auto_advanced = false;
                self.last_op_was_pinyin_edit = false;
                self.full_sentence_selected = false;
                self.current_page = 0;
                self.selected_candidate_index = 0;
                self.recompute_candidates();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::End => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.return_cursor_to_end();
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Space => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                // 若已通过按上方向键 `↑` 高亮选中了「整句」行，按空格直接将整句上屏；
                // 否则提交当前高亮选中的候选词（与界面视觉高亮完全一致）
                if self.full_sentence_selected && !self.last_full_sentence.is_empty() {
                    return self.commit_full_sentence();
                }
                self.commit_candidate_at_visible_index(self.selected_candidate_index)
            }
            VirtualKey::Digit(d) => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.full_sentence_selected = false;
                let idx = (d as usize).saturating_sub(1);
                if idx < self.page_size {
                    self.commit_candidate_at_visible_index(idx)
                } else {
                    KeyAction::EatAndUpdateUi(self.build_view_model())
                }
            }
            VirtualKey::Enter => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                if self.full_sentence_selected && !self.last_full_sentence.is_empty() {
                    return self.commit_full_sentence();
                }
                // 回车键直接将当前已锁定的前缀词与剩余原始英文字母串一并上屏
                let raw = self.format_raw_with_pins();
                self.reset_input_state();
                self.append_history(&raw);
                KeyAction::EatAndCommit {
                    committed_text: raw,
                    ghost_text: None,
                }
            }
            VirtualKey::Tab => {
                // 如果当前无输入缓冲但存在 AI 预测的 Ghost Text，按 Tab 键一键上屏补全！
                if self.input_buffer.is_empty() {
                    if let Some(ghost) = self.pending_ghost_text.take() {
                        self.append_history(&ghost);
                        return KeyAction::EatAndCommit {
                            committed_text: ghost,
                            ghost_text: None,
                        };
                    }
                    return KeyAction::PassThrough;
                }
                // 如果正在输入，按 Tab 直接跳选列表中的第一个纯英文候选（方便快速切英文）
                if let Some(eng_pos) = self
                    .current_candidates
                    .iter()
                    .position(|c| c.text.is_ascii())
                {
                    return self.commit_candidate_global_index(eng_pos);
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::PageDown => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.full_sentence_selected = false;
                let max_page = self.current_candidates.len().saturating_sub(1) / self.page_size;
                if self.current_page < max_page {
                    self.current_page += 1;
                    self.selected_candidate_index = 0;
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::PageUp => {
                if self.input_buffer.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.full_sentence_selected = false;
                self.current_page = self.current_page.saturating_sub(1);
                self.selected_candidate_index = 0;
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Other(_) => KeyAction::PassThrough,
        }
    }

    pub fn recompute_candidates(&mut self) {
        self.cursor_pos = self.cursor_pos.min(self.input_buffer.len());
        let context: &str = self
            .host_context
            .as_deref()
            .unwrap_or(&self.committed_history);
        let l1 = self.core.decode_with_pins(
            &self.input_buffer,
            self.active_scene,
            context,
            &self.pinned_segments,
        );
        let user_dict = &self.core.user_dict;
        let code = self.input_buffer.as_str();
        let mut l1_candidates = l1.candidates;
        if self.context_memory_enabled && self.pinned_segments.is_empty() {
            // 用户曾在同样的上文末尾为这串拼音改选过：直接把那个词放到首位
            if let Some(chosen) = user_dict.context_choice(context, code) {
                let n = code.len();
                if let Some(pos) = l1_candidates
                    .iter()
                    .position(|c| c.consumed_len == n && c.text == chosen)
                {
                    let mut item = l1_candidates.remove(pos);
                    item.tag = "[记忆·上文]".to_string();
                    item.final_score = 150.0;
                    l1_candidates.insert(0, item);
                }
            }
        }
        // 简拼匹配到的全部记录（同一个词的几条都要，见 `user_memory`）：调度器第一次要某个候选的个人记忆时才查，
        // 每次重算至多查一次
        let jianpin: std::cell::OnceCell<Vec<UserWordRecord>> = std::cell::OnceCell::new();
        let memory = |text: &str| {
            let recalled = jianpin.get_or_init(|| user_dict.jianpin_matches(code));
            user_memory(user_dict, code, text, recalled)
        };
        let l2 = self.ai.rerank_with_user_priors(
            code,
            context,
            self.active_scene,
            l1_candidates,
            &memory,
        );
        if let Some(first) = l2.candidates.first() {
            self.last_full_sentence = first.text.clone();
            self.last_full_segments = first.segment_spans.clone();
            self.last_syllable_spans = first.syllable_spans.clone();
        } else {
            self.last_full_sentence.clear();
            self.last_full_segments.clear();
            self.last_syllable_spans.clear();
        }

        // 若光标移动到了某个拼音音节上，则候选词列表自动联动为该音节起始的候选词（与主流输入法一致）
        if let Some(syl_idx) = self.active_syllable_index() {
            if let Some(syl_span) = self.last_syllable_spans.get(syl_idx).cloned() {
                let prev_word: Option<String> = if syl_span.start > 0 {
                    self.pinned_segments
                        .iter()
                        .filter(|(_, (end, _))| *end <= syl_span.start)
                        .max_by_key(|(_, (end, _))| *end)
                        .map(|(_, (_, txt))| txt.clone())
                        .or_else(|| {
                            self.last_full_segments
                                .iter()
                                .filter(|s| s.end <= syl_span.start)
                                .max_by_key(|s| s.end)
                                .map(|s| s.text.clone())
                        })
                } else {
                    None
                };
                let mut local_cands = self.core.query_segment_alternatives_with_prev(
                    &self.input_buffer,
                    syl_span.start,
                    syl_span.end,
                    prev_word.as_deref(),
                );
                if !self.pinned_segments.is_empty() {
                    local_cands.retain(|c| {
                        let c_end = c
                            .segment_spans
                            .last()
                            .map(|s| s.end)
                            .unwrap_or(syl_span.end);
                        !self
                            .pinned_segments
                            .iter()
                            .any(|(&ps, _)| ps > syl_span.start && ps < c_end)
                    });
                }
                let n = self.input_buffer.len();
                // 当用户已选定至少一个词块（`!self.pinned_segments.is_empty()`）、当前音节之前已无未选音节（`first_unpinned >= syl_span.start`）
                // 且当前音节之后仍有音节时，将从当前音节直到句尾的合成句（如选定「我来」后的「测试一下」，或先选定后方音节跳回句首后的完整合成句）排在第 1 位，
                // 其后依次为当前音节起始的多字词组与单字。
                if syl_span.end < n
                    && !self.pinned_segments.is_empty()
                    && self.first_unpinned_offset() >= syl_span.start
                {
                    let suffix_syls: Vec<SegmentSpan> = self
                        .last_syllable_spans
                        .iter()
                        .filter(|s| s.start >= syl_span.start)
                        .cloned()
                        .collect();
                    let suffix_segs: Vec<SegmentSpan> = if self
                        .last_full_segments
                        .iter()
                        .any(|s| s.start == syl_span.start)
                    {
                        self.last_full_segments
                            .iter()
                            .filter(|s| s.start >= syl_span.start)
                            .cloned()
                            .collect()
                    } else if let Some(split_seg) = self
                        .last_full_segments
                        .iter()
                        .find(|s| s.start < syl_span.start && s.end > syl_span.start)
                    {
                        let mut segs: Vec<SegmentSpan> = suffix_syls
                            .iter()
                            .filter(|s| s.end <= split_seg.end)
                            .cloned()
                            .collect();
                        segs.extend(
                            self.last_full_segments
                                .iter()
                                .filter(|s| s.start >= split_seg.end)
                                .cloned(),
                        );
                        segs
                    } else {
                        suffix_syls.clone()
                    };
                    let suffix_text = Self::format_segment_spans(&suffix_segs);
                    if !suffix_text.is_empty() {
                        if let Some(pos) = local_cands.iter().position(|c| {
                            c.text == suffix_text
                                && c.segment_spans.last().map(|s| s.end).unwrap_or(0) >= n
                        }) {
                            if pos > 0 {
                                let item = local_cands.remove(pos);
                                local_cands.insert(0, item);
                            }
                        } else {
                            let suffix_preedit = suffix_syls
                                .iter()
                                .map(|s| s.code_key.as_str())
                                .collect::<Vec<_>>()
                                .join("'");
                            local_cands.insert(
                                0,
                                Candidate {
                                    text: suffix_text.clone(),
                                    preedit: suffix_preedit,
                                    consumed_len: n,
                                    l1_cost: 0.1,
                                    final_score: 140.0,
                                    ai_boosted: false,
                                    tag: "[余下整句]".to_string(),
                                    segments: suffix_segs.iter().map(|s| s.text.clone()).collect(),
                                    segment_spans: suffix_segs.clone(),
                                    syllable_spans: suffix_syls.clone(),
                                },
                            );
                        }
                        if suffix_segs.len() >= 3 {
                            let sub_segs = suffix_segs[..suffix_segs.len() - 1].to_vec();
                            let sub_text = Self::format_segment_spans(&sub_segs);
                            let sub_end = sub_segs.last().unwrap().end;
                            let overlaps_pin = self
                                .pinned_segments
                                .iter()
                                .any(|(&ps, &(pe, _))| ps < sub_end && pe > syl_span.start);
                            if !overlaps_pin
                                && !sub_text.is_empty()
                                && !local_cands.iter().any(|c| c.text == sub_text)
                            {
                                let sub_syls: Vec<SegmentSpan> = suffix_syls
                                    .iter()
                                    .filter(|s| s.end <= sub_end)
                                    .cloned()
                                    .collect();
                                let sub_preedit = sub_syls
                                    .iter()
                                    .map(|s| s.code_key.as_str())
                                    .collect::<Vec<_>>()
                                    .join("'");
                                local_cands.insert(
                                    1.min(local_cands.len()),
                                    Candidate {
                                        text: sub_text,
                                        preedit: sub_preedit,
                                        consumed_len: sub_end.saturating_sub(syl_span.start),
                                        l1_cost: 0.2,
                                        final_score: 138.0,
                                        ai_boosted: false,
                                        tag: "[前缀选词]".to_string(),
                                        segments: sub_segs.iter().map(|s| s.text.clone()).collect(),
                                        segment_spans: sub_segs,
                                        syllable_spans: sub_syls,
                                    },
                                );
                            }
                        }
                    }
                }
                if !local_cands.is_empty() {
                    self.current_candidates = local_cands;
                    self.last_ai_hint = format!(
                        "光标位于音节 '{}' (第 {}/{} 音节) —— 可继续向后选词，或按退格撤销锁定/修改拼音",
                        syl_span.code_key,
                        syl_idx + 1,
                        self.last_syllable_spans.len()
                    );
                    return;
                }
            }
        }

        self.current_candidates = l2.candidates;
        self.last_ai_hint = l2.active_semantic_hint;
        // 表情放在 AI 重排之后：不进打分池；句中逐段选词（有固定的词块）时不插
        if self.emoji_candidates_enabled && self.pinned_segments.is_empty() {
            emoji::insert_emoji_candidates(&mut self.current_candidates, self.input_buffer.len());
        }
    }

    /// 当前首选整句在拼音 `[start, end)` 上是否正好由几个完整的词块组成，拼起来（忽略显示用空格）就是 `text`。
    /// 句中选定这样的词，只是接受了整句里原来的字
    fn shown_sentence_spells(&self, start: usize, end: usize, text: &str) -> bool {
        let shown: Vec<&SegmentSpan> = self
            .last_full_segments
            .iter()
            .filter(|s| s.start >= start && s.end <= end)
            .collect();
        shown.first().is_some_and(|s| s.start == start)
            && shown.last().is_some_and(|s| s.end == end)
            && strip_whitespace(&shown.iter().map(|s| s.text.as_str()).collect::<String>())
                == strip_whitespace(text)
    }

    fn format_segment_spans(spans: &[SegmentSpan]) -> String {
        let mut out = String::new();
        let mut prev_is_ascii: Option<bool> = None;
        for sp in spans {
            if sp.text.is_empty() {
                continue;
            }
            let curr_is_ascii = sp.text.is_ascii();
            if let Some(prev_ascii) = prev_is_ascii {
                if prev_ascii != curr_is_ascii || (prev_ascii && curr_is_ascii) {
                    out.push(' ');
                }
            }
            out.push_str(&sp.text);
            prev_is_ascii = Some(curr_is_ascii);
        }
        out
    }

    /// 当后台异步大模型返回结果或设置变更时，刷新当前候选列表并返回最新视图模型
    pub fn refresh_candidates(&mut self) -> CandidateWindowViewModel {
        if !self.input_buffer.is_empty() {
            self.recompute_candidates();
        }
        self.build_view_model()
    }

    /// 后台大模型异步仲裁完成后的刷新：
    /// 若用户此时正在翻页、用 ↑↓ 选中了非首项或选中了整句行，则不立即重排，
    /// 避免候选在用户按数字键的瞬间突然变动导致选错词（结果已写入缓存，下一次击键自然生效）。
    pub fn refresh_candidates_after_ai(&mut self) -> CandidateWindowViewModel {
        let user_is_navigating = self.current_page > 0
            || self.selected_candidate_index > 0
            || self.full_sentence_selected;
        if !self.input_buffer.is_empty() && !user_is_navigating {
            self.recompute_candidates();
        }
        self.build_view_model()
    }

    /// 动态应用并保存 AI 模型与模糊音配置
    pub fn apply_persistent_config(&mut self, cfg: &ImePersistentConfig) {
        self.core.fuzzy_config = cfg.fuzzy_config.clone();
        self.ai.cloud_client.config = cfg.ai_config.clone();
        if !self.input_buffer.is_empty() {
            self.recompute_candidates();
        }
    }

    /// 直接将当前实时合成的整句首选上屏
    pub fn commit_full_sentence(&mut self) -> KeyAction {
        if self.last_full_sentence.is_empty() {
            return KeyAction::EatAndUpdateUi(self.build_view_model());
        }
        let full_text = self.last_full_sentence.clone();
        let full_code = self.input_buffer.clone();
        let has_pins = !self.pinned_segments.is_empty();
        // 主动改选：上屏的整句与第一次锁定之前显示的整句不同。只是逐段接受了显示的结果（选了前缀「我来」再按空格），
        // 与空格上屏没有区别
        let changed_by_user =
            has_pins && strip_whitespace(&full_text) != strip_whitespace(&self.sentence_before_pins);
        self.ai.invalidate_cache_for_pinyin(&full_code);
        let full_segments = self.last_full_segments.clone();
        // 仅在用户从句首起主动逐段选字造词（`!had_auto_prefix_lock` 且总长 <= 4 字）或单段短词（<= 4 字）时将整串记入词表；
        // 若用户只是把光标移到句中改字（自动锁定了前面未选的默认前缀）或输入多段长句，则只学习相邻词块的二元搭配（Bigram），
        // 避免把一次句中改字（如在 `gan jue |ying gai` 中把 `ying` 选为「影」合成「感觉影该」）误造成高权重四字词而污染后续输入！
        let full_char_cnt = full_text.chars().count();
        if (has_pins && !self.had_auto_prefix_lock && full_char_cnt <= 4)
            || (!has_pins && full_segments.len() <= 1 && full_char_cnt <= 4)
        {
            self.core
                .commit_candidate_ex(&full_code, &full_text, None, changed_by_user);
        }
        if has_pins {
            for pair in full_segments.windows(2) {
                self.core.learn_bigram(&pair[0].text, &pair[1].text);
            }
        }
        for seg in &full_segments {
            if seg.start < seg.end && seg.end <= full_code.len() {
                let seg_code = &full_code[seg.start..seg.end];
                if seg_code != full_code {
                    self.ai.invalidate_cache_for_pinyin(seg_code);
                }
            }
        }
        self.reset_input_state();
        self.append_history(&full_text);

        let ghost = self.predict_ghost();
        self.pending_ghost_text = ghost.clone();

        KeyAction::EatAndCommit {
            committed_text: full_text,
            ghost_text: ghost,
        }
    }

    fn commit_candidate_at_visible_index(&mut self, visible_idx: usize) -> KeyAction {
        let global_idx = self.current_page * self.page_size + visible_idx;
        self.commit_candidate_global_index(global_idx)
    }

    fn commit_candidate_global_index(&mut self, global_idx: usize) -> KeyAction {
        if global_idx >= self.current_candidates.len() {
            return KeyAction::EatAndUpdateUi(self.build_view_model());
        }
        let chosen = self.current_candidates[global_idx].clone();
        if chosen.tag == emoji::EMOJI_TAG {
            // 表情只上屏，不记进个人词库，也不学搭配
            self.reset_input_state();
            self.append_history(&chosen.text);
            let ghost = self.predict_ghost();
            self.pending_ghost_text = ghost.clone();
            return KeyAction::EatAndCommit {
                committed_text: chosen.text,
                ghost_text: ghost,
            };
        }
        let n = self.input_buffer.len();
        let span_start = chosen.segment_spans.first().map(|s| s.start).unwrap_or(0);
        let span_end = chosen
            .segment_spans
            .last()
            .map(|s| s.end)
            .unwrap_or(chosen.consumed_len)
            .min(n);

        // 统一渐进分词锁定逻辑（无论在句尾选中「[前缀选词]」还是将光标置于某音节选词）：
        // 若选中的是局部词块（`span_start > 0 || span_end < n`），则将该词块锁定在 `pinned_segments` 中：
        // - 若全部音节均已选定（`first_unpinned >= n`），立即合成完整句子一并上屏！
        // - 若所选词块前方仍有未选音节（`first_unpinned < span_start`），立即跳回前方首个未选音节供用户选字；
        // - 否则向后推进到下一个未选音节（如选定「我来」后从 `ce shi yi xia` 开始）！
        if self.active_syllable_index().is_some() || span_start > 0 || span_end < n {
            let picked_on_tapped_syllable = self.on_tapped_syllable();
            if !chosen.segment_spans.is_empty() {
                // 选的就是整句里这一段原来的字（如选前缀「我来」、改了前面的字之后按空格接受后面的「失败」）：
                // 只是接受了显示的结果，按空格上屏记，不算主动改选
                let accepts_shown = self.shown_sentence_spells(span_start, span_end, &chosen.text);
                if self.pinned_segments.is_empty() {
                    // 第一次锁定：记下锁定之前用户看到的整句，整句上屏时据此判断有没有改过
                    self.sentence_before_pins = self.last_full_sentence.clone();
                }
                if span_start > 0 && self.first_unpinned_offset() < span_start {
                    self.had_auto_prefix_lock = true;
                }
                for seg in &chosen.segment_spans {
                    let s_start = seg.start.min(n);
                    let s_end = seg.end.min(n);
                    if s_start >= s_end {
                        continue;
                    }
                    self.pinned_segments
                        .retain(|&s, &mut (e, _)| e <= s_start || s >= s_end);
                    self.pinned_segments
                        .insert(s_start, (s_end, seg.text.clone()));
                }
                let slice_code = self.input_buffer[span_start..span_end].to_string();
                let prev_word = self
                    .last_full_segments
                    .iter()
                    .find(|s| s.end == span_start)
                    .map(|s| s.text.clone())
                    .or_else(|| {
                        self.pinned_segments
                            .iter()
                            .find(|(_, &(e, _))| e == span_start)
                            .map(|(_, (_, txt))| txt.clone())
                    });
                let is_single_syllable = ime_core::pinyin::is_valid_syllable(&slice_code)
                    || slice_code.len() <= 1
                    || chosen.text.chars().count() <= 1;
                if is_single_syllable {
                    // 句中单字改选：只学习「上一个词 + 这个字」的搭配，不全局提升该字，
                    // 避免一次把 `de` 选成「得」后所有句子里的「的」都变成「得」
                    if let Some(prev) = prev_word.as_deref() {
                        self.core.learn_bigram(prev, &chosen.text);
                    }
                } else if chosen.segment_spans.len() == 1
                    || (!self.had_auto_prefix_lock && chosen.text.chars().count() <= 6)
                {
                    self.core.commit_candidate_ex(
                        &slice_code,
                        &chosen.text,
                        prev_word.as_deref(),
                        !accepts_shown,
                    );
                }
                self.ai.invalidate_cache_for_pinyin(&slice_code);
                self.ai.invalidate_cache_for_pinyin(&self.input_buffer);
            }

            let first_unpinned = self.first_unpinned_offset();
            // 触屏在点的音节上选字：只有选到句尾、整句也都选定了才直接上屏
            if first_unpinned >= n && (!picked_on_tapped_syllable || span_end >= n) {
                self.recompute_candidates();
                return self.commit_full_sentence();
            }
            if picked_on_tapped_syllable {
                // 回到整句（保留选定的字），不自动跳到别的音节：首选是 AI 按这次选字重新排过的整句
                self.return_cursor_to_end();
                return KeyAction::EatAndUpdateUi(self.build_view_model());
            }

            let next_pos = if first_unpinned < span_start {
                first_unpinned
            } else {
                let forward = self.next_unpinned_offset_from(span_end);
                if forward < n {
                    forward
                } else {
                    first_unpinned
                }
            };

            self.cursor_pos = next_pos;
            self.cursor_syllable_mode = true;
            self.cursor_auto_advanced = true;
            self.last_op_was_pinyin_edit = false;
            self.full_sentence_selected = false;
            self.current_page = 0;
            self.selected_candidate_index = 0;
            self.recompute_candidates();
            return KeyAction::EatAndUpdateUi(self.build_view_model());
        }

        let consumed = chosen.consumed_len.min(n);
        let consumed_code = self.input_buffer[..consumed].to_string();
        // 按选词来学（只有一段或不超过 6 个字的整句也记下，多段时再学相邻词块的搭配和其中的词；
        // 不然只记 4 个字以内的单段词）：没要首选；或者有锁定词块（回到整句后选整句）
        let learn_as_choice = global_idx > 0 || !self.pinned_segments.is_empty();
        // 记录里算不算主动改选：没要首选；或者有锁定词块，而选中的整句与第一次锁定之前显示的不同。
        // 与原来显示的相同，说明只是逐段接受了显示的结果：照样按选词来学，只是不算主动改选
        let is_explicit = global_idx > 0
            || (!self.pinned_segments.is_empty()
                && strip_whitespace(&chosen.text) != strip_whitespace(&self.sentence_before_pins));

        if self.context_memory_enabled
            && global_idx > 0
            && consumed == n
            && self.pinned_segments.is_empty()
        {
            // 用户没要首选、改选了别的整词：记住在这段上文下选的是它
            let context = self.context_before_cursor().to_string();
            self.core
                .learn_context_choice(&context, &consumed_code, &chosen.text);
        }

        self.ai.invalidate_cache_for_pinyin(&consumed_code);
        self.ai.invalidate_cache_for_pinyin(&self.input_buffer);

        let chosen_char_cnt = chosen.text.chars().count();
        if (learn_as_choice && (chosen_char_cnt <= 6 || chosen.segment_spans.len() <= 1))
            || (!learn_as_choice && chosen.segment_spans.len() <= 1 && chosen_char_cnt <= 4)
        {
            self.core
                .commit_candidate_ex(&consumed_code, &chosen.text, None, is_explicit);
        }
        if chosen.segment_spans.len() > 1 && learn_as_choice {
            for pair in chosen.segment_spans.windows(2) {
                self.core.learn_bigram(&pair[0].text, &pair[1].text);
            }
            for span in &chosen.segment_spans {
                let seg_chars = span.text.chars().count();
                let is_particle_compound =
                    seg_chars >= 3 && span.text.ends_with(['的', '了', '着', '过']);
                if (2..=4).contains(&seg_chars)
                    && !is_particle_compound
                    && span.start < span.end
                    && span.end <= self.input_buffer.len()
                {
                    let seg_code = &self.input_buffer[span.start..span.end];
                    if seg_code != consumed_code {
                        self.core
                            .commit_candidate_ex(seg_code, &span.text, None, false);
                        self.ai.invalidate_cache_for_pinyin(seg_code);
                    }
                }
            }
        }

        self.reset_input_state();
        self.append_history(&chosen.text);

        let ghost = self.predict_ghost();
        self.pending_ghost_text = ghost.clone();

        KeyAction::EatAndCommit {
            committed_text: chosen.text,
            ghost_text: ghost,
        }
    }

    fn append_history(&mut self, text: &str) {
        self.committed_history.push_str(text);
        if self.committed_history.chars().count() > HISTORY_MAX_CHARS {
            let chars: Vec<char> = self.committed_history.chars().collect();
            self.committed_history = chars[chars.len() - HISTORY_MAX_CHARS..].iter().collect();
        }
        // 宿主提供了光标前文本时，上屏的文字紧接在它后面，保持上文新鲜直到宿主下次刷新
        if let Some(ctx) = self.host_context.as_mut() {
            ctx.push_str(text);
            if ctx.chars().count() > HISTORY_MAX_CHARS {
                *ctx = Self::tail_chars(ctx, HISTORY_MAX_CHARS);
            }
        }
    }

    pub fn build_view_model(&self) -> CandidateWindowViewModel {
        if self.input_buffer.is_empty() {
            return CandidateWindowViewModel {
                visible: false,
                ghost_text_preview: self.pending_ghost_text.clone(),
                ..Default::default()
            };
        }
        let start = self.current_page * self.page_size;
        let end = (start + self.page_size).min(self.current_candidates.len());
        let visible_candidates = if start < end {
            self.current_candidates[start..end].to_vec()
        } else {
            Vec::new()
        };
        let selected_idx = if visible_candidates.is_empty() {
            0
        } else {
            self.selected_candidate_index
                .min(visible_candidates.len() - 1)
        };
        let active_syl = self.active_syllable_index();
        let preedit = if !self.last_syllable_spans.is_empty() {
            self.last_syllable_spans
                .iter()
                .map(|s| s.code_key.as_str())
                .collect::<Vec<_>>()
                .join("'")
        } else {
            self.input_buffer.clone()
        };

        let mut full_sentence_parts: Vec<(String, bool)> = Vec::new();
        if self.last_full_segments.is_empty() || self.pinned_segments.is_empty() {
            if !self.last_full_sentence.is_empty() {
                full_sentence_parts.push((self.last_full_sentence.clone(), false));
            }
        } else {
            let full = self.last_full_sentence.as_str();
            let mut byte_cursor = 0usize;
            for seg in &self.last_full_segments {
                if seg.text.is_empty() {
                    continue;
                }
                let is_pinned = self
                    .pinned_segments
                    .iter()
                    .any(|(&s, &(e, _))| seg.start >= s && seg.end <= e);
                if let Some(rel) = full[byte_cursor..].find(&seg.text) {
                    let prefix_gap = &full[byte_cursor..byte_cursor + rel];
                    if !prefix_gap.is_empty() {
                        let gap_pinned = is_pinned
                            && full_sentence_parts.last().is_some_and(|last| last.1);
                        if let Some(last) = full_sentence_parts.last_mut() {
                            if last.1 == gap_pinned {
                                last.0.push_str(prefix_gap);
                            } else {
                                full_sentence_parts.push((prefix_gap.to_string(), gap_pinned));
                            }
                        } else {
                            full_sentence_parts.push((prefix_gap.to_string(), false));
                        }
                    }
                    let seg_end = byte_cursor + rel + seg.text.len();
                    let piece = &full[byte_cursor + rel..seg_end];
                    if let Some(last) = full_sentence_parts.last_mut() {
                        if last.1 == is_pinned {
                            last.0.push_str(piece);
                        } else {
                            full_sentence_parts.push((piece.to_string(), is_pinned));
                        }
                    } else {
                        full_sentence_parts.push((piece.to_string(), is_pinned));
                    }
                    byte_cursor = seg_end;
                } else if let Some(last) = full_sentence_parts.last_mut() {
                    if last.1 == is_pinned {
                        last.0.push_str(&seg.text);
                    } else {
                        full_sentence_parts.push((seg.text.clone(), is_pinned));
                    }
                } else {
                    full_sentence_parts.push((seg.text.clone(), is_pinned));
                }
            }
            if byte_cursor < full.len() {
                let tail = &full[byte_cursor..];
                if let Some(last) = full_sentence_parts.last_mut() {
                    last.0.push_str(tail);
                } else {
                    full_sentence_parts.push((tail.to_string(), false));
                }
            }
        }

        CandidateWindowViewModel {
            visible: true,
            preedit_text: preedit,
            page_index: self.current_page,
            page_size: self.page_size,
            total_candidates: self.current_candidates.len(),
            visible_candidates,
            selected_candidate_index: selected_idx,
            full_sentence_selected: self.full_sentence_selected,
            active_scene_label: self.active_scene.label().to_string(),
            ai_semantic_hint: self.last_ai_hint.clone(),
            ghost_text_preview: None,
            composed_full_sentence: self.last_full_sentence.clone(),
            full_sentence_parts,
            syllable_spans: self.last_syllable_spans.clone(),
            top_segments: self.last_full_segments.clone(),
            cursor_byte_pos: self.cursor_pos,
            active_syllable_index: active_syl,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ime_lexicon::CompiledLexicon;
    use std::path::PathBuf;

    #[test]
    fn test_tsf_key_session_workflow_and_ghost_text() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let ai = DualModeAiOrchestrator::default();
        let mut session = KeySession::new(core, ai);

        // 模拟切换到 VS Code 进程
        session.on_foreground_process_changed("Code.exe");
        assert_eq!(session.active_scene, AppScene::CodeEditor);

        // 模拟逐字母敲击 "wozaiyongrust"
        for ch in "wozaiyongrust".chars() {
            let act = session.process_key(VirtualKey::Char(ch));
            assert!(matches!(act, KeyAction::EatAndUpdateUi(_)));
        }

        // 按空格键首选上屏 -> 应输出 "我在用 Rust" 并触发 AI Ghost Text 预测
        let commit_act = session.process_key(VirtualKey::Space);
        match commit_act {
            KeyAction::EatAndCommit {
                committed_text,
                ghost_text,
            } => {
                assert_eq!(committed_text, "我在用 Rust");
                assert!(ghost_text.is_some());
            }
            _ => panic!("Expected EatAndCommit"),
        }

        // 紧接着按 Tab 键一键采纳 AI Ghost Text 续写
        let tab_act = session.process_key(VirtualKey::Tab);
        assert!(matches!(
            tab_act,
            KeyAction::EatAndCommit {
                committed_text,
                ..
            } if committed_text.contains("输入法")
        ));
    }

    #[test]
    fn test_pinyin_cursor_navigation_typo_edit_and_syllable_selection() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let ai = DualModeAiOrchestrator::default();
        let mut session = KeySession::new(core, ai);
        session.active_scene = AppScene::CodeEditor;

        // 1. 测试句中拼音输错回退修改：将 "wozaiyongrustxieapi" 故意输错成 "wozaiyongrustxirapi" (xie -> xir)
        for ch in "wozaiyongrustxirapi".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        // ← 键按整音节跳：每次都落在某个音节的最前端
        let syl_starts: Vec<usize> = session.last_syllable_spans.iter().map(|s| s.start).collect();
        session.process_key(VirtualKey::Left);
        assert_eq!(session.cursor_pos, *syl_starts.last().unwrap());
        assert_eq!(
            session.active_syllable_index(),
            Some(syl_starts.len() - 1),
            "光标在音节最前端时应高亮该音节"
        );
        // 继续按 ← 直到越过错误字母 'r'（byte 15），再用 → 逐字母移动到 'r' 之后（byte 16）
        while session.cursor_pos > 15 {
            let before = session.cursor_pos;
            session.process_key(VirtualKey::Left);
            assert!(syl_starts.contains(&session.cursor_pos));
            assert!(session.cursor_pos < before);
        }
        while session.cursor_pos < 16 {
            session.process_key(VirtualKey::Right);
        }
        assert_eq!(session.cursor_pos, 16);
        // 按退格删除错误字母 'r'，再输入正确字母 'e'
        session.process_key(VirtualKey::Backspace);
        session.process_key(VirtualKey::Char('e'));
        assert_eq!(session.input_buffer, "wozaiyongrustxieapi");
        assert_eq!(session.last_full_sentence, "我在用 Rust 写 API");
        // 修改完句中拼音后，默认焦点在当前音节候选词上；按 ↑ 键选中「整句」行，按空格将修正后的整句上屏！
        assert!(!session.build_view_model().full_sentence_selected);
        session.process_key(VirtualKey::Up);
        assert!(session.build_view_model().full_sentence_selected);
        let commit_act = session.process_key(VirtualKey::Space);
        assert_eq!(
            commit_act,
            KeyAction::EatAndCommit {
                committed_text: "我在用 Rust 写 API".to_string(),
                ghost_text: None,
            }
        );

        // 1b. 测试用户真实场景：误输 "youxiaojiao"，按 ← 跳到 |jiao 后按退格删掉 xiao 的 'o' 变成 "youxiajiao"：
        // 此时默认焦点仍在 jiao 的候选词（1 叫），按 ↑ 键可向上选中「整句 右下角」，按 ↓ 键可退回 `1 叫`，按 ↑ 再按空格即输出「右下角」！
        for ch in "youxiaojiao".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        session.process_key(VirtualKey::Left); // 跳到 |jiao
        session.process_key(VirtualKey::Backspace); // 删掉 xiao 末尾的 'o'
        let vm_yxj = session.build_view_model();
        assert_eq!(session.input_buffer, "youxiajiao");
        assert_eq!(vm_yxj.composed_full_sentence, "右下角");
        assert!(!vm_yxj.full_sentence_selected);
        assert_eq!(vm_yxj.selected_candidate_index, 0);
        // 按 ↑ 选中「整句 右下角」
        session.process_key(VirtualKey::Up);
        assert!(session.build_view_model().full_sentence_selected);
        // 按 ↓ 退回下方第 1 候选词
        session.process_key(VirtualKey::Down);
        assert!(!session.build_view_model().full_sentence_selected);
        assert_eq!(session.build_view_model().selected_candidate_index, 0);
        // 再次按 ↑ 选中「整句 右下角」并按空格上屏
        session.process_key(VirtualKey::Up);
        assert!(session.build_view_model().full_sentence_selected);
        let commit_yxj = session.process_key(VirtualKey::Space);
        assert_eq!(
            commit_yxj,
            KeyAction::EatAndCommit {
                committed_text: "右下角".to_string(),
                ghost_text: None,
            }
        );

        // 2. 测试光标移动到某个拼音音节上，选词列表联动对应并局部换词：
        for ch in "woshiyongclaudedeapizuochengxu".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        // 点击第 1 个音节 "shi" (wo'shi'yong 的 shi, index = 1)
        session.click_pinyin_syllable(1, None);
        assert_eq!(session.active_syllable_index(), Some(1));
        let vm = session.build_view_model();
        // 此时候选列表应自动切换为从 "shi" 出发的候选词（含 "使用", "是用", "是"...）
        assert!(vm.visible_candidates.iter().any(|c| c.text == "是用" || c.text == "是"));
        let pos = vm
            .visible_candidates
            .iter()
            .position(|c| c.text == "是用" || c.text == "是")
            .unwrap();
        session.process_key(VirtualKey::Digit((pos + 1) as u8));
        let vm_after = session.build_view_model();
        // 选定后整句更新为 "我是用 Claude 的 API 做程序"，且光标自动推进到后续未锁定音节
        assert_eq!(vm_after.composed_full_sentence, "我是用 Claude 的 API 做程序");
        assert!(vm_after.full_sentence_parts.iter().any(|(_, pinned)| *pinned));
        let final_commit = session.process_key(VirtualKey::Space);
        assert_eq!(
            final_commit,
            KeyAction::EatAndCommit {
                committed_text: "我是用 Claude 的 API 做程序".to_string(),
                ghost_text: None,
            }
        );

        // 3. 测试「我来测试一下」(wolaiceshiyixia) 渐进选词：
        // 选定「我来」→ 整句中「我来」高亮标记为已锁定、备选中不再出现「我来」、光标与备选从 ce shi yi xia 开始 → 选定「测试」→ 选定「一下」完成整句上屏
        for ch in "wolaiceshiyixia".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_init = session.build_view_model();
        let wolai_idx = vm_init
            .visible_candidates
            .iter()
            .position(|c| c.text == "我来")
            .expect("Expected '我来' in initial prefix candidates");
        session.process_key(VirtualKey::Digit((wolai_idx + 1) as u8));

        let vm_step1 = session.build_view_model();
        // 整句预览中「我来」应标记为已选定 (is_pinned = true)，余下「测试一下」为未选定 (false)
        assert_eq!(
            vm_step1.full_sentence_parts,
            vec![("我来".to_string(), true), ("测试一下".to_string(), false)]
        );
        // 光标应推进到 `ce` (byte 5, 第 3 个音节 index = 2)
        assert_eq!(vm_step1.cursor_byte_pos, 5);
        assert_eq!(vm_step1.active_syllable_index, Some(2));
        // 已选定的「我来」或「我」绝不应再出现在备选列表中，且备选应从 ce shi yi xia 开始（包含「测试一下」「测试」等）
        assert!(!vm_step1.visible_candidates.iter().any(|c| c.text.starts_with('我')));
        assert_eq!(vm_step1.visible_candidates[0].text, "测试一下");
        let ceshi_idx = vm_step1
            .visible_candidates
            .iter()
            .position(|c| c.text == "测试")
            .expect("Expected '测试' in candidates starting at ce");
        session.process_key(VirtualKey::Digit((ceshi_idx + 1) as u8));

        let vm_step2 = session.build_view_model();
        // 此时「我来测试」均已锁定高亮，光标推进到 `yi` (byte 10, index = 4)
        assert_eq!(
            vm_step2.full_sentence_parts,
            vec![("我来测试".to_string(), true), ("一下".to_string(), false)]
        );
        assert_eq!(vm_step2.cursor_byte_pos, 10);
        assert_eq!(vm_step2.active_syllable_index, Some(4));
        assert!(!vm_step2.visible_candidates.iter().any(|c| c.text == "测试"));
        assert_eq!(vm_step2.visible_candidates[0].text, "一下");

        // 再选定「一下」到达句尾，自动将完整句子「我来测试一下」上屏！
        let step3_act = session.process_key(VirtualKey::Digit(1));
        assert_eq!(
            step3_act,
            KeyAction::EatAndCommit {
                committed_text: "我来测试一下".to_string(),
                ghost_text: None,
            }
        );

        // 5. 测试在 "wolaiceshi" 中选定前缀「我来」后继续追加输入 "yixia"，新字母应追加到句尾且保持「我来」锁定与 |ce 焦点
        for ch in "wolaiceshi".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let wolai_idx2 = session
            .build_view_model()
            .visible_candidates
            .iter()
            .position(|c| c.text == "我来")
            .unwrap();
        session.process_key(VirtualKey::Digit((wolai_idx2 + 1) as u8));
        for ch in "yixia".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_append = session.build_view_model();
        assert_eq!(
            vm_append.full_sentence_parts,
            vec![("我来".to_string(), true), ("测试一下".to_string(), false)]
        );
        assert_eq!(vm_append.cursor_byte_pos, 5);
        assert_eq!(vm_append.visible_candidates[0].text, "测试一下");
        session.process_key(VirtualKey::Escape);

        // 6. 测试双字前缀选词必须全部排在单字之前（如输入 "nizaifansipaichayixia"，"你在"/"你再"/"拟在" 均排在 "你"/"尼"/"泥" 之前）
        for ch in "nizaifansipaichayixia".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let all_texts: Vec<String> = session
            .current_candidates
            .iter()
            .map(|c| c.text.clone())
            .collect();
        let pos_nizai1 = all_texts.iter().position(|t| t == "你在").expect("Expected '你在'");
        let pos_nizai2 = all_texts.iter().position(|t| t == "你再").expect("Expected '你再'");
        let pos_nizai3 = all_texts.iter().position(|t| t == "拟在").expect("Expected '拟在'");
        let pos_ni_char = all_texts.iter().position(|t| t == "你").expect("Expected '你'");
        assert!(
            pos_nizai1 < pos_ni_char && pos_nizai2 < pos_ni_char && pos_nizai3 < pos_ni_char,
            "Expected all 2-char prefix words ('你在','你再','拟在') before single-char '你', got: {:?}",
            all_texts
        );
        // 且第一页即可直接选到「你再反思排查一下」或「你再」
        let page1_texts: Vec<String> = session
            .build_view_model()
            .visible_candidates
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert!(
            page1_texts.contains(&"你再".to_string()),
            "Expected '你再' on Page 1, got: {:?}",
            page1_texts
        );
        session.process_key(VirtualKey::Escape);

        // 7a. 测试句中改错拼音（如把 `zaojia` 改为 `zaijia`，或把 `ganjieyinggai` 改为 `ganjueyinggai`）后：
        // - 当尚未手动锁定任何词块时，修改拼音后候选栏必须从句首展示（如 `1 在家  2 在  3 再...` / `1 感觉应该  2 感觉...`），
        //   绝不能因为光标停在音节边界而跳去高亮后方音节（如 `|jia` 或 `|ying`）导致前面的字无法选择！
        for ch in "zaojia".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        session.process_key(VirtualKey::Left); // 跳到 |jia（即 zao 末尾 byte 3）
        session.process_key(VirtualKey::Backspace); // 删 'o'
        session.process_key(VirtualKey::Char('i')); // 补 'i' -> zai|jia
        let vm_zj_edit = session.build_view_model();
        assert_eq!(session.input_buffer, "zaijia");
        assert_eq!(vm_zj_edit.cursor_byte_pos, 3);
        assert_eq!(vm_zj_edit.active_syllable_index, None);
        assert_eq!(vm_zj_edit.visible_candidates[0].text, "在家");
        assert!(
            vm_zj_edit.visible_candidates.iter().any(|c| c.text == "在")
                && vm_zj_edit.visible_candidates.iter().any(|c| c.text == "再"),
            "修改句中拼音后第 1 页应同时包含『在』与『再』两个单字前缀候选，实际: {:?}",
            vm_zj_edit.visible_candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        session.process_key(VirtualKey::Escape);

        for ch in "ganjieyinggai".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        session.process_key(VirtualKey::Left); // 跳到 |gai
        session.process_key(VirtualKey::Left); // 跳到 |ying（即 jie 末尾 byte 6）
        session.process_key(VirtualKey::Backspace); // 删 'e'
        session.process_key(VirtualKey::Backspace); // 删 'i'
        session.process_key(VirtualKey::Char('u'));
        session.process_key(VirtualKey::Char('e'));
        let vm_gj_edit = session.build_view_model();
        assert_eq!(session.input_buffer, "ganjueyinggai");
        assert_eq!(vm_gj_edit.cursor_byte_pos, 6);
        assert_eq!(vm_gj_edit.active_syllable_index, None);
        assert_eq!(vm_gj_edit.visible_candidates[0].text, "感觉应该");
        assert_eq!(vm_gj_edit.visible_candidates[1].text, "感觉");
        assert_eq!(
            vm_gj_edit.full_sentence_parts,
            vec![("感觉应该".to_string(), false)]
        );

        // 7b. 测试按 ← 方向键跳到后方/句中音节选字时，前面尚未选过的音节绝不能被强制跳过，
        // 选完当前音节后光标必须自动跳回前方首个未选音节，让用户可以直接选择前面的字！
        session.process_key(VirtualKey::Escape);
        for ch in "zaijia".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        session.process_key(VirtualKey::Left); // 跳到 |jia
        let vm_zj_left = session.build_view_model();
        assert_eq!(vm_zj_left.active_syllable_index, Some(1));
        assert_eq!(vm_zj_left.visible_candidates[0].text, "家");
        assert_eq!(
            vm_zj_left.full_sentence_parts,
            vec![("在家".to_string(), false)],
            "仅移動光标到 |jia 尚未选字时，前面未选的『在』不应伪装成已选绿框"
        );
        let jia_idx = vm_zj_left
            .visible_candidates
            .iter()
            .position(|c| c.text == "加")
            .expect("Expected '加' in jia candidates");
        session.process_key(VirtualKey::Digit((jia_idx + 1) as u8));
        // 选定后方 `jia -> 加` 后，前方 `zai` 尚未选择，光标必须立即跳回 0 (`|zai`) 供用户选择前面的字！
        let vm_zj_after_jia = session.build_view_model();
        assert_eq!(vm_zj_after_jia.active_syllable_index, Some(0));
        assert_eq!(
            vm_zj_after_jia.full_sentence_parts,
            vec![("在".to_string(), false), ("加".to_string(), true)]
        );
        assert_eq!(vm_zj_after_jia.visible_candidates[0].text, "在加");
        let zai_idx = vm_zj_after_jia
            .visible_candidates
            .iter()
            .position(|c| c.text == "再")
            .expect("Expected '再' in zai candidates after pinning '加'");
        let commit_zj = session.process_key(VirtualKey::Digit((zai_idx + 1) as u8));
        assert_eq!(
            commit_zj,
            KeyAction::EatAndCommit {
                committed_text: "再加".to_string(),
                ghost_text: None,
            }
        );

        // 再测试四音节 `ganjueyinggai`：按 ← 跳到句中 `|ying` 选定单字『影』后，
        // 光标跳回句首 `|gan` 供选前面的『感觉』，选定『感觉』后再自动跳到末尾未选的 `|gai`！
        for ch in "ganjueyinggai".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        session.process_key(VirtualKey::Left); // |gai
        session.process_key(VirtualKey::Left); // |ying
        let vm_gj = session.build_view_model();
        assert_eq!(vm_gj.active_syllable_index, Some(2));
        assert_eq!(vm_gj.visible_candidates[0].text, "应该");
        let ying_char_idx = vm_gj
            .visible_candidates
            .iter()
            .position(|c| c.text == "影")
            .expect("Expected '影' in ying candidates");
        session.process_key(VirtualKey::Digit((ying_char_idx + 1) as u8));
        let vm_gj_after_ying = session.build_view_model();
        assert_eq!(
            vm_gj_after_ying.active_syllable_index,
            Some(0),
            "在 |ying 选定『影』后，由于前方 gan jue 未选，光标应跳回句首 |gan (Some(0))"
        );
        assert_eq!(
            vm_gj_after_ying.full_sentence_parts,
            vec![
                ("感觉".to_string(), false),
                ("影".to_string(), true),
                ("该".to_string(), false),
            ]
        );
        assert_eq!(vm_gj_after_ying.visible_candidates[0].text, "感觉影该");
        assert_eq!(vm_gj_after_ying.visible_candidates[1].text, "感觉");
        // 选定第 2 项『感觉』后，由于中间『影』已锁定，光标应直接跳到末尾未选的 |gai (Some(3))
        session.process_key(VirtualKey::Digit(2));
        let vm_gj_after_ganjue = session.build_view_model();
        assert_eq!(vm_gj_after_ganjue.active_syllable_index, Some(3));
        assert_eq!(
            vm_gj_after_ganjue.full_sentence_parts,
            vec![("感觉影".to_string(), true), ("该".to_string(), false)]
        );
        // 验证回退重选已锁定音节：按 ← 回到已锁定的 |ying 或 |jue 时，候选列表绝不能被清空，仍可重选！
        session.process_key(VirtualKey::Left); // |ying (已锁定为『影』)
        let vm_reselect_ying = session.build_view_model();
        assert_eq!(vm_reselect_ying.active_syllable_index, Some(2));
        assert!(
            !vm_reselect_ying.visible_candidates.is_empty()
                && vm_reselect_ying
                    .visible_candidates
                    .iter()
                    .any(|c| c.text == "应" || c.text == "影"),
            "回退到已锁定音节 |ying 时候选列表不应为空，实际: {:?}",
            vm_reselect_ying
                .visible_candidates
                .iter()
                .map(|c| &c.text)
                .collect::<Vec<_>>()
        );
        session.process_key(VirtualKey::CtrlRight); // 回到 |gai (Some(3))
        assert_eq!(session.build_view_model().active_syllable_index, Some(3));
        // 再按空格选定最后一个音节『该』，合成整句『感觉影该』上屏
        let commit_gj = session.process_key(VirtualKey::Space);
        assert_eq!(
            commit_gj,
            KeyAction::EatAndCommit {
                committed_text: "感觉影该".to_string(),
                ghost_text: None,
            }
        );
        // 关键防污染验证：因光标移到句中 |ying 改选单字而合成的『感觉影该』绝不能被误记为高优四字词，
        // 再次输入 "ganjueyinggai" 时，首选整句与第 1 候选必须依然是『感觉应该』！
        for ch in "ganjueyinggai".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_gj_retype = session.build_view_model();
        assert_eq!(vm_gj_retype.visible_candidates[0].text, "感觉应该");
        session.process_key(VirtualKey::Escape);
    }

    #[test]
    fn test_up_down_selection_user_memory_and_fuzzy_error_correction() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        let core = ImeCoreEngine::new(lexicon);
        let ai = DualModeAiOrchestrator::default();
        let mut session = KeySession::new(core, ai);

        // 1. 测试上下方向键 (Up / Down) 选词与输入记忆自动晋升首选词：
        // 输入 "wode"，默认第 1 候选为 "我的"
        for ch in "wode".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm0 = session.build_view_model();
        assert_eq!(vm0.selected_candidate_index, 0);
        assert_eq!(vm0.visible_candidates[0].text, "我的");
        let third_cand = vm0.visible_candidates[2].text.clone();
        assert_ne!(third_cand, "我的");

        // 按两下 Down 键高亮第 3 候选，再按一下 Up 键再按一下 Down 键验证上下切换
        session.process_key(VirtualKey::Down);
        assert_eq!(session.build_view_model().selected_candidate_index, 1);
        session.process_key(VirtualKey::Down);
        assert_eq!(session.build_view_model().selected_candidate_index, 2);
        session.process_key(VirtualKey::Up);
        assert_eq!(session.build_view_model().selected_candidate_index, 1);
        session.process_key(VirtualKey::Down);
        assert_eq!(session.build_view_model().selected_candidate_index, 2);

        // 按空格键提交当前高亮的第 3 候选
        let act = session.process_key(VirtualKey::Space);
        assert_eq!(
            act,
            KeyAction::EatAndCommit {
                committed_text: third_cand.clone(),
                ghost_text: None,
            }
        );

        // 再次输入 "wode"，刚才主动选择的词应凭借输入记忆直接晋升为第 1 首选词！
        for ch in "wode".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_after_mem = session.build_view_model();
        assert_eq!(vm_after_mem.visible_candidates[0].text, third_cand);
        assert!(vm_after_mem.visible_candidates[0].tag.contains("记忆"));
        session.process_key(VirtualKey::Escape);

        // 2. 测试平翘舌、前后鼻音与拼写手误纠错能力：
        // (a) 平翘舌 + 前后鼻音同时输错："zengde" -> 候选应包含 "真的"
        for ch in "zengde".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_zengde = session.build_view_model();
        assert!(
            vm_zengde.visible_candidates.iter().any(|c| c.text == "真的"),
            "Expected '真的' in candidates for 'zengde', got: {:?}",
            vm_zengde
                .visible_candidates
                .iter()
                .map(|c| (&c.text, &c.tag))
                .collect::<Vec<_>>()
        );
        session.process_key(VirtualKey::Escape);

        // (b) 平翘舌 + 前后鼻音同时输错："cenxu" -> 候选应包含 "程序"
        for ch in "cenxu".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_cenxu = session.build_view_model();
        assert!(
            vm_cenxu.visible_candidates.iter().any(|c| c.text == "程序"),
            "Expected '程序' in candidates for 'cenxu', got: {:?}",
            vm_cenxu
                .visible_candidates
                .iter()
                .map(|c| (&c.text, &c.tag))
                .collect::<Vec<_>>()
        );
        session.process_key(VirtualKey::Escape);

        // (c) 常见键盘顺序倒置手误："beijign" (ign -> ing) -> 候选应包含 "北京"
        for ch in "beijign".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_beijign = session.build_view_model();
        assert!(
            vm_beijign.visible_candidates.iter().any(|c| c.text == "北京"),
            "Expected '北京' in candidates for 'beijign', got: {:?}",
            vm_beijign
                .visible_candidates
                .iter()
                .map(|c| (&c.text, &c.tag))
                .collect::<Vec<_>>()
        );
        session.process_key(VirtualKey::Escape);

        // (d) 构词前缀「反」+「重力」与「这个词」会话级分步选词验证：
        for ch in "fanzhongli".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_fzl = session.build_view_model();
        assert_eq!(vm_fzl.visible_candidates[0].text, "反重力");
        // 用户主动选定首音节单字「反」，光标自动推进到 `|zhong li`，此时第 1 候选必须为「重力」且包含「中立」
        let fan_idx = vm_fzl
            .visible_candidates
            .iter()
            .position(|c| c.text == "反")
            .expect("Expected '反' on first page of fanzhongli");
        session.process_key(VirtualKey::Digit((fan_idx + 1) as u8));
        let vm_after_fan = session.build_view_model();
        assert_eq!(vm_after_fan.visible_candidates[0].text, "重力");
        assert!(vm_after_fan.visible_candidates.iter().any(|c| c.text == "中立"));
        session.process_key(VirtualKey::Escape);

        for ch in "zhegeci".chars() {
            session.process_key(VirtualKey::Char(ch));
        }
        let vm_zgc = session.build_view_model();
        assert_eq!(vm_zgc.visible_candidates[0].text, "这个词");
        session.process_key(VirtualKey::Escape);
    }

    // ------------------------------------------------------------------
    // 触屏键盘入口的测试
    // ------------------------------------------------------------------

    fn new_test_session() -> KeySession {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let lexicon = CompiledLexicon::load_or_build_default(root).unwrap();
        KeySession::new(ImeCoreEngine::new(lexicon), DualModeAiOrchestrator::default())
    }

    fn type_str(session: &mut KeySession, s: &str) {
        for ch in s.chars() {
            session.process_key(VirtualKey::Char(ch));
        }
    }

    /// 在点的音节上选第一个叫 `text` 的候选
    fn pick_on_syllable(session: &mut KeySession, syllable: usize, text: &str) -> KeyAction {
        session.click_pinyin_syllable(syllable, None);
        assert_eq!(session.active_syllable_index(), Some(syllable));
        let idx = session
            .current_candidates
            .iter()
            .position(|c| c.text == text)
            .unwrap_or_else(|| panic!("音节 {syllable} 的候选里没有「{text}」"));
        session.select_candidate(idx)
    }

    #[test]
    fn test_touch_pick_on_a_tapped_syllable_returns_to_the_sentence() {
        let mut session = new_test_session();
        session.touch_syllable_editing = true;
        type_str(&mut session, "tademingzijiaoyangjiaye");
        let n = session.input_buffer.len();

        // 点 jiao 选「叫」：回到整句，不跳到句首的 ta；首选是保留了「叫」的整句
        let action = pick_on_syllable(&mut session, 4, "叫");
        assert!(matches!(action, KeyAction::EatAndUpdateUi(_)));
        let vm = session.build_view_model();
        assert_eq!(vm.active_syllable_index, None, "选完字不自动选中别的音节");
        assert!(vm.composed_full_sentence.contains('叫'), "{}", vm.composed_full_sentence);
        assert_eq!(session.current_candidates[0].consumed_len, n, "首选是整句");
        assert!(session.current_candidates[0].text.contains('叫'));
        assert!(vm.full_sentence_parts.iter().any(|(text, pinned)| *pinned && text.contains('叫')));

        // 再点 jiao、按退格：撤销那里的选字，光标留在 jiao 上，拼音不变
        session.click_pinyin_syllable(4, None);
        let action = session.process_key(VirtualKey::Backspace);
        assert!(matches!(action, KeyAction::EatAndUpdateUi(_)));
        assert!(session.pinned_segments.is_empty(), "{:?}", session.pinned_segments);
        assert_eq!(session.input_buffer, "tademingzijiaoyangjiaye");
        assert_eq!(session.active_syllable_index(), Some(4));

        // 点着没选定的音节按退格：照旧删光标左边的字母
        session.click_pinyin_syllable(5, None);
        session.process_key(VirtualKey::Backspace);
        assert_eq!(session.input_buffer, "tademingzijiayangjiaye");
    }

    #[test]
    fn test_touch_pick_reaching_the_end_commits_when_everything_is_chosen() {
        let mut session = new_test_session();
        session.touch_syllable_editing = true;
        type_str(&mut session, "nihao");
        // 先选前面的音节：回到整句，不上屏
        let first = pick_on_syllable(&mut session, 0, "你");
        assert!(matches!(first, KeyAction::EatAndUpdateUi(_)));
        assert_eq!(session.active_syllable_index(), None);
        // 再选最后一个音节：选到句尾、整句都选定了，直接上屏
        let last = pick_on_syllable(&mut session, 1, "好");
        assert!(
            matches!(&last, KeyAction::EatAndCommit { committed_text, .. } if committed_text == "你好"),
            "{last:?}"
        );
    }

    #[test]
    fn test_keyboard_pick_on_a_syllable_still_moves_to_the_first_unchosen_one() {
        // 不开触屏模式（Windows 版）：在句中音节上选完字，光标照旧跳到前面第一个没选的音节
        let mut session = new_test_session();
        type_str(&mut session, "tademingzijiaoyangjiaye");
        pick_on_syllable(&mut session, 4, "叫");
        assert_eq!(session.active_syllable_index(), Some(0));
    }

    #[test]
    fn test_emoji_candidates_follow_words_and_commit_without_learning() {
        // 默认关闭：Windows 版的候选不变
        let mut session = new_test_session();
        type_str(&mut session, "mao");
        assert!(session.current_candidates.iter().all(|c| c.tag != emoji::EMOJI_TAG));
        session.cancel_composition();

        session.emoji_candidates_enabled = true;
        type_str(&mut session, "mao");
        let texts: Vec<String> = session.current_candidates.iter().map(|c| c.text.clone()).collect();
        let cat = texts.iter().position(|t| t == "猫").expect("候选里应有「猫」");
        assert_eq!(session.current_candidates[cat + 1].text, "🐈", "{texts:?}");
        assert_eq!(session.current_candidates[cat + 1].tag, emoji::EMOJI_TAG);
        assert_ne!(session.current_candidates[0].tag, emoji::EMOJI_TAG, "首选永远是词");

        // 选中表情：只上屏表情，个人词库里没有它
        let action = session.select_candidate(cat + 1);
        match action {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, "🐈"),
            other => panic!("应上屏表情：{other:?}"),
        }
        assert!(!session.is_composing());
        assert_eq!(session.core.user_dict.user_boost("mao", "🐈"), 0.0);
        assert_eq!(session.core.user_dict.explicit_choice_count("mao", "🐈"), 0);

        // 空格照常上屏首选的词
        type_str(&mut session, "mao");
        let top = session.current_candidates[0].text.clone();
        match session.process_key(VirtualKey::Space) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, top),
            other => panic!("空格应上屏首选：{other:?}"),
        }

        // 句中逐段选词时（有固定的词块）不插表情
        type_str(&mut session, "maoyi");
        session.pinned_segments.insert(0, (3, "贸".to_string()));
        session.recompute_candidates();
        assert!(session.current_candidates.iter().all(|c| c.tag != emoji::EMOJI_TAG));
    }

    #[test]
    fn test_is_composing_and_cancel_composition() {
        let mut session = new_test_session();
        assert!(!session.is_composing());
        type_str(&mut session, "nihao");
        assert!(session.is_composing());
        assert!(!session.current_candidates.is_empty());
        session.process_key(VirtualKey::Left);
        session.pending_ghost_text = Some("x".to_string());

        session.cancel_composition();
        assert!(!session.is_composing());
        assert!(session.input_buffer.is_empty());
        assert!(session.current_candidates.is_empty());
        assert_eq!(session.cursor_pos, 0);
        assert!(!session.cursor_syllable_mode);
        assert!(session.pinned_segments.is_empty());
        assert!(session.pending_ghost_text.is_none());
        assert!(!session.build_view_model().visible);
        // 取消后可以立即开始新的组合，且不上屏任何东西
        assert!(session.committed_history.is_empty());
        type_str(&mut session, "wo");
        assert!(session.is_composing());
        // 无组合时取消是幂等的
        session.cancel_composition();
        session.cancel_composition();
        assert!(!session.is_composing());
    }

    #[test]
    fn test_set_scene_directly() {
        let mut session = new_test_session();
        assert_eq!(session.active_scene, AppScene::General);
        session.committed_history = "上文".to_string();
        session.set_scene(AppScene::Chat);
        assert_eq!(session.active_scene, AppScene::Chat);
        // 与 on_foreground_process_changed 不同：不清空上文
        assert_eq!(session.committed_history, "上文");

        // 组合中设置场景会立即重算并反映在视图上
        type_str(&mut session, "nihao");
        session.set_scene(AppScene::CodeEditor);
        assert_eq!(session.active_scene, AppScene::CodeEditor);
        let vm = session.build_view_model();
        assert_eq!(vm.active_scene_label, AppScene::CodeEditor.label());
        assert!(!vm.visible_candidates.is_empty());
    }

    #[test]
    fn test_set_page_size_repaginates() {
        let mut session = new_test_session();
        type_str(&mut session, "shi");
        assert_eq!(session.build_view_model().page_size, 5);
        assert!(session.current_candidates.len() > 12, "shi 应有足够多的候选");

        session.set_page_size(9);
        let vm = session.build_view_model();
        assert_eq!(vm.page_size, 9);
        assert_eq!(vm.visible_candidates.len(), 9);
        assert_eq!(vm.visible_candidates[0].text, session.current_candidates[0].text);

        // 翻到第 2 页后改页大小：高亮的候选保持不变
        session.process_key(VirtualKey::PageDown);
        session.process_key(VirtualKey::Down);
        let before = {
            let vm = session.build_view_model();
            vm.visible_candidates[vm.selected_candidate_index].text.clone()
        };
        session.set_page_size(4);
        let vm = session.build_view_model();
        assert_eq!(vm.page_size, 4);
        assert_eq!(
            vm.visible_candidates[vm.selected_candidate_index].text,
            before
        );

        // 0 被夹到 1，不会除零
        session.set_page_size(0);
        assert_eq!(session.page_size, 1);
        assert_eq!(session.build_view_model().visible_candidates.len(), 1);
        session.process_key(VirtualKey::PageDown);
        session.process_key(VirtualKey::Down);
    }

    #[test]
    fn test_select_visible_candidate_and_global_index() {
        let mut session = new_test_session();

        // 无组合：放行
        assert_eq!(session.select_visible_candidate(0), KeyAction::PassThrough);
        assert_eq!(session.select_candidate(0), KeyAction::PassThrough);

        // 当前页第 0 项与空格首选等价
        type_str(&mut session, "ni");
        let first = session.current_candidates[0].text.clone();
        match session.select_visible_candidate(0) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, first),
            other => panic!("expected commit, got {other:?}"),
        }
        assert!(!session.is_composing());

        // 页内下标越界：只刷新视图，组合保持
        type_str(&mut session, "ni");
        let n_vis = session.build_view_model().visible_candidates.len();
        assert!(matches!(
            session.select_visible_candidate(n_vis),
            KeyAction::EatAndUpdateUi(_)
        ));
        assert!(session.is_composing());
        // 全局下标越界同样如此
        let total = session.current_candidates.len();
        assert!(matches!(
            session.select_candidate(total),
            KeyAction::EatAndUpdateUi(_)
        ));
        assert!(session.is_composing());

        // 翻到第 2 页，页内下标 1 对应全局下标 page_size + 1，与数字键 2 一致
        session.process_key(VirtualKey::PageDown);
        let expect = session.current_candidates[session.page_size + 1].text.clone();
        let mut by_digit = new_test_session();
        type_str(&mut by_digit, "ni");
        by_digit.process_key(VirtualKey::PageDown);
        let digit_act = by_digit.process_key(VirtualKey::Digit(2));
        let visible_act = session.select_visible_candidate(1);
        assert_eq!(visible_act, digit_act);
        match visible_act {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, expect),
            other => panic!("expected commit, got {other:?}"),
        }

        // 全局下标直接选第 7 项（跨页），与按页内下标选结果一致
        let mut g = new_test_session();
        type_str(&mut g, "ni");
        let expect7 = g.current_candidates[7].text.clone();
        match g.select_candidate(7) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, expect7),
            other => panic!("expected commit, got {other:?}"),
        }

        // 页大小大于 9 时，数字键够不到的页内候选可以用下标选到
        let mut big = new_test_session();
        type_str(&mut big, "shi");
        big.set_page_size(12);
        let expect10 = big.current_candidates[10].text.clone();
        match big.select_visible_candidate(10) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, expect10),
            other => panic!("expected commit, got {other:?}"),
        }
    }

    #[test]
    fn test_select_candidate_partial_pins_segment_like_digit_key() {
        // 选定局部词块（句中某段）时与数字键一样只锁定该词块，组合继续
        let mut a = new_test_session();
        let mut b = new_test_session();
        type_str(&mut a, "wolaiceshiyixia");
        type_str(&mut b, "wolaiceshiyixia");
        let act_a = a.process_key(VirtualKey::Digit(2));
        let act_b = b.select_visible_candidate(1);
        assert_eq!(act_a, act_b);
        assert_eq!(a.pinned_segments, b.pinned_segments);
        assert_eq!(a.input_buffer, b.input_buffer);
    }

    #[test]
    fn test_host_context_replaces_committed_history() {
        // 1. 路由：上屏历史为 A、宿主上文为 B 时，行为与「上屏历史为 B」完全一致
        let mut with_history = new_test_session();
        with_history.committed_history = "我在用".to_string();
        let mut with_host = new_test_session();
        with_host.committed_history = "完全无关的历史".to_string();
        with_host.set_context_before_cursor("我在用");
        assert_eq!(with_host.context_before_cursor(), "我在用");
        for inp in ["wozaiyongrust", "gongzuo", "xianzaizaiyanjiuyixia"] {
            type_str(&mut with_history, inp);
            type_str(&mut with_host, inp);
            assert_eq!(
                with_history.current_candidates, with_host.current_candidates,
                "input {inp}"
            );
            with_history.cancel_composition();
            with_host.cancel_composition();
        }

        // 2. Ghost Text 用的也是宿主上文：上屏历史里是「傅里」，宿主说前文是别的 -> 没有联想；
        //    宿主说前文是「傅里」 -> 选「叶」后联想出「级数展开」
        let commit_ye = |s: &mut KeySession| -> KeyAction {
            type_str(s, "ye");
            let pos = s
                .current_candidates
                .iter()
                .position(|c| c.text == "叶")
                .expect("ye 候选应包含 叶");
            s.select_candidate(pos)
        };
        let mut other = new_test_session();
        other.committed_history = "傅里".to_string();
        other.set_context_before_cursor("今天天气不错");
        match commit_ye(&mut other) {
            KeyAction::EatAndCommit { ghost_text, .. } => assert!(ghost_text.is_none()),
            act => panic!("expected commit, got {act:?}"),
        }
        let mut fourier = new_test_session();
        fourier.set_context_before_cursor("傅里");
        match commit_ye(&mut fourier) {
            KeyAction::EatAndCommit { ghost_text, .. } => {
                assert_eq!(ghost_text.as_deref(), Some("级数展开"))
            }
            act => panic!("expected commit, got {act:?}"),
        }
        // 上屏的文字同步追加到宿主上文，直到宿主再次刷新
        assert_eq!(fourier.context_before_cursor(), "傅里叶");
        // 会话自己的历史也照常记录（Windows 路径不受影响）
        assert_eq!(fourier.committed_history, "叶");

        // 3. 空串也算「提供了」：不回退到上屏历史
        let mut empty = new_test_session();
        empty.committed_history = "傅里".to_string();
        empty.set_context_before_cursor("");
        assert_eq!(empty.context_before_cursor(), "");
        match commit_ye(&mut empty) {
            KeyAction::EatAndCommit { ghost_text, .. } => assert!(ghost_text.is_none()),
            act => panic!("expected commit, got {act:?}"),
        }

        // 4. 过长的上文只保留末尾 64 个字符
        let long: String = "字".repeat(100) + "尾";
        let mut s = new_test_session();
        s.set_context_before_cursor(&long);
        assert_eq!(s.context_before_cursor().chars().count(), 64);
        assert!(s.context_before_cursor().ends_with('尾'));

        // 5. 清除后回到上屏历史；焦点应用切换同样清除宿主上文
        s.committed_history = "历史".to_string();
        s.clear_context_before_cursor();
        assert_eq!(s.context_before_cursor(), "历史");
        s.set_context_before_cursor("宿主");
        s.on_foreground_process_changed("Code.exe");
        assert_eq!(s.context_before_cursor(), "");
    }

    #[test]
    fn test_context_memory_remembers_an_explicit_override_for_that_context_only() {
        let correct_in = |s: &mut KeySession, context: &str, pinyin: &str, want: &str| -> usize {
            s.set_context_before_cursor(context);
            type_str(s, pinyin);
            let pos = s
                .current_candidates
                .iter()
                .position(|c| c.text == want)
                .unwrap_or_else(|| panic!("{pinyin} 的候选应包含 {want}"));
            match s.select_candidate(pos) {
                KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, want),
                act => panic!("expected commit, got {act:?}"),
            }
            pos
        };
        let top_in = |s: &mut KeySession, context: &str, pinyin: &str| -> Candidate {
            s.set_context_before_cursor(context);
            type_str(s, pinyin);
            let top = s.current_candidates[0].clone();
            s.cancel_composition();
            top
        };

        // 默认关闭（Windows 版行为不变）：改选只形成上文无关的个人记忆
        let mut off = new_test_session();
        assert!(!off.context_memory_enabled);
        let pos = correct_in(&mut off, "我对天发誓这是我的", "shiyan", "誓言");
        assert!(pos > 0, "「誓言」本来不是首选");
        assert!(off.core.user_dict.context_choices.is_empty());
        assert!(!top_in(&mut off, "我对天发誓这是我的", "shiyan").tag.contains("上文"));

        // 打开之后：同样的上文末尾直接把它排到首位，标签是 [记忆·上文]
        let mut on = new_test_session();
        on.context_memory_enabled = true;
        correct_in(&mut on, "我对天发誓这是我的", "shiyan", "誓言");
        assert_eq!(
            on.core.user_dict.context_choice("我对天发誓这是我的", "shiyan"),
            Some("誓言")
        );
        let same = top_in(&mut on, "我对天发誓这是我的", "shiyan");
        assert_eq!(same.text, "誓言");
        assert_eq!(same.tag, "[记忆·上文]");
        // 末尾 3 个字符相同的另一句话也算
        assert_eq!(top_in(&mut on, "这是我的", "shiyan").tag, "[记忆·上文]");

        // 在另一段上文里把同一串拼音改选成别的词：两段上文各记各的
        correct_in(&mut on, "今天化学课我们做了一个", "shiyan", "实验");
        let chem = top_in(&mut on, "今天化学课我们做了一个", "shiyan");
        assert_eq!(chem.text, "实验");
        assert_eq!(chem.tag, "[记忆·上文]");
        let oath = top_in(&mut on, "我对天发誓这是我的", "shiyan");
        assert_eq!(oath.text, "誓言");
        assert_eq!(oath.tag, "[记忆·上文]");
        // 没见过的上文不受这两条记录影响（首选由上文无关的个人记忆和词频决定）
        let elsewhere = top_in(&mut on, "这个结论还需要通过", "shiyan");
        assert_ne!(elsewhere.tag, "[记忆·上文]");

        // 用空格接受首选不会新增记录；首选就是记住的词时也不会
        let before = on.core.user_dict.context_choices.len();
        on.set_context_before_cursor("这个结论还需要通过");
        type_str(&mut on, "shiyan");
        assert!(matches!(on.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        on.set_context_before_cursor("我对天发誓这是我的");
        type_str(&mut on, "shiyan");
        assert!(matches!(on.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        assert_eq!(on.core.user_dict.context_choices.len(), before);

        // 上文不足 2 个字符：不记
        let mut short = new_test_session();
        short.context_memory_enabled = true;
        correct_in(&mut short, "我", "shiyan", "誓言");
        assert!(short.core.user_dict.context_choices.is_empty());
    }

    #[test]
    fn test_user_dict_is_written_once_when_the_composition_ends() {
        use ime_core::UserDict;
        use std::time::Duration;
        let dir = std::env::temp_dir().join(format!("ime_session_udict_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("user_dict.json");
        let wait = Duration::from_secs(5);

        let mut session = new_test_session();
        session.core.enable_persistence(path.clone());

        // 句中选定一个词：这一步就学了东西，但输入还没结束，不取快照、不写盘
        type_str(&mut session, "woxiangqugongyuan");
        session.click_pinyin_syllable(1, None);
        assert!(matches!(session.select_candidate(1), KeyAction::EatAndUpdateUi(_)));
        assert!(!session.core.user_dict.records.is_empty() || !session.core.user_dict.user_bigrams.is_empty());
        session.core.wait_for_user_dict_writes(wait);
        assert!(!path.exists(), "nothing is written in the middle of a composition");

        // 整句上屏（又学了几条搭配）：输入结束，写一次，内容是最新的
        assert!(matches!(session.commit_full_sentence(), KeyAction::EatAndCommit { .. }));
        session.core.wait_for_user_dict_writes(wait);
        let saved = UserDict::load_from_file(&path).expect("written after the commit");
        assert_eq!(saved.records.len(), session.core.user_dict.records.len());
        assert_eq!(saved.user_bigrams.len(), session.core.user_dict.user_bigrams.len());
        assert!(!saved.user_bigrams.is_empty());

        // 学了东西之后取消输入：学到的也要写出去
        type_str(&mut session, "nihaoshijie");
        session.click_pinyin_syllable(2, None);
        assert!(matches!(session.select_candidate(1), KeyAction::EatAndUpdateUi(_)));
        let learned = (session.core.user_dict.records.len(), session.core.user_dict.user_bigrams.len());
        assert_ne!(learned, (saved.records.len(), saved.user_bigrams.len()));
        session.cancel_composition();
        session.core.wait_for_user_dict_writes(wait);
        let saved = UserDict::load_from_file(&path).unwrap();
        assert_eq!((saved.records.len(), saved.user_bigrams.len()), learned);

        // 没学到东西的上屏（回车原样上屏）不写盘
        let before = ime_core::FileStamp::probe(&path).unwrap();
        type_str(&mut session, "abc");
        assert!(matches!(session.process_key(VirtualKey::Enter), KeyAction::EatAndCommit { .. }));
        session.core.wait_for_user_dict_writes(wait);
        assert_eq!(ime_core::FileStamp::probe(&path).unwrap(), before);

        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_explicitly_chosen_words_get_a_stronger_prior() {
        let mut s = new_test_session();
        // 空格接受首选：普通强度
        type_str(&mut s, "shiyan");
        let top = s.current_candidates[0].text.clone();
        assert!(matches!(s.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        let plain = user_memory(&s.core.user_dict, "shiyan", &top, &[]);
        assert!(plain.boost > 0.0);
        assert!(!plain.explicit);
        assert_eq!(plain.boost, s.core.user_dict.user_boost("shiyan", &top));
        assert_eq!(plain.prior(), plain.boost);

        // 没要首选、点了第二个：这个词的先验乘上倍数（记忆强度本身不变）
        type_str(&mut s, "shiyan");
        let other = s.current_candidates.iter().position(|c| c.text != top && c.consumed_len == 6).unwrap();
        let other_text = s.current_candidates[other].text.clone();
        assert!(matches!(s.select_candidate(other), KeyAction::EatAndCommit { .. }));
        assert_eq!(s.core.user_dict.explicit_choice_count("shiyan", &other_text), 1);
        let chosen = user_memory(&s.core.user_dict, "shiyan", &other_text, &[]);
        assert!(chosen.explicit);
        assert_eq!(chosen.boost, s.core.user_dict.user_boost("shiyan", &other_text));
        assert_eq!(chosen.prior(), chosen.boost * ime_ai::EXPLICIT_CHOICE_PRIOR_FACTOR);
        // 从没用过的词：0
        assert_eq!(user_memory(&s.core.user_dict, "shiyan", "食盐", &[]), UserMemory::default());
    }

    /// 读出某条记录的（记忆强度, 主动改选次数），没有记录时为 None
    fn record_of(s: &KeySession, code: &str, text: &str) -> Option<(f32, u32)> {
        s.core
            .user_dict
            .records
            .get(&format!("{code}\t{text}"))
            .map(|r| (r.dynamic_score, r.explicit_count))
    }

    /// 在当前候选里点选文本为 `text`、覆盖到拼音第 `end` 个字节的那一个
    fn pick(s: &mut KeySession, text: &str, end: usize) -> KeyAction {
        let pos = s
            .current_candidates
            .iter()
            .position(|c| c.text == text && c.segment_spans.last().map_or(c.consumed_len, |sp| sp.end) == end)
            .unwrap_or_else(|| {
                let shown: Vec<&str> = s.current_candidates.iter().map(|c| c.text.as_str()).collect();
                panic!("候选里应有 {text}（到第 {end} 字节），实际 {shown:?}")
            });
        s.select_candidate(pos)
    }

    #[test]
    fn test_only_choices_that_differ_from_what_was_shown_count_as_explicit() {
        // 1. 改了前面的词，再按空格接受后面原来的词：只有改过的词和改过的整句算主动改选
        let mut s = new_test_session();
        type_str(&mut s, "shiyanshibai");
        assert_eq!(s.last_full_sentence, "实验失败");
        s.click_pinyin_syllable(0, None);
        assert!(matches!(pick(&mut s, "试验", 6), KeyAction::EatAndUpdateUi(_)));
        assert_eq!(s.sentence_before_pins, "实验失败", "第一次锁定时记下原来的整句");
        assert_eq!(s.current_candidates[0].text, "失败");
        match s.process_key(VirtualKey::Space) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, "试验失败"),
            act => panic!("expected commit, got {act:?}"),
        }
        assert_eq!(record_of(&s, "shiyan", "试验"), Some((6.5, 1)));
        assert_eq!(record_of(&s, "shibai", "失败"), Some((2.8, 0)), "失败 只是被接受，与空格上屏相同");
        assert_eq!(record_of(&s, "shiyanshibai", "试验失败"), Some((6.5, 1)), "整句与原来的不同");
        assert!(s.sentence_before_pins.is_empty(), "输入结束后清空");
        assert!(!user_memory(&s.core.user_dict, "shibai", "失败", &[]).explicit);

        // 2. 选了与首选整句相同的前缀「我来」再按空格：上屏的整句就是原来的首选，什么都不算主动改选
        let mut s = new_test_session();
        type_str(&mut s, "wolaiceshi");
        assert_eq!(s.last_full_sentence, "我来测试");
        assert!(matches!(pick(&mut s, "我来", 5), KeyAction::EatAndUpdateUi(_)));
        match s.process_key(VirtualKey::Space) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, "我来测试"),
            act => panic!("expected commit, got {act:?}"),
        }
        for (code, text) in [("wolai", "我来"), ("ceshi", "测试"), ("wolaiceshi", "我来测试")] {
            assert_eq!(record_of(&s, code, text), Some((2.8, 0)), "{code} {text}");
        }

        // 2b. 同上，但选了前缀之后点回整句（iOS 上再点一次当前音节），在整句候选里按空格：同样不算主动改选，
        // 学到的东西却与在整句候选里改选时一样多（整句、相邻词块的搭配、其中的词），只是都不记主动改选
        let mut s = new_test_session();
        type_str(&mut s, "wolaiceshi");
        pick(&mut s, "我来", 5);
        s.return_cursor_to_end();
        assert_eq!(s.active_syllable_index(), None);
        assert_eq!(s.current_candidates[0].text, "我来测试");
        match s.process_key(VirtualKey::Space) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, "我来测试"),
            act => panic!("expected commit, got {act:?}"),
        }
        assert_eq!(record_of(&s, "wolaiceshi", "我来测试"), Some((2.8, 0)), "整句照样记下");
        assert_eq!(record_of(&s, "ceshi", "测试"), Some((2.8, 0)), "其中的词照样记下");
        assert_eq!(s.core.user_dict.bigram_boost("我来", "测试"), 1.6, "相邻词块的搭配照样学");
        // 「我来」选前缀时记过一次，整句上屏时作为其中的词又记一次，两次都不算主动改选
        let (wolai, wolai_explicit) = record_of(&s, "wolai", "我来").unwrap();
        assert!((wolai - 5.6).abs() < 0.01 && wolai_explicit == 0, "{wolai} {wolai_explicit}");
        // 改了字再点回整句按空格：整句与原来的不同，算主动改选
        let mut s = new_test_session();
        type_str(&mut s, "tadeshiyan");
        s.click_pinyin_syllable(0, None);
        pick(&mut s, "她", 2);
        s.return_cursor_to_end();
        assert_eq!(s.current_candidates[0].text, "她的实验");
        assert!(matches!(s.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        assert_eq!(record_of(&s, "tadeshiyan", "她的实验"), Some((6.5, 1)));

        // 3. 把「他」改成「她」，再按空格接受余下的「的实验」：只有整句「她的实验」算主动改选
        let mut s = new_test_session();
        type_str(&mut s, "tadeshiyan");
        assert_eq!(s.last_full_sentence, "他的实验");
        s.click_pinyin_syllable(0, None);
        assert!(matches!(pick(&mut s, "她", 2), KeyAction::EatAndUpdateUi(_)));
        assert_eq!(s.current_candidates[0].text, "的实验");
        match s.process_key(VirtualKey::Space) {
            KeyAction::EatAndCommit { committed_text, .. } => assert_eq!(committed_text, "她的实验"),
            act => panic!("expected commit, got {act:?}"),
        }
        assert_eq!(record_of(&s, "deshiyan", "的实验"), Some((2.8, 0)));
        assert_eq!(record_of(&s, "tadeshiyan", "她的实验"), Some((6.5, 1)));

        // 4. 真正的改选（整句候选里点了非首选）照旧算主动改选
        let mut s = new_test_session();
        type_str(&mut s, "shiyan");
        let top = s.current_candidates[0].text.clone();
        let other = s.current_candidates.iter().position(|c| c.text != top && c.consumed_len == 6).unwrap();
        assert!(other > 0);
        let other_text = s.current_candidates[other].text.clone();
        assert!(matches!(s.select_candidate(other), KeyAction::EatAndCommit { .. }));
        assert_eq!(record_of(&s, "shiyan", &other_text), Some((6.5, 1)));
        // 句中把一段改成整句里没有的词，同样算
        let mut s = new_test_session();
        type_str(&mut s, "shiyanshibai");
        s.click_pinyin_syllable(0, None);
        assert!(matches!(pick(&mut s, "饰演", 6), KeyAction::EatAndUpdateUi(_)));
        assert_eq!(record_of(&s, "shiyan", "饰演"), Some((6.5, 1)));
    }

    /// 假打分器：候选池第 1 项比当前首选高 `lead` nat（`default()` 是 1 nat），其余很低；记下每次收到的候选池
    struct PreferSecond {
        lead: f64,
        pools: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl Default for PreferSecond {
        fn default() -> Self {
            Self::leading_by(1.0)
        }
    }

    impl PreferSecond {
        fn leading_by(lead: f64) -> Self {
            Self { lead, pools: Default::default() }
        }
    }

    impl ime_ai::CandidateScorer for PreferSecond {
        fn model_id(&self) -> String {
            "prefer-second".to_string()
        }

        fn score(&self, _context: &str, candidates: &[String]) -> Result<Vec<f64>, ime_ai::ScoreError> {
            self.pools.lock().unwrap().push(candidates.to_vec());
            let score = |i: usize| match i {
                0 => -4.0 - self.lead,
                1 => -4.0,
                _ => -20.0,
            };
            Ok((0..candidates.len()).map(score).collect())
        }

        fn cancel(&self) {}
    }

    /// 装上假打分器、设好上文后敲 `pinyin`，等打分结束再按结果重排
    fn type_with_scorer(s: &mut KeySession, scorer: &std::sync::Arc<PreferSecond>, pinyin: &str) {
        s.ai.set_candidate_scorer(Some(scorer.clone()));
        s.ai.cloud_client.config.enabled = true;
        s.set_context_before_cursor("我们今天下午在会议上讨论了");
        type_str(s, pinyin);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.ai.inprocess_result_pending() {
            assert!(std::time::Instant::now() < deadline, "打分一直没有结束");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        s.refresh_candidates();
    }

    #[test]
    fn test_jianpin_recall_carries_the_memory_of_the_full_pinyin_record() {
        // 用户用全拼主动改选出「文摘集」；之后敲简拼 wzj，解码器把它召回到首位，
        // 但那条记录存在全拼编码 wenzhaiji 下，用键入编码查不到
        let mut s = new_test_session();
        s.core.commit_candidate_ex("wenzhaiji", "文摘集", None, true);
        assert_eq!(s.core.user_dict.user_boost("wzj", "文摘集"), 0.0);
        assert_eq!(user_memory(&s.core.user_dict, "wzj", "文摘集", &[]), UserMemory::default());
        let recalled = s.core.user_dict.jianpin_matches("wzj");
        let memory = user_memory(&s.core.user_dict, "wzj", "文摘集", &recalled);
        assert!(memory.explicit);
        assert!((memory.boost - 6.5).abs() < 0.01, "{memory:?}");

        // 模型只比它高 1 nat：主动改选过的记忆（6.5 × 3 × 0.25 ≈ 4.9 nat）挡得住，首选不变
        let scorer = std::sync::Arc::new(PreferSecond::default());
        type_with_scorer(&mut s, &scorer, "wzj");
        let pools = scorer.pools.lock().unwrap().clone();
        let wzj_pool = pools
            .iter()
            .find(|p| p.first().map(String::as_str) == Some("文摘集"))
            .unwrap_or_else(|| panic!("记忆首选应当交给打分器、以记忆为先验裁决: {pools:?}"));
        assert!(wzj_pool.len() >= 2);
        assert_eq!(s.current_candidates[0].text, "文摘集", "被换成了 {}", wzj_pool[1]);
        s.cancel_composition();

        // 在 wzj 下空格上屏一次之后，键入编码下也有了一条（较弱、不算主动改选的）记录：取两条里先验较强的那条
        let mut s = new_test_session();
        s.core.commit_candidate_ex("wenzhaiji", "文摘集", None, true);
        type_str(&mut s, "wzj");
        assert_eq!(s.current_candidates[0].text, "文摘集");
        assert!(matches!(s.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        assert_eq!(record_of(&s, "wzj", "文摘集"), Some((2.8, 0)));
        let recalled = s.core.user_dict.jianpin_matches("wzj");
        let memory = user_memory(&s.core.user_dict, "wzj", "文摘集", &recalled);
        assert!(memory.explicit && memory.boost > 6.0, "{memory:?}");
        let scorer = std::sync::Arc::new(PreferSecond::default());
        type_with_scorer(&mut s, &scorer, "wzj");
        assert!(scorer.pools.lock().unwrap().iter().any(|p| p.first().map(String::as_str) == Some("文摘集")));
        assert_eq!(s.current_candidates[0].text, "文摘集");

        // 对照：没有任何记忆时，同样的打分器把首选换掉（证明上面确实是记忆在起作用）
        let mut fresh = new_test_session();
        let scorer = std::sync::Arc::new(PreferSecond::default());
        type_with_scorer(&mut fresh, &scorer, "wzj");
        let pools = scorer.pools.lock().unwrap().clone();
        let last = pools.last().expect("应当问过打分器");
        assert_eq!(fresh.current_candidates[0].text, last[1]);
    }

    #[test]
    fn test_space_commits_under_the_abbreviation_do_not_hide_the_taught_full_pinyin_record() {
        // 用户用全拼主动改选出「文摘集」（6.5，主动改选 1 次），之后在简拼 wzj 下空格上屏三次（8.4，没有主动改选）。
        // 两条记录推导出的简拼都是 wzj，简拼那条记忆强度更高，每个词只留一条时留下的是它
        let mut s = new_test_session();
        s.core.commit_candidate_ex("wenzhaiji", "文摘集", None, true);
        for _ in 0..3 {
            type_str(&mut s, "wzj");
            assert_eq!(s.current_candidates[0].text, "文摘集");
            assert!(matches!(s.process_key(VirtualKey::Space), KeyAction::EatAndCommit { .. }));
        }
        let (typed, typed_explicit) = record_of(&s, "wzj", "文摘集").unwrap();
        assert!((typed - 8.4).abs() < 0.01 && typed_explicit == 0, "{typed} {typed_explicit}");
        let top = s.core.user_dict.top_jianpin_records("wzj");
        let kept: Vec<&str> = top.iter().filter(|r| r.text == "文摘集").map(|r| r.code_key.as_str()).collect();
        assert_eq!(kept, ["wzj"]);

        // 个人记忆取先验最强的那条：全拼下主动改选的（6.5 × 3 = 19.5），不是简拼下空格上屏的（8.4）
        let recalled = s.core.user_dict.jianpin_matches("wzj");
        let memory = user_memory(&s.core.user_dict, "wzj", "文摘集", &recalled);
        assert!(memory.explicit && (memory.boost - 6.5).abs() < 0.01, "{memory:?}");
        assert!(memory.prior() > 19.0, "{memory:?}");

        // 模型比它高 3 nat：空格上屏的记忆（8.4 × 0.25 = 2.1 nat）挡不住，用户教过的记忆（19.5 × 0.25 ≈ 4.9 nat）挡得住
        let scorer = std::sync::Arc::new(PreferSecond::leading_by(3.0));
        type_with_scorer(&mut s, &scorer, "wzj");
        let pools = scorer.pools.lock().unwrap().clone();
        let wzj_pool = pools
            .iter()
            .find(|p| p.first().map(String::as_str) == Some("文摘集"))
            .unwrap_or_else(|| panic!("记忆首选应当交给打分器、以记忆为先验裁决: {pools:?}"));
        assert!(wzj_pool.len() >= 2);
        assert_eq!(s.current_candidates[0].text, "文摘集", "被换成了 {}", wzj_pool[1]);

        // 对照：只有简拼下空格上屏的记忆时，同样的打分器把首选换掉（证明上面挡住模型的是教过的那条）
        let mut habit = new_test_session();
        for _ in 0..3 {
            habit.core.commit_candidate_ex("wzj", "文摘集", None, false);
        }
        let scorer = std::sync::Arc::new(PreferSecond::leading_by(3.0));
        type_with_scorer(&mut habit, &scorer, "wzj");
        let pools = scorer.pools.lock().unwrap().clone();
        let wzj_pool = pools
            .iter()
            .find(|p| p.first().map(String::as_str) == Some("文摘集"))
            .unwrap_or_else(|| panic!("{pools:?}"));
        assert_eq!(habit.current_candidates[0].text, wzj_pool[1]);
    }

    #[test]
    fn test_sentence_before_pins_never_outlives_its_input() {
        // 锁定之后改拼音：记下的整句作废，含锁定词块的上屏照旧按主动改选算
        let mut s = new_test_session();
        type_str(&mut s, "wolaiceshi");
        pick(&mut s, "我来", 5);
        assert_eq!(s.sentence_before_pins, "我来测试");
        type_str(&mut s, "yixia");
        assert!(!s.pinned_segments.is_empty(), "接着输入的拼音追加在后面，「我来」仍然锁定");
        assert!(s.sentence_before_pins.is_empty());
        // 再锁定一段也不会补记（不知道改拼音之前用户看到的是什么）
        pick(&mut s, "测试", 10);
        assert!(s.sentence_before_pins.is_empty());
        s.cancel_composition();

        // 退格把锁定全部撤销：清空，下次锁定时重新记
        type_str(&mut s, "wolaiceshi");
        pick(&mut s, "我来", 5);
        assert_eq!(s.sentence_before_pins, "我来测试");
        s.process_key(VirtualKey::Backspace);
        assert!(s.pinned_segments.is_empty());
        assert!(s.sentence_before_pins.is_empty());
        assert_eq!(s.input_buffer, "wolaiceshi", "退格先撤销锁定，不删字母");

        // 在句中删一个字母：同样清空
        pick(&mut s, "我来", 5);
        assert_eq!(s.sentence_before_pins, "我来测试");
        s.process_key(VirtualKey::Right);
        s.process_key(VirtualKey::Delete);
        assert!(s.sentence_before_pins.is_empty());

        // 取消输入：清空
        s.cancel_composition();
        assert!(s.sentence_before_pins.is_empty());
        type_str(&mut s, "wolaiceshi");
        assert!(s.sentence_before_pins.is_empty(), "新的输入从空开始");
    }

    #[test]
    fn test_persistent_config_roundtrip_with_explicit_path() {
        let dir = std::env::temp_dir().join(format!("ime_session_cfg_{}", std::process::id()));
        let path = dir.join("nested").join("ime_config.json");
        let mut cfg = ImePersistentConfig::default();
        cfg.ai_config.enabled = !cfg.ai_config.enabled;
        cfg.save_to(&path).unwrap();
        let loaded = ImePersistentConfig::load_from(&path);
        assert_eq!(loaded.ai_config.enabled, cfg.ai_config.enabled);
        // 文件缺失或损坏时退回默认值
        assert_eq!(
            ImePersistentConfig::load_from(&dir.join("missing.json")).ai_config.enabled,
            ImePersistentConfig::default().ai_config.enabled
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
