//! `ime-desktop-ui`: 原生 Win32 / GDI 深色主题桌面输入法可视化体验窗
//!
//! 遵循主流拼音输入法（搜狗/微软拼音/Rime）的统一交互直觉：
//! 1. **拼音预编辑栏内置光标 (`|`) 与音节高亮**：
//!    - 键盘按 **`←` / `→` 左右方向键**（或 **`Ctrl + ←/→`** 按音节跳动，或**鼠标直接点击预编辑栏中的任意拼音音节**），光标 `|` 直接在拼音音节与字母间移动。
//! 2. **句中拼音输错直接改 (`Backspace` / `Delete` / `A-Z`)**：
//!    - 当光标 `|` 移动到句中任意拼音位置时，直接按 `Backspace` 退格或输入新字母 `a-z`，即可原地修改输错的拼音，整句实时重算，改完直接按空格即可整句上屏。
//! 3. **光标所在音节与下方候选词列表自动联动**：
//!    - 光标 `|` 停留在哪个拼音音节上，下方候选词列表 `1~5` 就自动展示从该音节出发的候选词/单字/英文词；
//!    - 按数字键 `1~5` 或鼠标点击候选词，即可替换该音节对应的词并自动将光标移回句尾。

use ime_ai::DualModeAiOrchestrator;
use ime_core::{AppScene, ImeCoreEngine, SegmentSpan};
use ime_lexicon::CompiledLexicon;
use ime_tsf_win::{TsfKeySession, VirtualKey};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::Mutex;

struct UiAppState {
    session: TsfKeySession,
    hover_zone: HitZone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitZone {
    None,
    SceneTab(usize),
    PresetBtn(usize),
    ClearContextBtn,
    ClearPinyinBtn,
    PrevPageBtn,
    NextPageBtn,
    PrevSyllableBtn,
    NextSyllableBtn,
    CommitFullSentenceBtn,
    AcceptGhostBtn,
    PinyinSyllable(usize),
    PinyinEndZone,
    CandidateRow(usize),
}

#[derive(Clone, Copy)]
struct ClickHotspot {
    rect: RECT,
    zone: HitZone,
}

static APP_STATE: Mutex<Option<UiAppState>> = Mutex::new(None);

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

#[repr(C)]
#[derive(Copy, Clone)]
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl RECT {
    #[inline]
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

#[repr(C)]
struct WNDCLASSEXW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: isize,
    h_icon: isize,
    h_cursor: isize,
    hbr_background: isize,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: isize,
}

#[repr(C)]
struct MSG {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: POINT,
}

#[repr(C)]
struct PAINTSTRUCT {
    hdc: isize,
    f_erase: i32,
    rc_paint: RECT,
    f_restore: i32,
    f_inc_update: i32,
    rgb_reserved: [u8; 32],
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> isize;
    fn GetProcAddress(h_module: isize, lp_proc_name: *const u8) -> *const usize;
}

unsafe fn disable_window_system_ime(hwnd: isize) {
    let dll = to_wide("imm32.dll");
    let h_mod = LoadLibraryW(dll.as_ptr());
    if h_mod != 0 {
        let proc = GetProcAddress(h_mod, b"ImmAssociateContext\0".as_ptr());
        if !proc.is_null() {
            let func: unsafe extern "system" fn(isize, isize) -> isize =
                std::mem::transmute(proc);
            func(hwnd, 0);
        }
    }
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> u16;
    fn CreateWindowExW(
        dw_ex_style: u32,
        lp_class_name: *const u16,
        lp_window_name: *const u16,
        dw_style: u32,
        x: i32,
        y: i32,
        n_width: i32,
        n_height: i32,
        h_wnd_parent: isize,
        h_menu: isize,
        h_instance: isize,
        lp_param: *const u8,
    ) -> isize;
    fn ShowWindow(h_wnd: isize, n_cmd_show: i32) -> i32;
    fn UpdateWindow(h_wnd: isize) -> i32;
    fn BringWindowToTop(h_wnd: isize) -> i32;
    fn SetForegroundWindow(h_wnd: isize) -> i32;
    fn SetFocus(h_wnd: isize) -> isize;
    fn LoadCursorW(h_instance: isize, lp_cursor_name: *const u16) -> isize;
    fn SetCursor(h_cursor: isize) -> isize;
    fn GetMessageW(lp_msg: *mut MSG, h_wnd: isize, w_msg_filter_min: u32, w_msg_filter_max: u32) -> i32;
    fn TranslateMessage(lp_msg: *const MSG) -> i32;
    fn DispatchMessageW(lp_msg: *const MSG) -> isize;
    fn DefWindowProcW(h_wnd: isize, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn PostQuitMessage(n_exit_code: i32);
    fn BeginPaint(h_wnd: isize, lp_paint: *mut PAINTSTRUCT) -> isize;
    fn EndPaint(h_wnd: isize, lp_paint: *const PAINTSTRUCT) -> i32;
    fn InvalidateRect(h_wnd: isize, lp_rect: *const RECT, b_erase: i32) -> i32;
    fn FillRect(h_dc: isize, lprc: *const RECT, hbr: isize) -> i32;
    fn GetKeyState(n_virt_key: i32) -> i16;
    fn OpenDesktopW(lpsz_desktop: *const u16, dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> isize;
    fn SetThreadDesktop(h_desktop: isize) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateSolidBrush(color: u32) -> isize;
    fn DeleteObject(ho: isize) -> i32;
    fn SetBkMode(hdc: isize, mode: i32) -> i32;
    fn SetTextColor(hdc: isize, color: u32) -> u32;
    fn TextOutW(hdc: isize, x: i32, y: i32, lp_string: *const u16, c: i32) -> i32;
    fn GetTextExtentPoint32W(
        hdc: isize,
        lp_string: *const u16,
        c: i32,
        psizl: *mut [i32; 2],
    ) -> i32;
    fn CreateFontW(
        c_height: i32,
        c_width: i32,
        c_escapement: i32,
        c_orientation: i32,
        c_weight: i32,
        b_italic: u32,
        b_underline: u32,
        b_strike_out: u32,
        i_char_set: u32,
        i_out_precision: u32,
        i_clip_precision: u32,
        i_quality: u32,
        i_pitch_and_family: u32,
        psz_face_name: *const u16,
    ) -> isize;
    fn SelectObject(hdc: isize, h: isize) -> isize;
    fn CreateCompatibleDC(hdc: isize) -> isize;
    fn CreateCompatibleBitmap(hdc: isize, cx: i32, cy: i32) -> isize;
    fn DeleteDC(hdc: isize) -> i32;
    fn BitBlt(hdc: isize, x: i32, y: i32, cx: i32, cy: i32, hdc_src: isize, x1: i32, y1: i32, rop: u32) -> i32;
}

const WM_DESTROY: u32 = 0x0002;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_KEYDOWN: u32 = 0x0100;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WS_OVERLAPPEDWINDOW: u32 = 0x00CF0000;
const WS_VISIBLE: u32 = 0x10000000;
const WS_EX_APPWINDOW: u32 = 0x00040000;
const SW_SHOWNORMAL: i32 = 1;
const SW_SHOW: i32 = 5;
const SW_RESTORE: i32 = 9;
const TRANSPARENT: i32 = 1;
const SRCCOPY: u32 = 0x00CC0020;

const WIN_W: i32 = 1050;
const WIN_H: i32 = 680;

const SCENES: &[(AppScene, &str)] = &[
    (AppScene::CodeEditor, "1. 代码研发 (IDE)"),
    (AppScene::Chat, "2. 即时通讯 (微信)"),
    (AppScene::Academic, "3. 学术/数学写作"),
    (AppScene::Document, "4. 公文/文档撰写"),
    (AppScene::General, "5. 通用日常"),
];

struct PresetItem {
    label: &'static str,
    scene: AppScene,
    context: &'static str,
    pinyin: &'static str,
}

const PRESETS: &[PresetItem] = &[
    PresetItem {
        label: "长句混输: Claude+NVIDIA+AMD+CPU",
        scene: AppScene::CodeEditor,
        context: "",
        pinyin: "woshiyongclaudedeapizuochengxuyunxingzainvidiadexiankayijiamddecpushang",
    },
    PresetItem {
        label: "混输: wozaiyongrustxieapi",
        scene: AppScene::CodeEditor,
        context: "",
        pinyin: "wozaiyongrustxieapi",
    },
    PresetItem {
        label: "数学语境: 傅里叶+jishu -> 级数",
        scene: AppScene::Academic,
        context: "这道微积分题需要展开成傅里叶",
        pinyin: "jishu",
    },
    PresetItem {
        label: "编程语境: 计算机+jishu -> 技术",
        scene: AppScene::CodeEditor,
        context: "我们团队正在攻关核心计算机",
        pinyin: "jishu",
    },
    PresetItem {
        label: "释怀语境: 过去的事+buzaiyile",
        scene: AppScene::Chat,
        context: "算了，过去的事我已经",
        pinyin: "buzaiyile",
    },
    PresetItem {
        label: "相声语境: 搞笑相声+buzaiyile",
        scene: AppScene::Chat,
        context: "听完这段幽默搞笑相声怎么",
        pinyin: "buzaiyile",
    },
];

#[inline]
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// 估算单个拼音音节块在预编辑栏中的像素宽度（紧凑排布确保 22+ 音节超长句单行完整显示）
fn estimate_syllable_width(code_key: &str, has_caret: bool) -> i32 {
    let char_count = code_key.chars().count() as i32;
    let caret_extra = if has_caret { 6 } else { 0 };
    (char_count * 8 + 11 + caret_extra).max(23)
}

/// 将光标 `|` 插入到对应音节的字符串展示中
fn format_syllable_with_caret(
    span: &SegmentSpan,
    cursor_byte_pos: usize,
    show_caret_inside: bool,
) -> String {
    if !show_caret_inside {
        return span.code_key.clone();
    }
    let rel = cursor_byte_pos
        .saturating_sub(span.start)
        .min(span.code_key.len());
    let mut s = span.code_key.clone();
    if s.is_char_boundary(rel) {
        s.insert(rel, '|');
    } else {
        s.push('|');
    }
    s
}

fn build_hotspots(syllables: &[SegmentSpan], active_syl: Option<usize>) -> Vec<ClickHotspot> {
    let mut spots = Vec::new();
    // 1. 顶部 5 个场景切换按钮
    for i in 0..SCENES.len() {
        let left = 135 + (i as i32) * 172;
        spots.push(ClickHotspot {
            rect: RECT {
                left,
                top: 56,
                right: left + 162,
                bottom: 86,
            },
            zone: HitZone::SceneTab(i),
        });
    }
    // 2. 6 个一键测试预设按钮
    for i in 0..PRESETS.len() {
        let row = (i / 3) as i32;
        let col = (i % 3) as i32;
        let left = 135 + col * 292;
        let top = 94 + row * 34;
        spots.push(ClickHotspot {
            rect: RECT {
                left,
                top,
                right: left + 282,
                bottom: top + 28,
            },
            zone: HitZone::PresetBtn(i),
        });
    }
    // 3. 清空上文按钮
    spots.push(ClickHotspot {
        rect: RECT {
            left: 890,
            top: 174,
            right: 1005,
            bottom: 204,
        },
        zone: HitZone::ClearContextBtn,
    });
    // 4. 采纳 Ghost Text 按钮
    spots.push(ClickHotspot {
        rect: RECT {
            left: 32,
            top: 246,
            right: 1005,
            bottom: 276,
        },
        zone: HitZone::AcceptGhostBtn,
    });
    // 5. 预编辑拼音音节栏（占据整行宽度 112..1008，完整容纳全部 22+ 个拼音音节）
    let mut py_x = 112;
    let py_y = 298;
    let total_syls = syllables.len();
    for (idx, syl) in syllables.iter().enumerate() {
        let has_caret = active_syl == Some(idx) || (active_syl.is_none() && idx + 1 == total_syls);
        let w = estimate_syllable_width(&syl.code_key, has_caret);
        if py_x + w > 1008 {
            break;
        }
        spots.push(ClickHotspot {
            rect: RECT {
                left: py_x,
                top: py_y,
                right: py_x + w,
                bottom: py_y + 28,
            },
            zone: HitZone::PinyinSyllable(idx),
        });
        py_x += w + 3;
    }
    // 点击拼音末尾空白处回到句尾整句模式
    if py_x < 1008 {
        spots.push(ClickHotspot {
            rect: RECT {
                left: py_x,
                top: py_y,
                right: 1008,
                bottom: py_y + 28,
            },
            zone: HitZone::PinyinEndZone,
        });
    }

    // 6. 第二行右侧控制按钮组：◀音节 | 音节▶ | 上页(-) | 下页(=) | 整句上屏 | 清空(Esc)
    spots.push(ClickHotspot {
        rect: RECT {
            left: 642,
            top: 336,
            right: 696,
            bottom: 366,
        },
        zone: HitZone::PrevSyllableBtn,
    });
    spots.push(ClickHotspot {
        rect: RECT {
            left: 700,
            top: 336,
            right: 754,
            bottom: 366,
        },
        zone: HitZone::NextSyllableBtn,
    });
    spots.push(ClickHotspot {
        rect: RECT {
            left: 760,
            top: 336,
            right: 818,
            bottom: 366,
        },
        zone: HitZone::PrevPageBtn,
    });
    spots.push(ClickHotspot {
        rect: RECT {
            left: 822,
            top: 336,
            right: 880,
            bottom: 366,
        },
        zone: HitZone::NextPageBtn,
    });
    spots.push(ClickHotspot {
        rect: RECT {
            left: 886,
            top: 336,
            right: 948,
            bottom: 366,
        },
        zone: HitZone::CommitFullSentenceBtn,
    });
    spots.push(ClickHotspot {
        rect: RECT {
            left: 952,
            top: 336,
            right: 1008,
            bottom: 366,
        },
        zone: HitZone::ClearPinyinBtn,
    });

    // 7. 候选词列表行 0..5
    for idx in 0..5 {
        let y = 412 + (idx as i32) * 40;
        spots.push(ClickHotspot {
            rect: RECT {
                left: 32,
                top: y - 4,
                right: 1008,
                bottom: y + 32,
            },
            zone: HitZone::CandidateRow(idx),
        });
    }
    spots
}

fn hit_test(
    x: i32,
    y: i32,
    syllables: &[SegmentSpan],
    active_syl: Option<usize>,
) -> (HitZone, Option<RECT>) {
    for sp in build_hotspots(syllables, active_syl) {
        if sp.rect.contains(x, y) {
            return (sp.zone, Some(sp.rect));
        }
    }
    (HitZone::None, None)
}

unsafe fn draw_text(hdc: isize, x: i32, y: i32, text: &str, color: u32) {
    SetTextColor(hdc, color);
    let wide: Vec<u16> = OsStr::new(text).encode_wide().collect();
    TextOutW(hdc, x, y, wide.as_ptr(), wide.len() as i32);
}

unsafe fn measure_text_width(hdc: isize, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let wide: Vec<u16> = OsStr::new(text).encode_wide().collect();
    let mut sz = [0i32; 2];
    if GetTextExtentPoint32W(hdc, wide.as_ptr(), wide.len() as i32, &mut sz) != 0 {
        sz[0]
    } else {
        (text.chars().count() as i32) * 14
    }
}

unsafe fn fill_rect_color(hdc: isize, left: i32, top: i32, right: i32, bottom: i32, color: u32) {
    let rc = RECT {
        left,
        top,
        right,
        bottom,
    };
    let brush = CreateSolidBrush(color);
    FillRect(hdc, &rc, brush);
    DeleteObject(brush);
}

unsafe fn fill_rect_struct(hdc: isize, rc: &RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rc, brush);
    DeleteObject(brush);
}

fn truncate_left_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let tail: String = chars[chars.len() - max_chars..].iter().collect();
        format!("...{}", tail)
    }
}

fn apply_preset_to_session(session: &mut TsfKeySession, idx: usize) {
    if let Some(p) = PRESETS.get(idx) {
        session.active_scene = p.scene;
        session.committed_history = p.context.to_string();
        session.pending_ghost_text = None;
        session.input_buffer.clear();
        session.cursor_pos = 0;
        session.cursor_syllable_mode = false;
        session.last_op_was_pinyin_edit = false;
        session.current_candidates.clear();
        session.pinned_segments.clear();
        session.last_full_sentence.clear();
        session.last_full_segments.clear();
        session.last_syllable_spans.clear();
        for ch in p.pinyin.chars() {
            let _ = session.process_key(VirtualKey::Char(ch));
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, w_param: usize, l_param: isize) -> isize {
    match msg {
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let mut zone = HitZone::None;
            if let Ok(mut guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    let active_syl = st.session.active_syllable_index();
                    let (z, _) = hit_test(x, y, &st.session.last_syllable_spans, active_syl);
                    zone = z;
                    if st.hover_zone != zone {
                        st.hover_zone = zone;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            if zone != HitZone::None {
                SetCursor(LoadCursorW(0, 32649 as *const u16)); // IDC_HAND
            } else {
                SetCursor(LoadCursorW(0, 32512 as *const u16)); // IDC_ARROW
            }
            0
        }
        WM_RBUTTONDOWN => {
            // 鼠标右键点击：在拼音音节上向右移动一个音节（符合“鼠标左右键在拼音音节上移动”的直觉）
            SetFocus(hwnd);
            if let Ok(mut guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.session.process_key(VirtualKey::CtrlRight);
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_LBUTTONDOWN => {
            SetFocus(hwnd);
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            if let Ok(mut guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    let active_syl = st.session.active_syllable_index();
                    let (zone, rect_opt) =
                        hit_test(x, y, &st.session.last_syllable_spans, active_syl);
                    match zone {
                        HitZone::SceneTab(i) => {
                            if let Some(&(sc, _)) = SCENES.get(i) {
                                st.session.active_scene = sc;
                                if !st.session.input_buffer.is_empty() {
                                    st.session.recompute_candidates();
                                }
                            }
                        }
                        HitZone::PresetBtn(i) => {
                            apply_preset_to_session(&mut st.session, i);
                        }
                        HitZone::ClearContextBtn => {
                            st.session.committed_history.clear();
                            st.session.pending_ghost_text = None;
                            if !st.session.input_buffer.is_empty() {
                                st.session.recompute_candidates();
                            }
                        }
                        HitZone::PrevSyllableBtn => {
                            st.session.process_key(VirtualKey::CtrlLeft);
                        }
                        HitZone::NextSyllableBtn => {
                            st.session.process_key(VirtualKey::CtrlRight);
                        }
                        HitZone::PrevPageBtn => {
                            st.session.process_key(VirtualKey::PageUp);
                        }
                        HitZone::NextPageBtn => {
                            st.session.process_key(VirtualKey::PageDown);
                        }
                        HitZone::ClearPinyinBtn => {
                            st.session.process_key(VirtualKey::Escape);
                        }
                        HitZone::CommitFullSentenceBtn => {
                            st.session.commit_full_sentence();
                        }
                        HitZone::AcceptGhostBtn => {
                            st.session.process_key(VirtualKey::Tab);
                        }
                        HitZone::PinyinSyllable(syl_idx) => {
                            // 计算鼠标点击在该音节内部的相对字母位置（支持点击音节定位、再次点击音节内部精确到字母改错！）
                            let inner_offset = if let (Some(rc), Some(span)) =
                                (rect_opt, st.session.last_syllable_spans.get(syl_idx))
                            {
                                let w = (rc.right - rc.left).max(1);
                                let rel_x = (x - rc.left).clamp(0, w);
                                let len = span.end.saturating_sub(span.start).max(1);
                                let char_off = ((rel_x * len as i32 + w / 2) / w) as usize;
                                Some(char_off.clamp(1, len))
                            } else {
                                None
                            };
                            st.session.click_pinyin_syllable(syl_idx, inner_offset);
                        }
                        HitZone::PinyinEndZone => {
                            st.session.return_cursor_to_end();
                        }
                        HitZone::CandidateRow(idx) => {
                            st.session.process_key(VirtualKey::Digit((idx + 1) as u8));
                        }
                        HitZone::None => {}
                    }
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_KEYDOWN => {
            let vk = w_param as u32;
            if let Ok(mut guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    match vk {
                        0x70 => {
                            st.session.active_scene = AppScene::CodeEditor;
                            st.session.recompute_candidates();
                        }
                        0x71 => {
                            st.session.active_scene = AppScene::Chat;
                            st.session.recompute_candidates();
                        }
                        0x72 => {
                            st.session.active_scene = AppScene::Academic;
                            st.session.recompute_candidates();
                        }
                        0x73 => {
                            st.session.active_scene = AppScene::Document;
                            st.session.recompute_candidates();
                        }
                        0x74 => {
                            st.session.active_scene = AppScene::General;
                            st.session.recompute_candidates();
                        }
                        _ => {
                            let shift = (GetKeyState(0x10) as u16 & 0x8000) != 0;
                            let ctrl = (GetKeyState(0x11) as u16 & 0x8000) != 0;
                            let vkey = VirtualKey::from_win32_vk_ex(vk, shift, ctrl);
                            let _ = st.session.process_key(vkey);
                        }
                    }
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc_screen = BeginPaint(hwnd, &mut ps);
            let hdc = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, WIN_W, WIN_H);
            let old_bm = SelectObject(hdc, hbm);

            SetBkMode(hdc, TRANSPARENT);
            fill_rect_color(hdc, 0, 0, WIN_W, WIN_H, rgb(18, 20, 27));

            let font_face = to_wide("Microsoft YaHei UI");
            let h_font_title = CreateFontW(20, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, font_face.as_ptr());
            let h_font_body = CreateFontW(17, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, font_face.as_ptr());
            let h_font_bold = CreateFontW(19, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, font_face.as_ptr());
            let old_font = SelectObject(hdc, h_font_title);

            // 顶部导航栏
            fill_rect_color(hdc, 0, 0, WIN_W, 46, rgb(28, 33, 46));
            draw_text(
                hdc,
                20,
                12,
                "Rust AI 拼音输入法桌面体验台 (光标在拼音音节上左右移动 · 原地改错拼音 · 音节候选自动联动)",
                rgb(115, 210, 255),
            );

            SelectObject(hdc, h_font_body);

            if let Ok(guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_ref() {
                    let session = &st.session;
                    let hover = st.hover_zone;
                    let vm = session.build_view_model();
                    let hotspots = build_hotspots(&vm.syllable_spans, vm.active_syllable_index);

                    // 1. 绘制场景切换标签栏
                    draw_text(hdc, 20, 62, "焦点应用场景:", rgb(160, 175, 200));
                    for (i, &(sc, label)) in SCENES.iter().enumerate() {
                        let sp = hotspots.iter().find(|h| h.zone == HitZone::SceneTab(i)).unwrap();
                        let active = session.active_scene == sc;
                        let bg = if active {
                            rgb(45, 105, 175)
                        } else if hover == HitZone::SceneTab(i) {
                            rgb(50, 58, 78)
                        } else {
                            rgb(32, 37, 50)
                        };
                        fill_rect_struct(hdc, &sp.rect, bg);
                        draw_text(
                            hdc,
                            sp.rect.left + 12,
                            sp.rect.top + 6,
                            label,
                            if active { rgb(255, 255, 255) } else { rgb(200, 210, 230) },
                        );
                    }

                    // 2. 绘制 6 个一键测试预设按钮
                    draw_text(hdc, 20, 100, "鼠标一键测试:", rgb(160, 175, 200));
                    for (i, preset) in PRESETS.iter().enumerate() {
                        let sp = hotspots.iter().find(|h| h.zone == HitZone::PresetBtn(i)).unwrap();
                        let bg = if hover == HitZone::PresetBtn(i) {
                            rgb(60, 75, 105)
                        } else {
                            rgb(35, 42, 58)
                        };
                        fill_rect_struct(hdc, &sp.rect, bg);
                        draw_text(hdc, sp.rect.left + 10, sp.rect.top + 5, preset.label, rgb(220, 230, 250));
                    }

                    // 3. 模拟文档编辑框区域
                    fill_rect_color(hdc, 20, 168, 1020, 282, rgb(26, 30, 40));
                    draw_text(
                        hdc,
                        32,
                        180,
                        "【当前编辑器上屏文本 (Surrounding Context)】：",
                        rgb(140, 155, 185),
                    );

                    // 清空上文按钮
                    let clear_ctx_sp = hotspots.iter().find(|h| h.zone == HitZone::ClearContextBtn).unwrap();
                    fill_rect_struct(
                        hdc,
                        &clear_ctx_sp.rect,
                        if hover == HitZone::ClearContextBtn {
                            rgb(110, 55, 65)
                        } else {
                            rgb(65, 40, 48)
                        },
                    );
                    draw_text(hdc, clear_ctx_sp.rect.left + 20, clear_ctx_sp.rect.top + 6, "清空上文", rgb(255, 210, 210));

                    SelectObject(hdc, h_font_bold);
                    let history_display = if session.committed_history.is_empty() {
                        "（上文为空 —— 可点击上方「鼠标一键测试」按钮，或直接敲击键盘 A-Z 输入拼音）".to_string()
                    } else {
                        truncate_left_chars(&session.committed_history, 48)
                    };
                    draw_text(hdc, 36, 212, &history_display, rgb(245, 248, 255));
                    SelectObject(hdc, h_font_body);

                    if let Some(ghost) = &session.pending_ghost_text {
                        let ghost_sp = hotspots.iter().find(|h| h.zone == HitZone::AcceptGhostBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &ghost_sp.rect,
                            if hover == HitZone::AcceptGhostBtn {
                                rgb(35, 85, 65)
                            } else {
                                rgb(28, 62, 48)
                            },
                        );
                        let ghost_str = format!(
                            "✨ AI 灰字续写预测: \"{}{}\"   [点击此处 或 按 Tab 键一键补全]",
                            session.committed_history, ghost
                        );
                        draw_text(hdc, 42, 252, &ghost_str, rgb(130, 245, 180));
                    }

                    // 4. 悬浮候选词窗口区域
                    fill_rect_color(hdc, 20, 290, 1020, 628, rgb(30, 36, 50));
                    if vm.visible {
                        // 4.1 预编辑拼音音节栏（整行全宽 112..1008，带实时光标 `|`，支持 ←/→ 移动、点击任意拼音音节、原地退格改拼音）
                        SelectObject(hdc, h_font_bold);
                        draw_text(hdc, 32, 303, "拼音音节:", rgb(180, 200, 235));
                        SelectObject(hdc, h_font_body);

                        let total_syls = vm.syllable_spans.len();
                        for (idx, syl) in vm.syllable_spans.iter().enumerate() {
                            if let Some(syl_sp) = hotspots.iter().find(|h| h.zone == HitZone::PinyinSyllable(idx)) {
                                let is_active = vm.active_syllable_index == Some(idx);
                                let is_tail_caret = vm.active_syllable_index.is_none()
                                    && idx + 1 == total_syls
                                    && vm.cursor_byte_pos >= syl.end;
                                let is_hover = hover == HitZone::PinyinSyllable(idx);
                                let is_pinned = session
                                    .pinned_segments
                                    .iter()
                                    .any(|(&s, &(e, _))| syl.start >= s && syl.end <= e);

                                let bg = if is_active {
                                    rgb(215, 120, 20) // 当前光标所在的活跃拼音音节（亮橙色高亮）
                                } else if is_pinned {
                                    rgb(42, 115, 85) // 已手动选词锁定的音节
                                } else if is_hover {
                                    rgb(62, 80, 118)
                                } else {
                                    rgb(40, 48, 66)
                                };
                                fill_rect_struct(hdc, &syl_sp.rect, bg);

                                let display_syl = format_syllable_with_caret(
                                    syl,
                                    vm.cursor_byte_pos,
                                    is_active || is_tail_caret,
                                );
                                let text_col = if is_active || is_tail_caret {
                                    rgb(255, 245, 150)
                                } else {
                                    rgb(235, 242, 255)
                                };
                                draw_text(
                                    hdc,
                                    syl_sp.rect.left + 5,
                                    syl_sp.rect.top + 5,
                                    &display_syl,
                                    text_col,
                                );
                            }
                        }

                        // 4.2 第二行：左侧【实时整句预览】，右侧【◀音节 | 音节▶ | 上页(-) | 下页(=) | 整句上屏 | 清空(Esc)】
                        fill_rect_color(hdc, 32, 336, 636, 366, rgb(24, 30, 42));
                        let prefix_label = "整句: ";
                        draw_text(hdc, 40, 342, prefix_label, rgb(148, 175, 205));
                        let mut cur_x = 40 + measure_text_width(hdc, prefix_label);
                        let max_x = 630;
                        for (part_text, is_pinned) in &vm.full_sentence_parts {
                            if part_text.is_empty() || cur_x >= max_x {
                                continue;
                            }
                            let w = measure_text_width(hdc, part_text);
                            if *is_pinned {
                                let right_x = (cur_x + w + 6).min(max_x);
                                fill_rect_color(hdc, cur_x, 339, right_x, 363, rgb(38, 105, 78));
                                fill_rect_color(hdc, cur_x, 361, right_x, 363, rgb(255, 210, 90));
                                SelectObject(hdc, h_font_bold);
                                draw_text(hdc, cur_x + 3, 342, part_text, rgb(255, 238, 135));
                                SelectObject(hdc, h_font_body);
                                cur_x = right_x + 3;
                            } else {
                                draw_text(hdc, cur_x, 342, part_text, rgb(140, 235, 195));
                                cur_x += w;
                            }
                        }
                        if let Some(syl_idx) = vm.active_syllable_index {
                            if let Some(syl) = vm.syllable_spans.get(syl_idx) {
                                let hint_str = format!("  (当前音节 '{}'->「{}」)", syl.code_key, syl.text);
                                let hw = measure_text_width(hdc, &hint_str);
                                if cur_x + hw <= max_x {
                                    draw_text(hdc, cur_x, 342, &hint_str, rgb(145, 175, 215));
                                }
                            }
                        }

                        let prev_syl_sp = hotspots.iter().find(|h| h.zone == HitZone::PrevSyllableBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &prev_syl_sp.rect,
                            if hover == HitZone::PrevSyllableBtn {
                                rgb(65, 85, 120)
                            } else {
                                rgb(42, 52, 72)
                            },
                        );
                        draw_text(hdc, prev_syl_sp.rect.left + 5, prev_syl_sp.rect.top + 6, "◀音节", rgb(210, 230, 255));

                        let next_syl_sp = hotspots.iter().find(|h| h.zone == HitZone::NextSyllableBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &next_syl_sp.rect,
                            if hover == HitZone::NextSyllableBtn {
                                rgb(65, 85, 120)
                            } else {
                                rgb(42, 52, 72)
                            },
                        );
                        draw_text(hdc, next_syl_sp.rect.left + 5, next_syl_sp.rect.top + 6, "音节▶", rgb(210, 230, 255));

                        let prev_sp = hotspots.iter().find(|h| h.zone == HitZone::PrevPageBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &prev_sp.rect,
                            if hover == HitZone::PrevPageBtn {
                                rgb(60, 75, 105)
                            } else {
                                rgb(40, 48, 66)
                            },
                        );
                        draw_text(hdc, prev_sp.rect.left + 6, prev_sp.rect.top + 6, "上页(-)", rgb(210, 225, 250));

                        let next_sp = hotspots.iter().find(|h| h.zone == HitZone::NextPageBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &next_sp.rect,
                            if hover == HitZone::NextPageBtn {
                                rgb(60, 75, 105)
                            } else {
                                rgb(40, 48, 66)
                            },
                        );
                        draw_text(hdc, next_sp.rect.left + 6, next_sp.rect.top + 6, "下页(=)", rgb(210, 225, 250));

                        let commit_full_sp = hotspots
                            .iter()
                            .find(|h| h.zone == HitZone::CommitFullSentenceBtn)
                            .unwrap();
                        fill_rect_struct(
                            hdc,
                            &commit_full_sp.rect,
                            if hover == HitZone::CommitFullSentenceBtn {
                                rgb(45, 130, 95)
                            } else {
                                rgb(35, 95, 70)
                            },
                        );
                        draw_text(
                            hdc,
                            commit_full_sp.rect.left + 5,
                            commit_full_sp.rect.top + 6,
                            "整句上屏",
                            rgb(235, 255, 245),
                        );

                        let clear_py_sp = hotspots.iter().find(|h| h.zone == HitZone::ClearPinyinBtn).unwrap();
                        fill_rect_struct(
                            hdc,
                            &clear_py_sp.rect,
                            if hover == HitZone::ClearPinyinBtn {
                                rgb(95, 60, 90)
                            } else {
                                rgb(58, 45, 70)
                            },
                        );
                        let clear_label = if vm.active_syllable_index.is_some() {
                            "回句尾"
                        } else {
                            "清空"
                        };
                        draw_text(hdc, clear_py_sp.rect.left + 10, clear_py_sp.rect.top + 6, clear_label, rgb(245, 220, 255));

                        // 4.3 状态提示行
                        let hint_color = if vm.active_syllable_index.is_some() {
                            rgb(255, 200, 110)
                        } else {
                            rgb(155, 175, 210)
                        };
                        let hint_line = format!(
                            "💡 {}   (按 ← → 移动拼音光标可直接退格改错拼音，第 {}/{} 页)",
                            vm.ai_semantic_hint,
                            vm.page_index + 1,
                            (vm.total_candidates.saturating_sub(1) / vm.page_size.max(1)) + 1
                        );
                        draw_text(hdc, 34, 376, &hint_line, hint_color);

                        // 4.4 候选词列表 1..5（光标在句尾时展示整句候选，光标移到某音节时自动联动展示该音节候选词！）
                        for (idx, cand) in vm.visible_candidates.iter().enumerate() {
                            let row_sp = hotspots.iter().find(|h| h.zone == HitZone::CandidateRow(idx)).unwrap();
                            let is_hover = hover == HitZone::CandidateRow(idx);
                            let is_selected = idx == vm.selected_candidate_index;
                            if is_selected {
                                fill_rect_struct(
                                    hdc,
                                    &row_sp.rect,
                                    if is_hover { rgb(65, 95, 145) } else { rgb(48, 72, 112) },
                                );
                            } else if is_hover {
                                fill_rect_struct(hdc, &row_sp.rect, rgb(42, 52, 74));
                            }
                            let color = if is_selected {
                                rgb(255, 255, 255)
                            } else {
                                rgb(210, 220, 238)
                            };
                            let action_tip = if vm.active_syllable_index.is_some() {
                                "选定该音节词并自动合成整句"
                            } else {
                                "点击或按数字键上屏"
                            };
                            let line = format!(
                                "{}.  {}      {}    (得分: {:.1}  |  {})",
                                idx + 1,
                                cand.text,
                                cand.tag,
                                cand.final_score,
                                action_tip
                            );
                            draw_text(hdc, 44, row_sp.rect.top + 7, &line, color);
                        }
                    } else {
                        draw_text(
                            hdc,
                            36,
                            335,
                            "候选框待命：请直接敲击键盘字母 A-Z 输入拼音，或用鼠标点击上方 6 个「鼠标一键测试」按钮体验！",
                            rgb(160, 175, 205),
                        );
                    }
                }
            }

            BitBlt(hdc_screen, 0, 0, WIN_W, WIN_H, hdc, 0, 0, SRCCOPY);

            SelectObject(hdc, old_font);
            DeleteObject(h_font_title);
            DeleteObject(h_font_body);
            DeleteObject(h_font_bold);
            SelectObject(hdc, old_bm);
            DeleteObject(hbm);
            DeleteDC(hdc);
            EndPaint(hwnd, &ps);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    // 词库查找根目录：可执行文件所在目录（`load_or_build_default` 还会依次尝试其上级目录、当前工作目录与 %LOCALAPPDATA%\RustAiPinyinIME）
    let root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let lexicon = CompiledLexicon::load_or_build_default(&root)?;
    let mut core = ImeCoreEngine::new(lexicon);
    let persistent_cfg = ime_tsf_win::ImePersistentConfig::load_or_default();
    core.fuzzy_config = persistent_cfg.fuzzy_config.clone();
    let ai = DualModeAiOrchestrator::new(persistent_cfg.ai_config.clone());
    if !args.iter().any(|a| a == "--self-test") {
        core.enable_persistence(ime_core::UserDict::default_storage_path());
    }
    let mut session = TsfKeySession::new(core, ai);
    session.active_scene = AppScene::CodeEditor;

    // 默认预填第 0 组演示：超长中英硬件混输句
    apply_preset_to_session(&mut session, 0);

    if args.iter().any(|a| a == "--self-test") {
        let vm = session.build_view_model();
        println!("=== [ime-desktop-ui Self-Test] ===");
        println!("  - 词库加载词条数 : {}", session.core.lexicon.entry_count());
        println!("  - 预编辑音节数量 : {}", vm.syllable_spans.len());
        println!(
            "  - 预编辑音节列表 : {:?}",
            vm.syllable_spans.iter().map(|s| &s.code_key).collect::<Vec<_>>()
        );
        println!(
            "  - 悬浮窗首选候选 : {} {}",
            vm.visible_candidates[0].text, vm.visible_candidates[0].tag
        );
        return Ok(());
    }

    *APP_STATE.lock().unwrap() = Some(UiAppState {
        session,
        hover_zone: HitZone::None,
    });

    unsafe {
        let default_desk = to_wide("Default");
        let h_desk = OpenDesktopW(default_desk.as_ptr(), 0, 0, 0x01FF);
        if h_desk != 0 {
            SetThreadDesktop(h_desk);
        }

        let h_instance = GetModuleHandleW(null());
        let h_cursor = LoadCursorW(0, 32512 as *const u16);
        let class_name = to_wide("RustAiPinyinImeDesktopClassV4");
        let window_title = to_wide("Rust AI 拼音输入法 - 桌面交互体验台 (拼音音节光标移动 · 句中改错拼音 · 音节候选自动联动)");
        let wc = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 3,
            lpfn_wnd_proc: wnd_proc,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: 0,
            h_cursor,
            hbr_background: CreateSolidBrush(rgb(18, 20, 27)),
            lpsz_menu_name: null(),
            lpsz_class_name: class_name.as_ptr(),
            h_icon_sm: 0,
        };
        RegisterClassExW(&wc);

        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class_name.as_ptr(),
            window_title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            135,
            90,
            WIN_W,
            WIN_H,
            0,
            0,
            h_instance,
            null(),
        );

        disable_window_system_ime(hwnd);

        ShowWindow(hwnd, SW_SHOWNORMAL);
        ShowWindow(hwnd, SW_RESTORE);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
