//! `ime-tsf-win`: Windows TSF (Text Services Framework) 输入法外壳、按键状态机与 COM 导出接口
//!
//! 功能：
//! 1. **TSF 按键事件处理器 (`TsfKeySession`)**：
//!    处理 `A-Z`、数字键 `1-9` 选词、`Space` 首选上屏、`Enter` 原样字母上屏、`Tab` 采纳 AI Ghost Text 或切换英文候选、`Backspace` 退格、`Shift` 中英状态切换。
//! 2. **焦点应用程序场景感知 (`detect_scene_from_process`)**：
//!    根据当前活动窗口进程名（如 `Code.exe`, `WeChat.exe`, `winword.exe`）自动切换领域权重与盘古之白策略。
//! 3. **候选框 UI 视图模型 (`CandidateWindowViewModel`)**：
//!    提供无焦点悬浮窗（`WS_POPUP | WS_EX_TOPMOST | WS_EX_NOACTIVATE`）所需的分页候选、Preedit 拼音串与 AI 语境提示标签。
//! 4. **Windows COM DLL 标准导出函数**：
//!    提供 `DllGetClassObject`, `DllCanUnloadNow`, `DllRegisterServer`, `DllUnregisterServer` 导出入口。

pub mod tsf_com;
pub use tsf_com::{
    activate_tsf_profile_for_current_user, ensure_appcontainer_dir_acl,
    is_shell_appcontainer_host, is_tsf_com_registered, send_tsf_ipc, TsfIpcRequest, TsfIpcResponse,
    TSF_IPC_PORT,
};

use ime_ai::{DualModeAiOrchestrator, OpenAiCompatibleConfig};
use ime_core::{AppScene, Candidate, FuzzyPinyinConfig, ImeCoreEngine, SegmentSpan};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
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
        let path = Self::default_config_path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<Self>(&content) {
                return cfg;
            }
        }
        Self::default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::default_config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

/// 输入法唯一 CLSID: `{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}`
pub const IME_CLSID_STR: &str = "{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}";
/// 输入法 Profile GUID: `{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}`
pub const IME_PROFILE_GUID_STR: &str = "{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}";

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

/// TSF 文本输入会话状态机（遵循主流输入法标准逻辑：拼音光标移动 + 光标处直接增删改拼音 + 光标音节联动选词）
pub struct TsfKeySession {
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
    pub current_candidates: Vec<Candidate>,
    pub current_page: usize,
    pub page_size: usize,
    /// 当前页面中由上下方向键选中的高亮候选词下标 (`0..page_size`)
    pub selected_candidate_index: usize,
    /// 当前是否通过按上方向键 `↑` 将焦点移到了「整句」行（为 `true` 时按空格直接整句上屏，按 `↓` 回到候选词列表）
    pub full_sentence_selected: bool,
    pub committed_history: String,
    pub pending_ghost_text: Option<String>,
    pub active_scene: AppScene,
    pub last_ai_hint: String,
    /// 用户在句中选词锁定的片段：`start_byte -> (end_byte, replacement_text)`
    pub pinned_segments: HashMap<usize, (usize, String)>,
    /// 当前首选整句合成文本
    pub last_full_sentence: String,
    /// 当前首选整句的词级片段列表
    pub last_full_segments: Vec<SegmentSpan>,
    /// 当前首选整句的逐音节区间列表
    pub last_syllable_spans: Vec<SegmentSpan>,
    /// 是否启用 AI Ghost Text 续写（按 Tab 采纳）。全局版默认关闭，避免在真实应用中劫持 Tab 键
    pub ghost_text_enabled: bool,
}

impl TsfKeySession {
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
            current_candidates: Vec::new(),
            current_page: 0,
            page_size: 5,
            selected_candidate_index: 0,
            full_sentence_selected: false,
            committed_history: String::new(),
            pending_ghost_text: None,
            active_scene: AppScene::General,
            last_ai_hint: String::new(),
            pinned_segments: HashMap::new(),
            last_full_sentence: String::new(),
            last_full_segments: Vec::new(),
            last_syllable_spans: Vec::new(),
            ghost_text_enabled: true,
        }
    }

    /// 当焦点窗口切换时更新当前进程场景（例如在 IDE 中自动调整英文权重与空格策略）
    /// 同时清空上文语境，避免把微信里的聊天内容带进 VS Code 的选词判断
    pub fn on_foreground_process_changed(&mut self, process_name: &str) {
        self.active_scene = detect_scene_from_process(process_name);
        self.committed_history.clear();
        self.pending_ghost_text = None;
    }

    fn predict_ghost(&self) -> Option<String> {
        if !self.ghost_text_enabled {
            return None;
        }
        self.ai
            .local_engine
            .predict_next_ghost_text(&self.committed_history, self.active_scene)
    }

    /// 计算当前光标 `cursor_pos` 对应的活跃音节下标
    /// 规则：光标位于某音节「前端或内部」时归属该音节（`|shi`、`s|hi`、`sh|i` 均高亮 `shi`），
    /// 与左方向键「按音节跳到音节最前端」配合：按 ← 跳到 `|shi` 即显示 `shi` 起始的候选词；
    /// 光标处于整串末尾（音节模式下）时归属最后一个音节。
    pub fn active_syllable_index(&self) -> Option<usize> {
        if self.input_buffer.is_empty() || self.last_syllable_spans.is_empty() {
            return None;
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
        self.input_buffer.clear();
        self.cursor_pos = 0;
        self.cursor_syllable_mode = false;
        self.cursor_auto_advanced = false;
        self.last_op_was_pinyin_edit = false;
        self.full_sentence_selected = false;
        self.current_candidates.clear();
        self.pinned_segments.clear();
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
                let pos = self.cursor_pos.min(self.input_buffer.len());
                // 若当前光标左侧紧挨着一个已手动选定锁定的分词块（如选定「我来」后光标位于 `|ce`），
                // 按退格键首先解除该词块的锁定并把光标退回该词块起点（若已无任何锁定词则回到句尾）
                if let Some((&pin_start, _)) = self
                    .pinned_segments
                    .iter()
                    .find(|(_, &(e, _))| e == pos)
                {
                    self.pinned_segments.remove(&pin_start);
                    if self.pinned_segments.is_empty() {
                        self.cursor_pos = self.input_buffer.len();
                        self.cursor_syllable_mode = false;
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
                self.last_op_was_pinyin_edit = false;
                if self.full_sentence_selected {
                    // 已在「整句」行时保持不动
                } else if self.selected_candidate_index > 0 {
                    self.selected_candidate_index -= 1;
                } else if self.current_page > 0 {
                    self.current_page -= 1;
                    let cnt = self.visible_candidate_count();
                    self.selected_candidate_index = cnt.saturating_sub(1);
                } else if !self.last_full_sentence.is_empty() {
                    // 当处于第 1 页第 1 个候选词（如句中改错后默认聚焦 `1 叫`）时，
                    // 按上方向键 `↑` 向上跳选到正上方的「整句」行（如 `整句 右下角`），按空格即可整句上屏！
                    self.full_sentence_selected = true;
                }
                KeyAction::EatAndUpdateUi(self.build_view_model())
            }
            VirtualKey::Down => {
                if self.input_buffer.is_empty() || self.current_candidates.is_empty() {
                    return KeyAction::PassThrough;
                }
                self.last_op_was_pinyin_edit = false;
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
                self.cursor_auto_advanced = false;
                self.last_op_was_pinyin_edit = false;
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
        let l1 = self.core.decode_with_pins(
            &self.input_buffer,
            self.active_scene,
            &self.committed_history,
            &self.pinned_segments,
        );
        let l2 = self.ai.rerank(
            &self.input_buffer,
            &self.committed_history,
            self.active_scene,
            l1.candidates,
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
                let mut local_cands = self.core.query_segment_alternatives(
                    &self.input_buffer,
                    syl_span.start,
                    syl_span.end,
                );
                let n = self.input_buffer.len();
                // 当用户已选定至少一个词块（`!self.pinned_segments.is_empty()`）且光标位于非句首音节、剩余还有多个音节时，
                // 将从当前音节直到句尾的「余下整句」（如选定「我来」后的「测试一下」）排在第 1 位，方便直接按空格完成整句上屏，
                // 其后依次为当前音节起始的多字词组（如「测试」「侧视」）与单字（如「测」「策」）。
                if syl_span.start > 0
                    && syl_span.end < n
                    && !self.pinned_segments.is_empty()
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
                            if !sub_text.is_empty() && !local_cands.iter().any(|c| c.text == sub_text) {
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
        self.ai.invalidate_cache_for_pinyin(&full_code);
        // 记忆「用户改过的整句」或中短词句，并同步记录句内双字及以上常用词频
        if has_pins || full_text.chars().count() <= 12 {
            self.core
                .commit_candidate_ex(&full_code, &full_text, None, has_pins);
        }
        let full_segments = self.last_full_segments.clone();
        for seg in &full_segments {
            if seg.text.chars().count() >= 2 && seg.start < seg.end && seg.end <= full_code.len() {
                let seg_code = &full_code[seg.start..seg.end];
                if seg_code != full_code {
                    self.core
                        .commit_candidate_ex(seg_code, &seg.text, None, false);
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
        // - 若锁定后已到达句尾（`next_pos >= n`），立即合成完整句子一并上屏！
        // - 若句尾仍有未选音节（`next_pos < n`），则将光标与候选列表自动推进到下一个未锁定音节（如选定「我来」后从 `ce shi yi xia` 开始），
        //   且已选定部分绝不再重复出现在候选中！
        if self.active_syllable_index().is_some() || span_start > 0 || span_end < n {
            if !chosen.segment_spans.is_empty() {
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
                } else if chosen.segment_spans.len() == 1 || chosen.text.chars().count() <= 6 {
                    self.core.commit_candidate_ex(
                        &slice_code,
                        &chosen.text,
                        prev_word.as_deref(),
                        true,
                    );
                }
                self.ai.invalidate_cache_for_pinyin(&slice_code);
                self.ai.invalidate_cache_for_pinyin(&self.input_buffer);
            }

            let next_pos = self.next_unpinned_offset_from(span_end);
            if next_pos >= n {
                self.recompute_candidates();
                return self.commit_full_sentence();
            }

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
        let is_explicit = global_idx > 0 || !self.pinned_segments.is_empty();

        self.ai.invalidate_cache_for_pinyin(&consumed_code);
        self.ai.invalidate_cache_for_pinyin(&self.input_buffer);

        if is_explicit || chosen.text.chars().count() <= 12 {
            self.core
                .commit_candidate_ex(&consumed_code, &chosen.text, None, is_explicit);
        }
        if chosen.segment_spans.len() > 1 {
            for span in &chosen.segment_spans {
                if span.text.chars().count() >= 2
                    && span.start < span.end
                    && span.end <= self.input_buffer.len()
                {
                    let seg_code = &self.input_buffer[span.start..span.end];
                    if seg_code != consumed_code {
                        self.core
                            .commit_candidate_ex(seg_code, &span.text, None, is_explicit);
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
        if self.committed_history.chars().count() > 64 {
            let chars: Vec<char> = self.committed_history.chars().collect();
            self.committed_history = chars[chars.len() - 64..].iter().collect();
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

// ============================================================================
// Windows COM / TSF 标准 DLL 导出函数（供 regsvr32.exe 与系统 CTF 加载器调用）
// ============================================================================

#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const c_void,
    riid: *const c_void,
    ppv: *mut *mut c_void,
) -> i32 {
    tsf_com::com_dll_get_class_object(rclsid, riid, ppv)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    tsf_com::com_dll_can_unload_now()
}

#[no_mangle]
pub unsafe extern "system" fn DllRegisterServer() -> i32 {
    #[cfg(target_os = "windows")]
    {
        tsf_com::com_dll_register_server()
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}

#[no_mangle]
pub unsafe extern "system" fn DllUnregisterServer() -> i32 {
    #[cfg(target_os = "windows")]
    {
        tsf_com::com_dll_unregister_server()
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
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
        let mut session = TsfKeySession::new(core, ai);

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
        let mut session = TsfKeySession::new(core, ai);
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
        let mut session = TsfKeySession::new(core, ai);

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
    }

    #[test]
    fn test_tsf_com_class_factory_and_interfaces() {
        unsafe {
            assert_eq!(DllCanUnloadNow(), tsf_com::S_OK);

            // 1. 获取 IClassFactory
            let mut cf_ptr: *mut c_void = std::ptr::null_mut();
            let hr = DllGetClassObject(
                &tsf_com::CLSID_RUST_AI_IME as *const _ as *const c_void,
                &tsf_com::IID_ICLASS_FACTORY as *const _ as *const c_void,
                &mut cf_ptr,
            );
            assert_eq!(hr, tsf_com::S_OK);
            assert!(!cf_ptr.is_null());

            // 2. 通过 TsfTextServiceCom::new_boxed 验证 ITfTextInputProcessorEx 与 ITfKeyEventSink 双接口转换
            let tip = tsf_com::TsfTextServiceCom::new_boxed();
            assert_eq!(DllCanUnloadNow(), tsf_com::S_FALSE);

            let unk_vtbl = *(tip as *mut *const tsf_com::IUnknownVtbl);
            let mut sink_ptr: *mut c_void = std::ptr::null_mut();
            let hr_sink = ((*unk_vtbl).query_interface)(
                tip as *mut c_void,
                &tsf_com::IID_ITF_KEY_EVENT_SINK,
                &mut sink_ptr,
            );
            assert_eq!(hr_sink, tsf_com::S_OK);
            assert!(!sink_ptr.is_null());

            // 从 sink_ptr 再 QueryInterface 回 ITfTextInputProcessorEx
            let sink_unk_vtbl = *(sink_ptr as *mut *const tsf_com::IUnknownVtbl);
            let mut tip_ex_ptr: *mut c_void = std::ptr::null_mut();
            let hr_tip = ((*sink_unk_vtbl).query_interface)(
                sink_ptr,
                &tsf_com::IID_ITF_TEXT_INPUT_PROCESSOR_EX,
                &mut tip_ex_ptr,
            );
            assert_eq!(hr_tip, tsf_com::S_OK);
            assert_eq!(tip_ex_ptr, tip as *mut c_void);

            // 释放全部引用后 DllCanUnloadNow 应恢复 S_OK
            ((*unk_vtbl).release)(tip_ex_ptr);
            ((*sink_unk_vtbl).release)(sink_ptr);
            assert_eq!(((*unk_vtbl).release)(tip as *mut c_void), 0);
            assert_eq!(DllCanUnloadNow(), tsf_com::S_OK);
        }
    }
}
