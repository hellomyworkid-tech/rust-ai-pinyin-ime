#![windows_subsystem = "windows"]
//! `ime-global-service` (`RustAI拼音输入法.exe`):
//! Windows 全局系统级 AI 拼音输入法常驻服务 + 无焦点置顶跟随候选框 + 右下角悬浮状态栏
//!
//! 核心能力：
//! 1. **全局任何软件内直接打字 (`WH_KEYBOARD_LL` + `SendInput(KEYEVENTF_UNICODE)`)**：
//!    支持在记事本、微信、Chrome/Edge 浏览器、VS Code、Cursor、Word、终端等所有 Windows 窗口中直接输入！
//! 2. **无焦点置顶悬浮候选框 (`WS_POPUP | WS_EX_TOPMOST | WS_EX_NOACTIVATE`)**：
//!    鼠标点击悬浮框上的拼音音节、候选词或翻页按钮时，**绝不抢夺当前打字窗口的焦点**，选词后直接上屏到目标窗口。
//! 3. **完整继承标准输入法交互**：
//!    - `←` / `→` 方向键或鼠标点击预编辑栏音节，光标 `|` 在拼音音节上移动，下方候选词自动联动对应音节；
//!    - 句中拼音输错时，移动光标 `|` 按 `Backspace` / `Delete` / `a-z` 原地改错，按空格直接整句上屏；
//!    - `Shift` 键或点击右下角悬浮状态条切换【中/英】模式；支持中文标点自动转换与前台应用进程场景自动识别。

use ime_ai::{
    AiProvider, BuiltinModelTier, DualModeAiOrchestrator, ManagedLocalLlm, OpenAiCompatibleClient,
    MANAGED_LLM_BASE_URL,
};
use ime_core::{AppScene, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use ime_tsf_win::{
    activate_tsf_profile_for_current_user, ensure_appcontainer_dir_acl,
    is_shell_appcontainer_host, is_tsf_com_registered, CandidateWindowViewModel,
    ImePersistentConfig, KeyAction,
    TsfIpcRequest, TsfIpcResponse, TsfKeySession, VirtualKey, TSF_IPC_PORT,
};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static DOWNLOAD_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static TSF_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 「单击 Shift 切换中英文」判定（由全局低级键盘/鼠标钩子统一裁决）
//
// 只有满足以下全部条件才算一次有效的单击 Shift：
//   1. 按下 Shift 时没有按住 Ctrl / Alt / Win 或任何鼠标键；
//   2. Shift 按住期间没有任何其他按键（含被系统热键吞掉的 Win+Shift+S、Ctrl+Shift、Alt+Shift 等）
//      以及鼠标点击 / 滚轮（Shift+点击选择文本、Shift+滚轮横向滚动）；
//   3. 从按下到松开不超过 SHIFT_TAP_MAX_MS（长按 Shift 后松开不算）。
// 低级钩子能看到所有物理输入（包括被系统热键截获、从未送达应用 TSF 的按键），因此比应用内判断可靠。
// ---------------------------------------------------------------------------
const SHIFT_TAP_MAX_MS: u64 = 500;
static SHIFT_DOWN_AT_MS: AtomicU64 = AtomicU64::new(0);
static SHIFT_TAP_CLEAN: AtomicBool = AtomicBool::new(false);
static SHIFT_TAP_VALID_AT_MS: AtomicU64 = AtomicU64::new(0);
static LAST_SHIFT_TOGGLED_AT_MS: AtomicU64 = AtomicU64::new(0);
static LL_HOOKS_INSTALLED: AtomicBool = AtomicBool::new(false);
/// 状态栏是否因贴靠屏幕边缘而自动收缩为紧凑悬浮小方框
static STATUS_COLLAPSED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// 右下角状态栏的显隐：只有当前前台应用正在使用本输入法时才显示
//
// TSF 在某个进程中激活 / 停用本输入法时，DLL 会上报 activate / deactivate（含进程 ID）。
// 用户用 Win+Space 或语言栏切到微软拼音、英文键盘等其他输入法时，该进程会收到 deactivate，
// 状态栏随即隐藏；切回本输入法时收到 activate，状态栏重新出现。
// ---------------------------------------------------------------------------
const STATUS_VIS_TIMER_ID: usize = 0x5EED;
static TIP_ACTIVE_BY_PID: Mutex<Option<std::collections::HashMap<u32, bool>>> = Mutex::new(None);
static TIP_LAST_ACTIVATE_MS_BY_PID: Mutex<Option<std::collections::HashMap<u32, u64>>> =
    Mutex::new(None);
static LAST_TIP_EVENT_ACTIVE: AtomicBool = AtomicBool::new(true);
static LAST_DEACTIVATED_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// 管理员权限目标进程（如 Taskmgr.exe）因 UIPI 阻止跨进程 SendInput 时，转交进程内 TSF DLL 轮询上屏的队列
static PENDING_ELEVATED_COMMITS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 检查指定 PID 的进程是否以管理员高完整性级别（Elevated / High Integrity）运行
unsafe fn is_process_elevated(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_ELEVATION: u32 = 20;
    let h_proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h_proc == 0 {
        return false;
    }
    let mut h_tok: isize = 0;
    let mut elevated = false;
    if OpenProcessToken(h_proc, TOKEN_QUERY, &mut h_tok) != 0 && h_tok != 0 {
        let mut val: u32 = 0;
        let mut ret_len: u32 = 0;
        if GetTokenInformation(h_tok, TOKEN_ELEVATION, &mut val as *mut u32 as *mut u8, 4, &mut ret_len) != 0 {
            elevated = val != 0;
        }
        CloseHandle(h_tok);
    }
    CloseHandle(h_proc);
    elevated
}

/// 检查指定 PID 的进程是否仍在运行
unsafe fn is_process_still_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h == 0 {
        return false;
    }
    let mut exit_code: u32 = 0;
    let ok = GetExitCodeProcess(h, &mut exit_code) != 0 && exit_code == STILL_ACTIVE;
    CloseHandle(h);
    ok
}

/// 读取 `HKCU\Software\Microsoft\CTF\Assemblies\0x00000804\{34745C63-B2F0-4784-8B67-5E12C8701A31}` 的 `Default` 值，
/// 判断当前系统中文输入法默认激活项是否为本输入法 `{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}`
unsafe fn is_our_tip_default_in_ctf_registry() -> Option<bool> {
    const HKEY_CURRENT_USER: isize = -2147483647isize; // 0x80000001
    const KEY_READ: u32 = 0x20019;
    let sub = to_wide(
        "Software\\Microsoft\\CTF\\Assemblies\\0x00000804\\{34745C63-B2F0-4784-8B67-5E12C8701A31}",
    );
    let mut hk: isize = 0;
    if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut hk) != 0 || hk == 0 {
        return None;
    }
    let val_name = to_wide("Default");
    let mut buf = [0u16; 64];
    let mut cb: u32 = (buf.len() * 2) as u32;
    let mut reg_type: u32 = 0;
    let rc = RegQueryValueExW(
        hk,
        val_name.as_ptr(),
        std::ptr::null_mut(),
        &mut reg_type,
        buf.as_mut_ptr() as *mut u8,
        &mut cb,
    );
    RegCloseKey(hk);
    if rc != 0 || cb < 4 {
        return None;
    }
    let len = (cb as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..len])
        .trim_end_matches('\0')
        .trim()
        .to_ascii_uppercase();
    if s.is_empty() {
        return None;
    }
    Some(s == "{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}")
}

fn note_tip_state(pid: u32, active: bool) {
    let now = mono_ms();
    if active {
        LAST_TIP_EVENT_ACTIVE.store(true, Ordering::Relaxed);
        LAST_DEACTIVATED_PID.store(0, Ordering::Relaxed);
        if pid == 0 {
            return;
        }
        if let Ok(mut g) = TIP_LAST_ACTIVATE_MS_BY_PID.lock() {
            let m = g.get_or_insert_with(std::collections::HashMap::new);
            if m.len() > 512 {
                m.clear();
            }
            m.insert(pid, now);
        }
        if let Ok(mut g) = TIP_ACTIVE_BY_PID.lock() {
            let map = g.get_or_insert_with(std::collections::HashMap::new);
            if map.len() > 512 {
                map.clear();
            }
            // 一旦确认本输入法处于激活状态，清理其他进程可能残留的 false 记录（避免切回 Edge 等窗口时误隐藏状态栏）
            map.retain(|_, &mut v| v);
            map.insert(pid, true);
        }
    } else {
        if pid != 0 {
            // 防抖：过滤旧版 DLL 在 Activate 探测接口瞬间释放临时 COM 对象产生的假 deactivate
            let recent_act = TIP_LAST_ACTIVATE_MS_BY_PID
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|m| m.get(&pid).copied()))
                .unwrap_or(0);
            if recent_act != 0 && now.saturating_sub(recent_act) <= 250 {
                return;
            }
        }
        let fg = unsafe { GetForegroundWindow() };
        let mut fg_pid: u32 = 0;
        let fg_visible = if fg != 0 {
            unsafe {
                GetWindowThreadProcessId(fg, &mut fg_pid);
                IsWindowVisible(fg) != 0
            }
        } else {
            false
        };
        // 若发起 deactivate 的并非当前可见前台窗口（例如后台进程退出、正在关闭的窗口或弹窗销毁），
        // 仅移除该进程的进程内 TSF 标记，绝不将全局输入法状态置为停用
        if pid != 0 && (!fg_visible || fg_pid == 0 || pid != fg_pid) {
            if let Ok(mut g) = TIP_ACTIVE_BY_PID.lock() {
                if let Some(map) = g.as_mut() {
                    map.remove(&pid);
                }
            }
            return;
        }
        LAST_TIP_EVENT_ACTIVE.store(false, Ordering::Relaxed);
        LAST_DEACTIVATED_PID.store(pid, Ordering::Relaxed);
        if pid != 0 {
            if let Ok(mut g) = TIP_ACTIVE_BY_PID.lock() {
                let map = g.get_or_insert_with(std::collections::HashMap::new);
                if map.len() > 512 {
                    map.clear();
                }
                map.insert(pid, false);
            }
        }
    }
}

/// 计算当前是否应显示右下角状态栏
unsafe fn status_bar_should_show() -> bool {
    if !TSF_MODE_ACTIVE.load(Ordering::Relaxed) {
        return true; // 全局钩子模式下输入法对所有窗口生效
    }
    // 若最近一次触发 deactivate 的进程已经退出（说明是关闭软件而非切换输入法），自动恢复激活状态
    if !LAST_TIP_EVENT_ACTIVE.load(Ordering::Relaxed) {
        let deact_pid = LAST_DEACTIVATED_PID.load(Ordering::Relaxed);
        if deact_pid != 0 && !is_process_still_alive(deact_pid) {
            LAST_DEACTIVATED_PID.store(0, Ordering::Relaxed);
            LAST_TIP_EVENT_ACTIVE.store(true, Ordering::Relaxed);
            if let Ok(mut g) = TIP_ACTIVE_BY_PID.lock() {
                if let Some(map) = g.as_mut() {
                    map.remove(&deact_pid);
                }
            }
        }
    }
    let fg = GetForegroundWindow();
    if fg == 0 {
        return LAST_TIP_EVENT_ACTIVE.load(Ordering::Relaxed);
    }
    let mut pid: u32 = 0;
    let fg_tid = GetWindowThreadProcessId(fg, &mut pid);
    if pid == std::process::id() {
        return true; // 本输入法自己的设置窗口
    }
    // 若当前前台线程的键盘布局语言已切换为非中文（如 0x0409 英文键盘），立即隐藏状态栏
    if fg_tid != 0 {
        let hkl = GetKeyboardLayout(fg_tid);
        let lang_id = (hkl as usize) & 0xFFFF;
        if lang_id != 0 && lang_id != 0x0804 {
            return false;
        }
    }
    if let Some(active) = TIP_ACTIVE_BY_PID
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(&pid).copied()))
    {
        return active;
    }
    if let Some(false) = is_our_tip_default_in_ctf_registry() {
        return false;
    }
    LAST_TIP_EVENT_ACTIVE.load(Ordering::Relaxed)
}

unsafe fn update_status_bar_visibility(hwnd_status: isize) {
    let want = status_bar_should_show();
    let visible = IsWindowVisible(hwnd_status) != 0;
    if want != visible {
        ShowWindow(hwnd_status, if want { SW_SHOWNOACTIVATE } else { SW_HIDE });
    }
    if !want {
        // 已切换到其他输入法：同时收起可能残留的候选窗
        let hwnd_cand = GLOBAL_STATE
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|st| st.hwnd_cand))
            .unwrap_or(0);
        if hwnd_cand != 0 && IsWindowVisible(hwnd_cand) != 0 {
            ShowWindow(hwnd_cand, SW_HIDE);
        }
    }
}

fn mono_ms() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    BASE.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
}

/// 由低级键盘钩子在每个物理按键事件上调用，更新 Shift 单击判定状态
unsafe fn track_shift_tap(vk: u32, is_down: bool) {
    let is_shift = matches!(vk, 0x10 | 0xA0 | 0xA1);
    if is_shift {
        if is_down {
            // 忽略按住不放时的自动重复按下事件
            if SHIFT_DOWN_AT_MS.load(Ordering::Relaxed) == 0 {
                let held = |k: i32| (GetAsyncKeyState(k) as u16 & 0x8000) != 0;
                let other_mod = held(0x11) || held(0x12) || held(0x5B) || held(0x5C);
                let mouse_btn = held(0x01) || held(0x02) || held(0x04);
                SHIFT_DOWN_AT_MS.store(mono_ms(), Ordering::Relaxed);
                SHIFT_TAP_CLEAN.store(!other_mod && !mouse_btn, Ordering::Relaxed);
                SHIFT_TAP_VALID_AT_MS.store(0, Ordering::Relaxed);
            }
        } else {
            let down_at = SHIFT_DOWN_AT_MS.swap(0, Ordering::Relaxed);
            let now = mono_ms();
            let valid = down_at != 0
                && SHIFT_TAP_CLEAN.swap(false, Ordering::Relaxed)
                && now.saturating_sub(down_at) <= SHIFT_TAP_MAX_MS;
            SHIFT_TAP_VALID_AT_MS.store(if valid { now } else { 0 }, Ordering::Relaxed);
        }
    } else if is_down {
        SHIFT_TAP_CLEAN.store(false, Ordering::Relaxed);
    }
}

/// 取走（消费）最近一次 Shift 松开的判定结果。若低级钩子未能安装，则退回到调用方自身的判断。
fn take_valid_shift_tap() -> bool {
    if !LL_HOOKS_INSTALLED.load(Ordering::Relaxed) {
        return true;
    }
    let at = SHIFT_TAP_VALID_AT_MS.swap(0, Ordering::Relaxed);
    at != 0 && mono_ms().saturating_sub(at) <= 800
}

/// 全局低级鼠标钩子：Shift 按住期间出现任何点击或滚轮，都说明是组合操作，取消本次 Shift 单击
unsafe extern "system" fn low_level_mouse_proc(n_code: i32, w_param: usize, l_param: isize) -> isize {
    if n_code >= 0
        && matches!(w_param as u32, 0x0201 | 0x0204 | 0x0207 | 0x020A | 0x020B | 0x020E)
        && SHIFT_DOWN_AT_MS.load(Ordering::Relaxed) != 0
    {
        SHIFT_TAP_CLEAN.store(false, Ordering::Relaxed);
    }
    CallNextHookEx(0, n_code, w_param, l_param)
}

#[repr(C)]
struct ICONINFO {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
}

#[repr(C)]
struct BITMAPINFOHEADER {
    bi_size: u32,
    bi_width: i32,
    bi_height: i32,
    bi_planes: u16,
    bi_bit_count: u16,
    bi_compression: u32,
    bi_size_image: u32,
    bi_x_pels_per_meter: i32,
    bi_y_pels_per_meter: i32,
    bi_clr_used: u32,
    bi_clr_important: u32,
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

#[repr(C)]
struct KBDLLHOOKSTRUCT {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

#[repr(C)]
struct INPUT {
    input_type: u32,
    _pad0: u32,
    w_vk: u16,
    w_scan: u16,
    dw_flags: u32,
    time: u32,
    _pad1: u32,
    dw_extra_info: usize,
    _pad_union: u64,
}
const _: () = assert!(std::mem::size_of::<INPUT>() == 40);

#[repr(C)]
struct GUITHREADINFO {
    cb_size: u32,
    flags: u32,
    hwnd_active: isize,
    hwnd_focus: isize,
    hwnd_capture: isize,
    hwnd_menu_owner: isize,
    hwnd_move_size: isize,
    hwnd_caret: isize,
    rc_caret: RECT,
}
const _: () = assert!(std::mem::size_of::<GUITHREADINFO>() == 72);

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
    fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> isize;
    fn QueryFullProcessImageNameW(
        h_process: isize,
        dw_flags: u32,
        lp_exe_name: *mut u16,
        lpdw_size: *mut u32,
    ) -> i32;
    fn CloseHandle(h_object: isize) -> i32;
    fn CreateMutexW(
        lp_mutex_attributes: *const u8,
        b_initial_owner: i32,
        lp_name: *const u16,
    ) -> isize;
    fn GetLastError() -> u32;
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
    fn SetWindowPos(
        h_wnd: isize,
        h_wnd_insert_after: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        u_flags: u32,
    ) -> i32;
    fn LoadCursorW(h_instance: isize, lp_cursor_name: *const u16) -> isize;
    fn LoadImageW(
        h_inst: isize,
        name: *const u16,
        image_type: u32,
        cx: i32,
        cy: i32,
        fu_load: u32,
    ) -> isize;
    fn SetCursor(h_cursor: isize) -> isize;
    fn GetMessageW(
        lp_msg: *mut MSG,
        h_wnd: isize,
        w_msg_filter_min: u32,
        w_msg_filter_max: u32,
    ) -> i32;
    fn TranslateMessage(lp_msg: *const MSG) -> i32;
    fn DispatchMessageW(lp_msg: *const MSG) -> isize;
    fn DefWindowProcW(h_wnd: isize, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn PostQuitMessage(n_exit_code: i32);
    fn BeginPaint(h_wnd: isize, lp_paint: *mut PAINTSTRUCT) -> isize;
    fn EndPaint(h_wnd: isize, lp_paint: *const PAINTSTRUCT) -> i32;
    fn InvalidateRect(h_wnd: isize, lp_rect: *const RECT, b_erase: i32) -> i32;
    fn FillRect(h_dc: isize, lprc: *const RECT, hbr: isize) -> i32;
    fn GetAsyncKeyState(v_key: i32) -> i16;
    fn GetKeyState(n_virt_key: i32) -> i16;
    fn SetWindowsHookExW(
        id_hook: i32,
        lpfn: unsafe extern "system" fn(i32, usize, isize) -> isize,
        hmod: isize,
        dw_thread_id: u32,
    ) -> isize;
    fn UnhookWindowsHookEx(hhk: isize) -> i32;
    fn CallNextHookEx(hhk: isize, n_code: i32, w_param: usize, l_param: isize) -> isize;
    fn SendInput(c_inputs: u32, p_inputs: *const INPUT, cb_size: i32) -> u32;
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(h_wnd: isize, lpdw_process_id: *mut u32) -> u32;
    fn GetGUIThreadInfo(id_thread: u32, pgui: *mut GUITHREADINFO) -> i32;
    fn GetWindowLongW(hwnd: isize, n_index: i32) -> i32;
    fn GetWindowRect(h_wnd: isize, lp_rect: *mut RECT) -> i32;
    fn IsWindowVisible(h_wnd: isize) -> i32;
    fn SetTimer(h_wnd: isize, id_event: usize, elapse_ms: u32, timer_func: *const u8) -> usize;
    fn ClientToScreen(h_wnd: isize, lp_point: *mut POINT) -> i32;
    fn GetCursorPos(lp_point: *mut POINT) -> i32;
    fn GetSystemMetrics(n_index: i32) -> i32;
    fn OpenDesktopW(
        lpsz_desktop: *const u16,
        dw_flags: u32,
        f_inherit: i32,
        dw_desired_access: u32,
    ) -> isize;
    fn SetThreadDesktop(h_desktop: isize) -> i32;
    fn PostMessageW(h_wnd: isize, msg: u32, w_param: usize, l_param: isize) -> i32;
    fn SendMessageW(h_wnd: isize, msg: u32, w_param: usize, l_param: isize) -> isize;
    fn SetWindowTextW(h_wnd: isize, lp_string: *const u16) -> i32;
    fn GetWindowTextW(h_wnd: isize, lp_string: *mut u16, n_max_count: i32) -> i32;
    fn GetWindowTextLengthW(h_wnd: isize) -> i32;
    fn SetForegroundWindow(h_wnd: isize) -> i32;
    fn BringWindowToTop(h_wnd: isize) -> i32;
    fn CreateIconIndirect(piconinfo: *const ICONINFO) -> isize;
    fn FindWindowW(lp_class_name: *const u16, lp_window_name: *const u16) -> isize;
    fn SystemParametersInfoW(
        ui_action: u32,
        ui_param: u32,
        pv_param: *mut RECT,
        f_win_ini: u32,
    ) -> i32;
    fn SetCapture(h_wnd: isize) -> isize;
    fn ReleaseCapture() -> i32;
    fn KillTimer(h_wnd: isize, id_event: usize) -> i32;
    fn GetKeyboardLayout(id_thread: u32) -> isize;
}

#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> isize;
    fn GetProcAddress(h_module: isize, lp_proc_name: *const u8) -> *const usize;
    fn GetExitCodeProcess(h_process: isize, lp_exit_code: *mut u32) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        h_key: isize,
        lp_sub_key: *const u16,
        ul_options: u32,
        sam_desired: u32,
        phk_result: *mut isize,
    ) -> i32;
    fn RegQueryValueExW(
        h_key: isize,
        lp_value_name: *const u16,
        lp_reserved: *mut u32,
        lp_type: *mut u32,
        lp_data: *mut u8,
        lpcb_data: *mut u32,
    ) -> i32;
    fn RegCloseKey(h_key: isize) -> i32;
    fn OpenProcessToken(process_handle: isize, desired_access: u32, token_handle: *mut isize) -> i32;
    fn GetTokenInformation(
        token_handle: isize,
        token_information_class: u32,
        token_information: *mut u8,
        token_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreateSolidBrush(color: u32) -> isize;
    fn DeleteObject(ho: isize) -> i32;
    fn SetBkMode(hdc: isize, mode: i32) -> i32;
    fn SetTextColor(hdc: isize, color: u32) -> u32;
    #[allow(dead_code)]
    fn TextOutW(hdc: isize, x: i32, y: i32, lp_string: *const u16, c: i32) -> i32;
    fn GetTextExtentPoint32W(
        hdc: isize,
        lp_string: *const u16,
        c: i32,
        psizl: *mut POINT,
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
    fn BitBlt(
        hdc: isize,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        hdc_src: isize,
        x1: i32,
        y1: i32,
        rop: u32,
    ) -> i32;
    fn CreateDIBSection(
        hdc: isize,
        pbmi: *const BITMAPINFOHEADER,
        usage: u32,
        ppv_bits: *mut *mut u32,
        h_section: isize,
        offset: u32,
    ) -> isize;
    fn CreateBitmap(
        n_width: i32,
        n_height: i32,
        n_planes: u32,
        n_bit_count: u32,
        lp_bits: *const u8,
    ) -> isize;
    fn GdiFlush() -> i32;
}

const WH_KEYBOARD_LL: i32 = 13;
const LLKHF_INJECTED: u32 = 0x00000010;
const WM_CREATE: u32 = 0x0001;
const WM_DESTROY: u32 = 0x0002;
const WM_CLOSE: u32 = 0x0010;
const WM_PAINT: u32 = 0x000F;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const MA_NOACTIVATE: isize = 3;
const WM_SETFONT: u32 = 0x0030;
const WM_SETICON: u32 = 0x0080;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_COMMAND: u32 = 0x0111;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_APP_AI_READY: u32 = 0x8000 + 42;
const WM_APP_TSF_SYNC: u32 = 0x8000 + 43;
const WM_APP_OPEN_SETTINGS: u32 = 0x8000 + 45;

const WS_POPUP: u32 = 0x80000000;
const WS_CHILD: u32 = 0x40000000;
const WS_VISIBLE: u32 = 0x10000000;
const WS_CAPTION: u32 = 0x00C00000;
const WS_SYSMENU: u32 = 0x00080000;
const WS_TABSTOP: u32 = 0x00010000;
const WS_EX_TOPMOST: u32 = 0x00000008;
const WS_EX_TOOLWINDOW: u32 = 0x00000080;
const WS_EX_NOACTIVATE: u32 = 0x08000000;
const WS_EX_APPWINDOW: u32 = 0x00040000;

const ES_AUTOHSCROLL: u32 = 0x0080;

const SW_HIDE: i32 = 0;
const SW_SHOW: i32 = 5;
const SW_SHOWNOACTIVATE: i32 = 4;
const HWND_TOPMOST: isize = -1;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOACTIVATE: u32 = 0x0010;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const TRANSPARENT: i32 = 1;
const SRCCOPY: u32 = 0x00CC0020;

/// 候选框标准高度（拼音行 + 整句行 + 横排候选行）；宽度随内容自适应，见 [`CAND_CUR_W`]
const CAND_H: i32 = 104;
/// 候选框的最小 / 默认宽度（实际宽度由 [`compute_cand_layout`] 根据内容计算）
const CAND_W: i32 = 420;
const CAND_GHOST_H: i32 = 44;
const STATUS_W_TSF: i32 = 160;
const STATUS_W_HOOK: i32 = 192;
const STATUS_H: i32 = 34;
const STATUS_ORB_SIZE: i32 = 34;
/// 状态栏贴边后延时自动收缩的定时器
const STATUS_AUTOHIDE_TIMER_ID: usize = 0x5EEF;
const STATUS_AUTOHIDE_DELAY_MS: u32 = 2500;

static CAND_CUR_W: AtomicI32 = AtomicI32::new(CAND_W);
static CAND_CUR_H: AtomicI32 = AtomicI32::new(CAND_H);
/// 收缩为小方块时停靠的边：0 = 无，1 = 左，2 = 右，3 = 上
static STATUS_DOCK_SIDE: AtomicI32 = AtomicI32::new(0);
static STATUS_DRAG_START_X: AtomicI32 = AtomicI32::new(0);
static STATUS_DRAG_START_Y: AtomicI32 = AtomicI32::new(0);
/// DWM 原生圆角是否生效（Win11）；未生效时（Win10）由程序自绘 1px 边框
static DWM_ROUND_OK: AtomicBool = AtomicBool::new(false);

#[inline]
fn cand_cur_size() -> (i32, i32) {
    (
        CAND_CUR_W.load(Ordering::Relaxed),
        CAND_CUR_H.load(Ordering::Relaxed),
    )
}

#[inline]
fn current_status_width() -> i32 {
    if TSF_MODE_ACTIVE.load(Ordering::Relaxed) {
        STATUS_W_TSF
    } else {
        STATUS_W_HOOK
    }
}

#[inline]
fn current_status_size() -> (i32, i32) {
    if STATUS_COLLAPSED.load(Ordering::Relaxed) {
        (STATUS_ORB_SIZE, STATUS_ORB_SIZE)
    } else {
        (current_status_width(), STATUS_H)
    }
}

// ---------------------------------------------------------------------------
// 候选框 / 状态栏共享字体与配色（Win11 微软拼音风格：纯白底、深灰字、单一蓝色强调）
// ---------------------------------------------------------------------------
const UI_BG: u32 = rgb(255, 255, 255);
const UI_TEXT: u32 = rgb(26, 26, 26);
const UI_TEXT2: u32 = rgb(96, 96, 96);
const UI_MUTED: u32 = rgb(150, 150, 150);
const UI_DISABLED: u32 = rgb(205, 205, 205);
const UI_ACCENT: u32 = rgb(0, 103, 192);
const UI_ACCENT_BG: u32 = rgb(230, 240, 251);
const UI_HOVER: u32 = rgb(243, 243, 243);
const UI_SELECTED: u32 = rgb(237, 241, 247);
const UI_DIVIDER: u32 = rgb(236, 236, 236);
const UI_BORDER: u32 = rgb(214, 214, 214);
const UI_PINNED: u32 = rgb(16, 124, 65);
const UI_PINNED_BG: u32 = rgb(231, 245, 236);

#[derive(Clone, Copy)]
struct UiFonts {
    pinyin: isize,
    small: isize,
    small_b: isize,
    num: isize,
    cand: isize,
    icon: isize,
    arrow: isize,
    status: isize,
    status_b: isize,
}

static UI_FONTS: std::sync::OnceLock<UiFonts> = std::sync::OnceLock::new();

fn ui_fonts() -> UiFonts {
    *UI_FONTS.get_or_init(|| unsafe {
        let ui = to_wide("Microsoft YaHei UI");
        let icon = to_wide("Segoe MDL2 Assets");
        let sym = to_wide("Segoe UI Symbol");
        let mk = |h: i32, w: i32, face: &Vec<u16>| {
            CreateFontW(h, 0, 0, 0, w, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr())
        };
        UiFonts {
            pinyin: mk(-15, 400, &ui),
            small: mk(-12, 400, &ui),
            small_b: mk(-12, 700, &ui),
            num: mk(-13, 400, &ui),
            cand: mk(-17, 400, &ui),
            icon: mk(-12, 400, &icon),
            arrow: mk(-10, 400, &sym),
            status: mk(-14, 400, &ui),
            status_b: mk(-16, 700, &ui),
        }
    })
}

/// 用临时内存 DC 测量文本像素宽度（与绘制使用同一字体，保证热区与绘制严格一致）
struct TextMeasurer {
    dc: isize,
    old: isize,
}

impl TextMeasurer {
    unsafe fn new() -> Self {
        let dc = CreateCompatibleDC(0);
        let old = SelectObject(dc, ui_fonts().pinyin);
        Self { dc, old }
    }
    unsafe fn width(&self, font: isize, s: &str) -> i32 {
        SelectObject(self.dc, font);
        measure_text_width(self.dc, s)
    }
}

impl Drop for TextMeasurer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteDC(self.dc);
        }
    }
}

/// 获取排除任务栏后的桌面工作区矩形 (`SPI_GETWORKAREA`)
unsafe fn get_work_area() -> RECT {
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if SystemParametersInfoW(0x0030 /* SPI_GETWORKAREA */, 0, &mut rc, 0) != 0
        && rc.right > rc.left + 200
        && rc.bottom > rc.top + 200
    {
        rc
    } else {
        let sw = GetSystemMetrics(SM_CXSCREEN).max(1024);
        let sh = GetSystemMetrics(SM_CYSCREEN).max(768);
        RECT {
            left: 0,
            top: 0,
            right: sw,
            bottom: sh - 48,
        }
    }
}

/// 将窗口坐标限制在排除任务栏后的桌面工作区内（绝不压到任务栏或拖出屏幕边框）
unsafe fn clamp_to_work_area(x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
    let wa = get_work_area();
    let min_x = wa.left + 2;
    let max_x = (wa.right - w - 2).max(min_x);
    let min_y = wa.top + 2;
    let max_y = (wa.bottom - h - 2).max(min_y);
    (x.clamp(min_x, max_x), y.clamp(min_y, max_y))
}

/// 启用 Windows 11 原生硬件抗锯齿圆角与立体投影
unsafe fn enable_win11_round_corners(hwnd: isize) {
    if hwnd != 0 {
        let dll = to_wide("dwmapi.dll");
        let h_mod = LoadLibraryW(dll.as_ptr());
        if h_mod != 0 {
            let p = GetProcAddress(h_mod, b"DwmSetWindowAttribute\0".as_ptr());
            if !p.is_null() {
                type DwmSetAttrFn = unsafe extern "system" fn(isize, u32, *const u32, u32) -> i32;
                let func: DwmSetAttrFn = std::mem::transmute(p);
                let pref: u32 = 2; // DWMWCP_ROUND
                let hr = func(
                    hwnd,
                    33, /* DWMWA_WINDOW_CORNER_PREFERENCE */
                    &pref,
                    std::mem::size_of::<u32>() as u32,
                );
                if hr == 0 {
                    DWM_ROUND_OK.store(true, Ordering::Relaxed);
                    // 细浅灰描边，与 Win11 系统弹出层一致
                    let border: u32 = UI_BORDER;
                    let _ = func(
                        hwnd,
                        34, /* DWMWA_BORDER_COLOR */
                        &border,
                        std::mem::size_of::<u32>() as u32,
                    );
                }
            }
        }
    }
}

const ID_CHK_AI_ENABLED: isize = 1001;
const ID_EDT_API_BASE: isize = 1003;
const ID_EDT_MODEL: isize = 1004;
const ID_EDT_API_KEY: isize = 1005;
const ID_BTN_PROBE_LOCAL: isize = 1006;
const ID_BTN_TEST_CONN: isize = 1007;
const ID_BTN_PRESET_QWEN15B: isize = 1010;
const ID_BTN_PRESET_QWEN05B: isize = 1011;
const ID_BTN_PRESET_DEEPSEEK: isize = 1012;
const ID_BTN_PRESET_SILICON: isize = 1013;
const ID_CHK_Z_ZH: isize = 1020;
const ID_CHK_AN_ANG: isize = 1021;
const ID_CHK_TYPO: isize = 1022;
const ID_CHK_N_L: isize = 1023;
const ID_CHK_F_H: isize = 1024;
const ID_LBL_STATUS: isize = 1030;
const ID_BTN_CLEAR_MEM: isize = 1040;
const ID_BTN_SAVE: isize = 1041;
const ID_BTN_CLOSE: isize = 1042;
const ID_BTN_REG_TSF: isize = 1050;
const ID_BTN_UNREG_TSF: isize = 1051;

#[derive(Default, Clone, Copy)]
struct SettingsControls {
    chk_ai_enabled: isize,
    seg_provider: [isize; 5],
    edt_api_base: isize,
    edt_model: isize,
    edt_api_key: isize,
    chk_z_zh: isize,
    chk_an_ang: isize,
    chk_typo: isize,
    chk_n_l: isize,
    chk_f_h: isize,
    lbl_status: isize,
    btn_reg_tsf: isize,
    btn_unreg_tsf: isize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandHitZone {
    None,
    DragHeader,
    PinyinSyllable(usize),
    PinyinEndZone,
    PrevSyllableBtn,
    NextSyllableBtn,
    PrevPageBtn,
    NextPageBtn,
    CommitFullSentenceBtn,
    ClearPinyinBtn,
    CandidateRow(usize),
    AcceptGhostBtn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusHitZone {
    None,
    DragArea,
    ToggleLangBtn,
    TogglePunctBtn,
    OpenSettingsBtn,
    ExitBtn,
}

#[derive(Clone, Copy)]
struct CandHotspot {
    rect: RECT,
    zone: CandHitZone,
}

struct GlobalImeState {
    session: TsfKeySession,
    persistent_cfg: ImePersistentConfig,
    chinese_punct_mode: bool,
    shift_tap_armed: bool,
    tsf_mode_active: bool,
    eaten_vks: [bool; 256],
    hwnd_cand: isize,
    hwnd_status: isize,
    hwnd_settings: isize,
    settings_ctrls: SettingsControls,
    cand_hover: CandHitZone,
    status_hover: StatusHitZone,
    cand_pos: (i32, i32),
    status_pos: (i32, i32),
    dragging_status: Option<(i32, i32)>,
    status_drag_moved: bool,
    dragging_cand: Option<(i32, i32)>,
    last_fg_exe: String,
    pending_tsf_commits: Vec<String>,
}

static GLOBAL_STATE: Mutex<Option<GlobalImeState>> = Mutex::new(None);

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

#[inline]
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

#[allow(dead_code)]
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
    let mut sz = POINT { x: 0, y: 0 };
    if GetTextExtentPoint32W(hdc, wide.as_ptr(), wide.len() as i32, &mut sz) != 0 {
        sz.x
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

#[allow(dead_code)]
fn truncate_left_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let tail: String = chars[chars.len() - max_chars..].iter().collect();
        format!("...{}", tail)
    }
}

/// 将选定文本通过 `SendInput(KEYEVENTF_UNICODE)` 直接注入当前活动窗口（兼容记事本、微信、浏览器、VS Code 等所有应用）
unsafe fn send_unicode_text(text: &str) {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    if utf16.is_empty() {
        return;
    }
    let mut inputs = Vec::with_capacity(utf16.len() * 2);
    for &unit in &utf16 {
        inputs.push(INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0,
            w_scan: unit,
            dw_flags: 0x0004, // KEYEVENTF_UNICODE
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        });
        inputs.push(INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0,
            w_scan: unit,
            dw_flags: 0x0004 | 0x0002, // KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        });
    }
    let sent = SendInput(
        inputs.len() as u32,
        inputs.as_ptr(),
        std::mem::size_of::<INPUT>() as i32,
    );
    // 若当前前台窗口为管理员高权限窗口（如 Taskmgr.exe），跨进程 SendInput 会被 Windows UIPI 拦截并返回 0，
    // 此时将待上屏文本压入高权限回传队列，由目标进程内的 ime_tsf_win.dll 以自身高权限执行上屏！
    if sent == 0 {
        if let Ok(mut q) = PENDING_ELEVATED_COMMITS.lock() {
            q.push(text.to_string());
        }
    }
}

/// 获取当前焦点窗口的可执行文件名（如 `notepad.exe`, `Code.exe`, `WeChat.exe`）
unsafe fn get_foreground_exe_name() -> String {
    let hwnd = GetForegroundWindow();
    if hwnd == 0 {
        return String::new();
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return String::new();
    }
    let h_proc = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
    if h_proc == 0 {
        return String::new();
    }
    let mut buf = [0u16; 512];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(h_proc, 0, buf.as_mut_ptr(), &mut size);
    CloseHandle(h_proc);
    if ok == 0 || size == 0 {
        return String::new();
    }
    let full_path = String::from_utf16_lossy(&buf[..size as usize]);
    full_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_string()
}

/// 当前焦点是否是 Win32 密码输入框（`ES_PASSWORD`）。是则输入法完全放行，不拦截、不记忆。
/// 注：浏览器/Electron 等自绘控件无法通过此方式识别。
unsafe fn focus_is_password_field() -> bool {
    let hwnd_fg = GetForegroundWindow();
    if hwnd_fg == 0 {
        return false;
    }
    let mut pid: u32 = 0;
    let tid = GetWindowThreadProcessId(hwnd_fg, &mut pid);
    if tid == 0 {
        return false;
    }
    let mut gti: GUITHREADINFO = std::mem::zeroed();
    gti.cb_size = std::mem::size_of::<GUITHREADINFO>() as u32;
    if GetGUIThreadInfo(tid, &mut gti) == 0 || gti.hwnd_focus == 0 {
        return false;
    }
    const GWL_STYLE: i32 = -16;
    const ES_PASSWORD: i32 = 0x0020;
    (GetWindowLongW(gti.hwnd_focus, GWL_STYLE) & ES_PASSWORD) != 0
}

/// 当用户在 Win11 开始菜单 (`StartMenuExperienceHost.exe`) 未点击搜索框就直接敲击字母键时，
/// 注入 `Win + S` 使系统立即无缝切换到 `SearchHost.exe` 搜索输入框，同时保留首字母在拼音预编辑串中
unsafe fn activate_windows_search_box_from_start_menu() {
    let inputs = [
        INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0x5B, // VK_LWIN
            w_scan: 0,
            dw_flags: 0,
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        },
        INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0x53, // 'S'
            w_scan: 0,
            dw_flags: 0,
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        },
        INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0x53, // 'S'
            w_scan: 0,
            dw_flags: 0x0002, // KEYEVENTF_KEYUP
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        },
        INPUT {
            input_type: 1,
            _pad0: 0,
            w_vk: 0x5B, // VK_LWIN
            w_scan: 0,
            dw_flags: 0x0002, // KEYEVENTF_KEYUP
            time: 0,
            _pad1: 0,
            dw_extra_info: 0,
            _pad_union: 0,
        },
    ];
    SendInput(
        inputs.len() as u32,
        inputs.as_ptr(),
        std::mem::size_of::<INPUT>() as i32,
    );
}

/// 探测当前焦点窗口的输入插入符坐标（若目标应用未暴露 Win32 Caret 则回退至鼠标附近安全位置）
unsafe fn resolve_caret_screen_pos(default_pos: (i32, i32)) -> (i32, i32) {
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1024);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(768);

    let hwnd_fg = GetForegroundWindow();
    let fg_exe = get_foreground_exe_name();
    if hwnd_fg != 0 && is_shell_appcontainer_host(&fg_exe) {
        // Win11 搜索 (`SearchHost.exe`) 与开始菜单 (`StartMenuExperienceHost.exe`) 位于 DWM Immersive 层级，
        // 需将悬浮候选框定位在其毛玻璃面板外侧紧邻处（右侧或上方），防止被 Immersive 面板遮挡
        let mut fg_rc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let mut m_pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut m_pt);
        if GetWindowRect(hwnd_fg, &mut fg_rc) != 0 {
            let fg_w = fg_rc.right - fg_rc.left;
            let fg_h = fg_rc.bottom - fg_rc.top;
            if fg_w > 100 && fg_h > 100 && (fg_w < screen_w - 80 || fg_h < screen_h - 80) {
                if m_pt.y > 0
                    && m_pt.y < (fg_rc.top + fg_rc.bottom) / 2
                    && fg_rc.top >= CAND_H + 14
                {
                    let x = fg_rc.left.clamp(20, (screen_w - CAND_W - 20).max(20));
                    let y = (fg_rc.top - CAND_H - 8).max(12);
                    return clamp_to_work_area(x, y, CAND_W, CAND_H);
                }
                if fg_rc.right + CAND_W + 16 <= screen_w {
                    let x = fg_rc.right + 10;
                    let y = (fg_rc.bottom - CAND_H - 12).clamp(20, (screen_h - CAND_H - 60).max(20));
                    return clamp_to_work_area(x, y, CAND_W, CAND_H);
                }
                if fg_rc.top >= CAND_H + 14 {
                    let x = fg_rc.left.clamp(20, (screen_w - CAND_W - 20).max(20));
                    let y = (fg_rc.top - CAND_H - 8).max(12);
                    return clamp_to_work_area(x, y, CAND_W, CAND_H);
                }
            }
        }
        let x = ((screen_w + 780) / 2 + 12).clamp(20, (screen_w - CAND_W - 20).max(20));
        let y = (screen_h - CAND_H - 68).max(20);
        return clamp_to_work_area(x, y, CAND_W, CAND_H);
    }

    if hwnd_fg != 0 {
        let mut pid: u32 = 0;
        let tid = GetWindowThreadProcessId(hwnd_fg, &mut pid);
        if tid != 0 {
            let mut gti: GUITHREADINFO = std::mem::zeroed();
            gti.cb_size = std::mem::size_of::<GUITHREADINFO>() as u32;
            if GetGUIThreadInfo(tid, &mut gti) != 0 && gti.hwnd_caret != 0 {
                if gti.rc_caret.right > 0 || gti.rc_caret.bottom > 0 {
                    let mut pt = POINT {
                        x: gti.rc_caret.left,
                        y: gti.rc_caret.bottom,
                    };
                    if ClientToScreen(gti.hwnd_caret, &mut pt) != 0 && pt.x > 5 && pt.y > 5 {
                        return clamp_to_work_area(pt.x, pt.y + 8, CAND_W, CAND_H);
                    }
                }
            }
        }
    }

    let mut m_pt = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut m_pt) != 0 && m_pt.x > 5 && m_pt.y > 5 {
        return clamp_to_work_area(m_pt.x - 60, m_pt.y + 28, CAND_W, CAND_H);
    }

    clamp_to_work_area(default_pos.0, default_pos.1, CAND_W, CAND_H)
}

/// 中文标点映射（当处于中文模式且开启全角标点时生效）
fn map_chinese_punctuation(vk: u32, shift: bool) -> Option<&'static str> {
    match (vk, shift) {
        (0xBC, false) => Some("，"),
        (0xBE, false) => Some("。"),
        (0xBF, true) => Some("？"),
        (0x31, true) => Some("！"),
        (0xBA, false) => Some("；"),
        (0xBA, true) => Some("："),
        (0x39, true) => Some("（"),
        (0x30, true) => Some("）"),
        (0xDB, false) => Some("【"),
        (0xDD, false) => Some("】"),
        (0xDC, false) => Some("、"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 候选框布局（Win11 微软拼音风格横排候选条，宽度随内容自适应）
//
//   第 1 行 (y 6..32)   ：拼音音节（可点击定位光标）……………………… ‹  ›  ✕
//   第 2 行 (y 33..55)  ：整句  我来测试一下（已锁定部分绿色）—— 点击整句上屏
//   ─────────────────────────────────────────────────────────────
//   第 3 行 (y 62..98)  ：▍1 如图   2 入土   3 乳突   4 如   5 入          ◀ ▶
// ---------------------------------------------------------------------------
const C_ROW1_T: i32 = 6;
const C_ROW1_B: i32 = 32;
const C_ROW2_T: i32 = 33;
const C_ROW2_B: i32 = 55;
const C_DIVIDER_Y: i32 = 58;
const C_ROW3_T: i32 = 62;
const C_ROW3_B: i32 = 98;
const C_TOOL_W: i32 = 26;
const C_ARROW_W: i32 = 24;
const C_CAND_TEXT_MAX: i32 = 320;

/// 最近一次计算出的候选框热区（由 `sync_ui_windows` 计算，`WM_PAINT` 与鼠标命中测试共用）
static CAND_LAYOUT: Mutex<Vec<CandHotspot>> = Mutex::new(Vec::new());

fn ghost_label(ghost: &str) -> String {
    format!("✨ AI 续写：{}", ghost)
}

/// 根据当前视图模型计算候选框尺寸与全部可点击热区
unsafe fn compute_cand_layout(
    vm: &CandidateWindowViewModel,
    ghost: Option<&str>,
) -> (i32, i32, Vec<CandHotspot>) {
    let f = ui_fonts();
    let m = TextMeasurer::new();
    let wa = get_work_area();
    let max_w = ((wa.right - wa.left) - 24).clamp(CAND_W, 980);
    let mut spots = Vec::new();
    let mk = |l: i32, t: i32, r: i32, b: i32, zone: CandHitZone| CandHotspot {
        rect: RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        },
        zone,
    };

    if let Some(g) = ghost {
        let tw = m.width(f.cand, &ghost_label(g)).min(max_w - 140);
        let hint_w = m.width(f.small, "Tab 采纳");
        let w = (16 + tw + 24 + hint_w + 16).clamp(260, max_w);
        spots.push(mk(4, 4, w - 4, CAND_GHOST_H - 4, CandHitZone::AcceptGhostBtn));
        return (w, CAND_GHOST_H, spots);
    }

    // 第 1 行：拼音音节
    let tools_w = 3 * C_TOOL_W + 8;
    let mut syl_rects = Vec::new();
    let mut x = 10;
    for (idx, syl) in vm.syllable_spans.iter().enumerate() {
        let w = m.width(f.pinyin, &syl.code_key) + 10;
        if x + w > max_w - tools_w - 40 {
            break;
        }
        syl_rects.push((idx, x, x + w));
        x += w;
    }
    let syl_end = x;
    let row1_need = syl_end + 40 + tools_w;

    // 第 2 行：整句预览
    let label_w = m.width(f.small_b, "整句");
    let mut sx = 8 + 12 + label_w + 10;
    for (part, pinned) in &vm.full_sentence_parts {
        if part.is_empty() {
            continue;
        }
        sx += m.width(f.num, part) + if *pinned { 10 } else { 0 };
    }
    let hint_w = if vm.full_sentence_selected {
        m.width(f.small, "空格上屏") + 18
    } else if vm.active_syllable_index.is_some() {
        m.width(f.small, "↑ 整句") + 18
    } else {
        0
    };
    let row2_need = sx + hint_w + 8 + 8;

    // 第 3 行：横排候选
    let arrows_w = 2 * C_ARROW_W + 6;
    let mut items = Vec::new();
    let mut cx = 8;
    for (idx, cand) in vm.visible_candidates.iter().enumerate() {
        let num_w = m.width(f.num, &format!("{}", idx + 1));
        let fixed = 14 + num_w + 6 + 12;
        let remain = max_w - 8 - arrows_w - 4 - cx - fixed;
        if remain < 36 {
            break;
        }
        let tw = m.width(f.cand, &cand.text).min(C_CAND_TEXT_MAX).min(remain);
        let iw = fixed + tw;
        items.push((idx, cx, cx + iw));
        cx += iw + 2;
    }
    let row3_need = cx + 4 + arrows_w + 8;

    let w = row1_need.max(row2_need).max(row3_need).max(CAND_W).min(max_w);

    for (idx, l, r) in syl_rects {
        spots.push(mk(l, C_ROW1_T, r, C_ROW1_B, CandHitZone::PinyinSyllable(idx)));
    }
    let tx0 = w - 8 - 3 * C_TOOL_W;
    let end_r = (syl_end + 40).min(tx0 - 4);
    if end_r > syl_end {
        spots.push(mk(syl_end, C_ROW1_T, end_r, C_ROW1_B, CandHitZone::PinyinEndZone));
    }
    for (i, zone) in [
        CandHitZone::PrevSyllableBtn,
        CandHitZone::NextSyllableBtn,
        CandHitZone::ClearPinyinBtn,
    ]
    .into_iter()
    .enumerate()
    {
        let l = tx0 + i as i32 * C_TOOL_W;
        spots.push(mk(l, C_ROW1_T + 1, l + C_TOOL_W, C_ROW1_B - 1, zone));
    }
    if !vm.full_sentence_parts.is_empty() {
        spots.push(mk(
            8,
            C_ROW2_T,
            w - 8,
            C_ROW2_B,
            CandHitZone::CommitFullSentenceBtn,
        ));
    }
    for (idx, l, r) in items {
        spots.push(mk(l, C_ROW3_T, r, C_ROW3_B, CandHitZone::CandidateRow(idx)));
    }
    let ax = w - 8 - 2 * C_ARROW_W;
    spots.push(mk(ax, C_ROW3_T + 4, ax + C_ARROW_W, C_ROW3_B - 4, CandHitZone::PrevPageBtn));
    spots.push(mk(
        ax + C_ARROW_W,
        C_ROW3_T + 4,
        ax + 2 * C_ARROW_W,
        C_ROW3_B - 4,
        CandHitZone::NextPageBtn,
    ));
    (w, CAND_H, spots)
}

/// 命中测试：落在热区外但仍在窗口内的空白区域均可用于拖动候选框
fn hit_test_cand(x: i32, y: i32) -> (CandHitZone, Option<RECT>) {
    if let Ok(spots) = CAND_LAYOUT.lock() {
        for sp in spots.iter() {
            if sp.rect.contains(x, y) {
                return (sp.zone, Some(sp.rect));
            }
        }
    }
    let (w, h) = cand_cur_size();
    if (0..w).contains(&x) && (0..h).contains(&y) {
        (CandHitZone::DragHeader, None)
    } else {
        (CandHitZone::None, None)
    }
}

// 状态栏布局（展开态，高 34）：[⋮⋮ 拖动] [中] [，。] [⚙ 设置] [✕ 退出(仅钩子模式)]
const S_GRIP_R: i32 = 20;
const S_LANG: (i32, i32) = (20, 56);
const S_PUNCT: (i32, i32) = (56, 92);
const S_SET: (i32, i32) = (92, 156);
const S_EXIT: (i32, i32) = (156, 188);

fn hit_test_status(x: i32, y: i32) -> StatusHitZone {
    if STATUS_COLLAPSED.load(Ordering::Relaxed) {
        if (0..=STATUS_ORB_SIZE).contains(&x) && (0..=STATUS_ORB_SIZE).contains(&y) {
            return StatusHitZone::DragArea;
        }
        return StatusHitZone::None;
    }
    if !(0..=STATUS_H).contains(&y) || x < 0 || x > current_status_width() {
        return StatusHitZone::None;
    }
    let tsf_on = TSF_MODE_ACTIVE.load(Ordering::Relaxed);
    let inside = |r: (i32, i32)| x >= r.0 && x < r.1;
    if x < S_GRIP_R {
        StatusHitZone::DragArea
    } else if inside(S_LANG) {
        StatusHitZone::ToggleLangBtn
    } else if inside(S_PUNCT) {
        StatusHitZone::TogglePunctBtn
    } else if inside(S_SET) {
        StatusHitZone::OpenSettingsBtn
    } else if !tsf_on && inside(S_EXIT) {
        StatusHitZone::ExitBtn
    } else {
        StatusHitZone::DragArea
    }
}

/// 判断展开态状态栏是否紧贴工作区的某条边：0 = 否，1 = 左，2 = 右，3 = 上
unsafe fn status_edge_side(pos: (i32, i32)) -> i32 {
    let wa = get_work_area();
    let w = current_status_width();
    if pos.0 <= wa.left + 6 {
        1
    } else if pos.0 + w >= wa.right - 6 {
        2
    } else if pos.1 <= wa.top + 6 {
        3
    } else {
        0
    }
}

/// 若状态栏停在屏幕边缘，则启动「延时自动收缩」定时器；否则取消
unsafe fn arm_status_autohide(hwnd: isize, pos: (i32, i32)) {
    if hwnd == 0 {
        return;
    }
    if !STATUS_COLLAPSED.load(Ordering::Relaxed) && status_edge_side(pos) != 0 {
        SetTimer(hwnd, STATUS_AUTOHIDE_TIMER_ID, STATUS_AUTOHIDE_DELAY_MS, null());
    } else {
        KillTimer(hwnd, STATUS_AUTOHIDE_TIMER_ID);
    }
}

/// 收缩为贴边小方块，返回小方块位置
unsafe fn collapse_status_to_orb(pos: (i32, i32), side: i32) -> (i32, i32) {
    let wa = get_work_area();
    let w = current_status_width();
    let orb = STATUS_ORB_SIZE;
    let y_mid = pos.1 + (STATUS_H - orb) / 2;
    let p = match side {
        1 => (wa.left + 2, y_mid),
        2 => (wa.right - orb - 2, y_mid),
        _ => (pos.0 + (w - orb) / 2, wa.top + 2),
    };
    STATUS_DOCK_SIDE.store(side, Ordering::Relaxed);
    STATUS_COLLAPSED.store(true, Ordering::Relaxed);
    clamp_to_work_area(p.0, p.1, orb, orb)
}

/// 从贴边小方块展开为完整状态栏（保持贴靠在原来那条边上），返回展开后的位置
unsafe fn expand_status_from_orb(orb_pos: (i32, i32)) -> (i32, i32) {
    let wa = get_work_area();
    let w = current_status_width();
    let orb = STATUS_ORB_SIZE;
    let y = orb_pos.1 - (STATUS_H - orb) / 2;
    let p = match STATUS_DOCK_SIDE.load(Ordering::Relaxed) {
        1 => (wa.left + 2, y),
        2 => (wa.right - w - 2, y),
        3 => (orb_pos.0 + orb / 2 - w / 2, wa.top + 2),
        _ => orb_pos,
    };
    STATUS_COLLAPSED.store(false, Ordering::Relaxed);
    STATUS_DOCK_SIDE.store(0, Ordering::Relaxed);
    clamp_to_work_area(p.0, p.1, w, STATUS_H)
}

unsafe fn sync_ui_windows() {
    let mut show_cand = false;
    let mut cand_pos = (0, 0);
    let mut cand_size = cand_cur_size();
    let mut status_pos = (0, 0);
    let mut hwnd_cand = 0;
    let mut hwnd_status = 0;

    if let Ok(mut guard) = GLOBAL_STATE.lock() {
        if let Some(st) = guard.as_mut() {
            show_cand =
                !st.session.input_buffer.is_empty() || st.session.pending_ghost_text.is_some();
            if show_cand {
                let vm = st.session.build_view_model();
                let ghost = if st.session.input_buffer.is_empty() {
                    st.session.pending_ghost_text.clone()
                } else {
                    None
                };
                let (w, h, spots) = compute_cand_layout(&vm, ghost.as_deref());
                if let Ok(mut g) = CAND_LAYOUT.lock() {
                    *g = spots;
                }
                CAND_CUR_W.store(w, Ordering::Relaxed);
                CAND_CUR_H.store(h, Ordering::Relaxed);
                cand_size = (w, h);
            }
            let (sw, sh) = current_status_size();
            st.status_pos = clamp_to_work_area(st.status_pos.0, st.status_pos.1, sw, sh);
            st.cand_pos =
                clamp_to_work_area(st.cand_pos.0, st.cand_pos.1, cand_size.0, cand_size.1);
            cand_pos = st.cand_pos;
            status_pos = st.status_pos;
            hwnd_cand = st.hwnd_cand;
            hwnd_status = st.hwnd_status;
        }
    }

    if hwnd_status != 0 {
        let (sw, sh) = current_status_size();
        SetWindowPos(
            hwnd_status,
            HWND_TOPMOST,
            status_pos.0,
            status_pos.1,
            sw,
            sh,
            SWP_NOACTIVATE,
        );
        InvalidateRect(hwnd_status, null(), 0);
    }
    if hwnd_cand != 0 {
        if show_cand {
            SetWindowPos(
                hwnd_cand,
                HWND_TOPMOST,
                cand_pos.0,
                cand_pos.1,
                cand_size.0,
                cand_size.1,
                SWP_NOACTIVATE,
            );
            ShowWindow(hwnd_cand, SW_SHOWNOACTIVATE);
            InvalidateRect(hwnd_cand, null(), 0);
        } else {
            ShowWindow(hwnd_cand, SW_HIDE);
        }
    }
}

/// 处理来自 `ime_tsf_win.dll` (Windows TSF 原生 COM 客户端) 的 IPC 请求
unsafe fn handle_tsf_ipc_request(req: TsfIpcRequest) -> (TsfIpcResponse, bool) {
    if req.op == "poll_commit" {
        let mut resp = TsfIpcResponse::default();
        if let Ok(mut q) = PENDING_ELEVATED_COMMITS.lock() {
            if !q.is_empty() {
                resp.commit_text = q.drain(..).collect::<String>();
            }
        }
        return (resp, false);
    }
    // 除 deactivate 外，任何来自 DLL 的请求都说明本输入法正在该进程中使用
    note_tip_state(req.pid, req.op != "deactivate");
    // 放行由 SendInput(KEYEVENTF_UNICODE) 注入的 VK_PACKET (0xE7) 事件（兼容已加载旧版 DLL 的进程）
    if req.vk == 0xE7
        || (req.vk == 0 && matches!(req.op.as_str(), "test_key_down" | "key_down" | "key_up"))
    {
        return (TsfIpcResponse::default(), false);
    }

    let caps_on = (GetKeyState(0x14) & 0x0001) != 0;

    let mut resp = TsfIpcResponse::default();
    let mut need_ui_sync = false;

    if let Ok(mut guard) = GLOBAL_STATE.lock() {
        if let Some(st) = guard.as_mut() {
            st.tsf_mode_active = true;
            TSF_MODE_ACTIVE.store(true, Ordering::Relaxed);

            if !req.exe_name.is_empty() && req.exe_name != st.last_fg_exe {
                st.last_fg_exe = req.exe_name.clone();
                st.session.on_foreground_process_changed(&req.exe_name);
                need_ui_sync = true;
            }

            match req.op.as_str() {
                "activate" => {
                    resp.eaten = true;
                    need_ui_sync = true;
                }
                "deactivate" => {
                    st.session.input_buffer.clear();
                    st.session.current_candidates.clear();
                    st.session.pending_ghost_text = None;
                    resp.eaten = false;
                    need_ui_sync = true;
                }
                "test_key_down" => {
                    let vk = req.vk;
                    if matches!(vk, 0x10 | 0xA0 | 0xA1) {
                        st.shift_tap_armed = true;
                        resp.eaten = false;
                    } else {
                        st.shift_tap_armed = false;
                        if req.ctrl && !req.alt && vk == 0x20 {
                            resp.eaten = true;
                        } else if req.alt || (req.ctrl && !matches!(vk, 0x25 | 0x27)) {
                            resp.eaten = false;
                        } else if st.session.english_passthrough_mode || caps_on {
                            resp.eaten = false;
                        } else if st.session.input_buffer.is_empty() {
                            if (0x41..=0x5A).contains(&vk) && !req.shift && !req.ctrl {
                                resp.eaten = true;
                            } else if vk == 0x09 && st.session.pending_ghost_text.is_some() {
                                resp.eaten = true;
                            } else if st.chinese_punct_mode
                                && st.session.active_scene != AppScene::CodeEditor
                                && map_chinese_punctuation(vk, req.shift).is_some()
                            {
                                resp.eaten = true;
                            } else {
                                resp.eaten = false;
                            }
                        } else if map_chinese_punctuation(vk, req.shift).is_some() {
                            resp.eaten = true;
                        } else {
                            let vkey = VirtualKey::from_win32_vk_ex(vk, req.shift, req.ctrl);
                            resp.eaten = !matches!(vkey, VirtualKey::Other(_));
                        }
                    }
                }
                "key_down" => {
                    let vk = req.vk;
                    st.shift_tap_armed = false;

                    if req.caret_x > 5 && req.caret_y > 5 && req.caret_x != 500 && req.caret_y != 500 {
                        st.cand_pos = clamp_to_work_area(req.caret_x, req.caret_y + 8, CAND_W, CAND_H);
                    } else if st.session.input_buffer.is_empty() {
                        st.cand_pos = resolve_caret_screen_pos(st.cand_pos);
                    }

                    if req.ctrl && !req.alt && vk == 0x20 {
                        st.session.english_passthrough_mode = !st.session.english_passthrough_mode;
                        st.session.input_buffer.clear();
                        st.session.current_candidates.clear();
                        resp.eaten = true;
                        need_ui_sync = true;
                    } else if !st.session.english_passthrough_mode && !caps_on {
                        if st.session.input_buffer.is_empty() {
                            if (0x41..=0x5A).contains(&vk) && !req.shift && !req.ctrl {
                                let ch = (b'a' + (vk as u8 - 0x41)) as char;
                                let _ = st.session.process_key(VirtualKey::Char(ch));
                                resp.eaten = true;
                                need_ui_sync = true;
                            } else if vk == 0x09 && st.session.pending_ghost_text.is_some() {
                                if let KeyAction::EatAndCommit { committed_text, .. } =
                                    st.session.process_key(VirtualKey::Tab)
                                {
                                    resp.commit_text = committed_text;
                                    resp.eaten = true;
                                    need_ui_sync = true;
                                }
                            } else if st.chinese_punct_mode
                                && st.session.active_scene != AppScene::CodeEditor
                            {
                                if let Some(punct) = map_chinese_punctuation(vk, req.shift) {
                                    st.session.pending_ghost_text = None;
                                    resp.commit_text = punct.to_string();
                                    resp.eaten = true;
                                    need_ui_sync = true;
                                }
                            }
                        } else if let Some(punct) = map_chinese_punctuation(vk, req.shift) {
                            if let KeyAction::EatAndCommit { committed_text, .. } =
                                st.session.commit_full_sentence()
                            {
                                let out_punct = if st.chinese_punct_mode {
                                    punct
                                } else {
                                    match vk {
                                        0xBC => ",",
                                        0xBE => ".",
                                        0xBF => "?",
                                        0x31 => "!",
                                        0xBA if req.shift => ":",
                                        0xBA => ";",
                                        _ => punct,
                                    }
                                };
                                resp.commit_text = format!("{}{}", committed_text, out_punct);
                                resp.eaten = true;
                                need_ui_sync = true;
                            }
                        } else {
                            let vkey = VirtualKey::from_win32_vk_ex(vk, req.shift, req.ctrl);
                            match st.session.process_key(vkey) {
                                KeyAction::EatAndUpdateUi(_) => {
                                    resp.eaten = true;
                                    need_ui_sync = true;
                                }
                                KeyAction::EatAndCommit { committed_text, .. } => {
                                    resp.commit_text = committed_text;
                                    resp.eaten = true;
                                    need_ui_sync = true;
                                }
                                KeyAction::PassThrough => {
                                    resp.eaten = false;
                                }
                            }
                        }
                    }
                }
                "key_up" => {
                    // TSF DLL 与全局键盘钩子双重保障单击 Shift 切换中英文，使用时间戳去重防止重复切换
                    if matches!(req.vk, 0x10 | 0xA0 | 0xA1) && take_valid_shift_tap() {
                        let now = mono_ms();
                        let last = LAST_SHIFT_TOGGLED_AT_MS.load(Ordering::Relaxed);
                        if now.saturating_sub(last) > 250 {
                            LAST_SHIFT_TOGGLED_AT_MS.store(now, Ordering::Relaxed);
                            st.shift_tap_armed = false;
                            let act = st.session.process_key(VirtualKey::Shift);
                            if let KeyAction::EatAndCommit { committed_text, .. } = act {
                                resp.commit_text = committed_text;
                            }
                            resp.eaten = false;
                            need_ui_sync = true;
                        }
                    }
                }
                _ => {}
            }

            // 对常规权限窗口：将待上屏文本排入主线程队列，等 TCP 响应返回后再通过 SendInput(KEYEVENTF_UNICODE) 注入目标窗口；
            // 对管理员高权限窗口（如 Taskmgr.exe）：跨进程 SendInput 会被 Windows UIPI 拦截（返回 0 导致无法落字），
            // 因此直接通过 resp.commit_text 返回给目标进程内的 ime_tsf_win.dll，由其在高权限进程内部完成上屏！
            if !resp.commit_text.is_empty() && !is_process_elevated(req.pid) {
                let txt = std::mem::take(&mut resp.commit_text);
                st.pending_tsf_commits.push(txt);
                need_ui_sync = true;
            }
        }
    }

    (resp, need_ui_sync)
}

/// 启动本地回环 TSF IPC 服务端线程 (`127.0.0.1:18435`)，接收来自 `ime_tsf_win.dll` 的 TSF 原生按键与光标事件
fn spawn_tsf_ipc_server(hwnd_cand: isize) {
    std::thread::spawn(move || {
        let Ok(listener) = TcpListener::bind(("127.0.0.1", TSF_IPC_PORT)) else {
            return;
        };
        for stream in listener.incoming().flatten() {
            let _ = stream.set_nodelay(true);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(80)));
            let _ = stream.set_write_timeout(Some(Duration::from_millis(80)));
            let Ok(mut writer) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let mut notify_ui = false;
            if reader.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                if let Ok(req) = serde_json::from_str::<TsfIpcRequest>(line.trim()) {
                    let (resp, need_sync) = unsafe { handle_tsf_ipc_request(req) };
                    notify_ui = need_sync;
                    if let Ok(mut out) = serde_json::to_string(&resp) {
                        out.push('\n');
                        let _ = writer.write_all(out.as_bytes());
                        let _ = writer.flush();
                    }
                }
            }
            drop(writer);
            drop(reader);
            if notify_ui && hwnd_cand != 0 {
                unsafe {
                    PostMessageW(hwnd_cand, WM_APP_TSF_SYNC, 0, 0);
                }
            }
        }
    });
}

/// 全局低级键盘钩子回调：当处于免管理员钩子模式时拦截按键；若已切换为 TSF 正式输入法模式则自动让位放行
unsafe extern "system" fn low_level_keyboard_proc(
    n_code: i32,
    w_param: usize,
    l_param: isize,
) -> isize {
    if n_code < 0 || l_param == 0 {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }
    let kb = &*(l_param as *const KBDLLHOOKSTRUCT);
    // 放行由本输入法 SendInput 注入的 Unicode 字符事件；
    // 在 TSF 正式模式下对常规桌面应用自动让位，但对受 AppContainer 网络隔离的 SearchHost / StartMenuExperienceHost 直接接管按键
    if (kb.flags & LLKHF_INJECTED) != 0 {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }
    let fg_exe = get_foreground_exe_name();
    // 无论 TSF / 钩子模式，都在全局键盘钩子中实时跟踪并执行「单击 Shift 切换中英文」
    // （避免 Windows TSF 对未吞掉的修饰键不派发 OnTestKeyUp 导致 Shift 无法切换）
    {
        let m = w_param as u32;
        let is_down = matches!(m, 0x0100 | 0x0104); // WM_KEYDOWN / WM_SYSKEYDOWN
        let is_up = matches!(m, 0x0101 | 0x0105); // WM_KEYUP / WM_SYSKEYUP
        if is_down || is_up {
            track_shift_tap(kb.vk_code, is_down);
        }
        if is_up && matches!(kb.vk_code, 0x10 | 0xA0 | 0xA1) && take_valid_shift_tap() {
            let fg_hwnd = GetForegroundWindow();
            let mut skip_shift = !status_bar_should_show()
                || fg_exe.to_ascii_lowercase().contains("ime-desktop-ui")
                || fg_exe.contains("体验台");
            let mut commit_on_shift_up: Option<String> = None;
            let mut did_toggle = false;
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if st.hwnd_settings != 0 && fg_hwnd == st.hwnd_settings {
                        skip_shift = true;
                    }
                    if !skip_shift {
                        let now = mono_ms();
                        let last = LAST_SHIFT_TOGGLED_AT_MS.load(Ordering::Relaxed);
                        if now.saturating_sub(last) > 250 {
                            LAST_SHIFT_TOGGLED_AT_MS.store(now, Ordering::Relaxed);
                            st.shift_tap_armed = false;
                            let act = st.session.process_key(VirtualKey::Shift);
                            if let KeyAction::EatAndCommit { committed_text, .. } = act {
                                commit_on_shift_up = Some(committed_text);
                            }
                            did_toggle = true;
                        }
                    }
                }
            }
            if let Some(txt) = commit_on_shift_up {
                send_unicode_text(&txt);
            }
            if did_toggle {
                sync_ui_windows();
            }
        }
    }
    if !status_bar_should_show() {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }
    // TSF 模式下常规桌面程序由进程内 TSF 接管（且避免捕获 ESET ekbdflt.sys 等安全驱动在进程外混淆的按键码），
    // 仅对不加载未签名 TSF DLL 的系统壳层搜索宿主保留低级键盘钩子兜底
    if TSF_MODE_ACTIVE.load(Ordering::Relaxed) && !is_shell_appcontainer_host(&fg_exe) {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    let msg = w_param as u32;
    let vk = kb.vk_code;
    if vk >= 256 {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    // 处理按键抬起 (WM_KEYUP)
    if msg == WM_KEYUP {
        let mut should_eat_up = false;
        let mut commit_on_shift_up: Option<String> = None;

        if let Ok(mut guard) = GLOBAL_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                if matches!(vk, 0x10 | 0xA0 | 0xA1) && st.shift_tap_armed {
                    st.shift_tap_armed = false;
                    if take_valid_shift_tap() {
                        let act = st.session.process_key(VirtualKey::Shift);
                        if let KeyAction::EatAndCommit { committed_text, .. } = act {
                            commit_on_shift_up = Some(committed_text);
                        }
                    }
                }
                if st.eaten_vks[vk as usize] {
                    st.eaten_vks[vk as usize] = false;
                    should_eat_up = true;
                }
            }
        }
        if let Some(txt) = commit_on_shift_up {
            send_unicode_text(&txt);
        }
        sync_ui_windows();
        if should_eat_up {
            return 1;
        }
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    if msg != WM_KEYDOWN {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    // 检测当前焦点进程（若焦点在桌面体验台或本输入法设置窗口内，则直接放行）
    let fg_hwnd = GetForegroundWindow();
    if let Ok(guard) = GLOBAL_STATE.lock() {
        if let Some(st) = guard.as_ref() {
            if st.hwnd_settings != 0 && fg_hwnd == st.hwnd_settings {
                return CallNextHookEx(0, n_code, w_param, l_param);
            }
        }
    }
    if fg_exe.to_ascii_lowercase().contains("ime-desktop-ui") || fg_exe.contains("体验台") {
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    let shift = (GetAsyncKeyState(0x10) as u16 & 0x8000) != 0;
    let ctrl = (GetAsyncKeyState(0x11) as u16 & 0x8000) != 0;
    let alt = (GetAsyncKeyState(0x12) as u16 & 0x8000) != 0;
    let win_key = (GetAsyncKeyState(0x5B) as u16 & 0x8000) != 0
        || (GetAsyncKeyState(0x5C) as u16 & 0x8000) != 0;
    let caps_on = (GetKeyState(0x14) & 0x0001) != 0;

    // 记录单按 Shift 切换中英文的候选状态
    if matches!(vk, 0x10 | 0xA0 | 0xA1) {
        if let Ok(mut guard) = GLOBAL_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.shift_tap_armed = true;
            }
        }
        return CallNextHookEx(0, n_code, w_param, l_param);
    }

    let mut eat_key = false;
    let mut need_activate_win_search = false;
    let mut text_to_send: Option<String> = None;

    if let Ok(mut guard) = GLOBAL_STATE.lock() {
        if let Some(st) = guard.as_mut() {
            st.shift_tap_armed = false;

            // 更新前台应用场景
            if !fg_exe.is_empty() && fg_exe != st.last_fg_exe {
                st.last_fg_exe = fg_exe.clone();
                st.session.on_foreground_process_changed(&fg_exe);
            }

            // Ctrl + Space 切换中/英模式
            if ctrl && !alt && !win_key && vk == 0x20 {
                st.session.english_passthrough_mode = !st.session.english_passthrough_mode;
                st.session.input_buffer.clear();
                st.session.current_candidates.clear();
                st.eaten_vks[vk as usize] = true;
                eat_key = true;
            } else if alt || win_key || (ctrl && !matches!(vk, 0x25 | 0x27)) {
                // 带有 Ctrl/Alt/Win 的快捷键（除 Ctrl+Left/Right 音节跳动外）全部原样放行
                return CallNextHookEx(0, n_code, w_param, l_param);
            } else if st.session.english_passthrough_mode || caps_on {
                // 英文直通模式或大写锁定开启时，直接放行
                return CallNextHookEx(0, n_code, w_param, l_param);
            } else {
                let composing = !st.session.input_buffer.is_empty();

                if !composing {
                    // 当前未在拼写拼音：
                    // 1) 若敲击小写字母 a..z (未按 Shift)，启动拼音输入并锚定候选窗位置
                    if (0x41..=0x5A).contains(&vk) && !shift && !ctrl && !focus_is_password_field() {
                        if fg_exe.eq_ignore_ascii_case("startmenuexperiencehost.exe") {
                            need_activate_win_search = true;
                        }
                        st.cand_pos = resolve_caret_screen_pos(st.cand_pos);
                        let ch = (b'a' + (vk as u8 - 0x41)) as char;
                        let _ = st.session.process_key(VirtualKey::Char(ch));
                        st.eaten_vks[vk as usize] = true;
                        eat_key = true;
                    } else if vk == 0x09 && st.session.pending_ghost_text.is_some() {
                        // 按 Tab 采纳 AI Ghost Text 续写
                        if let KeyAction::EatAndCommit { committed_text, .. } =
                            st.session.process_key(VirtualKey::Tab)
                        {
                            text_to_send = Some(committed_text);
                            st.eaten_vks[vk as usize] = true;
                            eat_key = true;
                        }
                    } else if st.chinese_punct_mode && st.session.active_scene != AppScene::CodeEditor {
                        // 中文标点直接转换
                        if let Some(punct) = map_chinese_punctuation(vk, shift) {
                            st.session.pending_ghost_text = None;
                            text_to_send = Some(punct.to_string());
                            st.eaten_vks[vk as usize] = true;
                            eat_key = true;
                        }
                    } else {
                        st.session.pending_ghost_text = None;
                    }
                } else {
                    // 当前正在拼写拼音 (composing == true)：
                    // 若敲击常用句末标点 (，。？！；：)，自动将首选整句连同标点一并上屏！
                    if let Some(punct) = map_chinese_punctuation(vk, shift) {
                        if let KeyAction::EatAndCommit { committed_text, .. } =
                            st.session.commit_full_sentence()
                        {
                            let out_punct = if st.chinese_punct_mode {
                                punct
                            } else {
                                match vk {
                                    0xBC => ",",
                                    0xBE => ".",
                                    0xBF => "?",
                                    0x31 => "!",
                                    0xBA if shift => ":",
                                    0xBA => ";",
                                    _ => punct,
                                }
                            };
                            text_to_send = Some(format!("{}{}", committed_text, out_punct));
                            st.eaten_vks[vk as usize] = true;
                            eat_key = true;
                        }
                    } else {
                        let vkey = VirtualKey::from_win32_vk_ex(vk, shift, ctrl);
                        match st.session.process_key(vkey) {
                            KeyAction::EatAndUpdateUi(_) => {
                                st.eaten_vks[vk as usize] = true;
                                eat_key = true;
                            }
                            KeyAction::EatAndCommit { committed_text, .. } => {
                                text_to_send = Some(committed_text);
                                st.eaten_vks[vk as usize] = true;
                                eat_key = true;
                            }
                            KeyAction::PassThrough => {}
                        }
                    }
                }
            }
        }
    }

    if need_activate_win_search {
        activate_windows_search_box_from_start_menu();
    }
    if let Some(txt) = text_to_send {
        send_unicode_text(&txt);
    }
    if eat_key {
        sync_ui_windows();
        return 1;
    }

    CallNextHookEx(0, n_code, w_param, l_param)
}

/// 悬浮候选框窗口过程（`WS_EX_NOACTIVATE` 无焦点点击）
unsafe extern "system" fn wnd_proc_cand(
    hwnd: isize,
    msg: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_ERASEBKGND => 1,
        WM_APP_AI_READY => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.session.refresh_candidates();
                }
            }
            sync_ui_windows();
            0
        }
        WM_APP_TSF_SYNC => {
            let mut commits = Vec::new();
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if !st.pending_tsf_commits.is_empty() {
                        commits = std::mem::take(&mut st.pending_tsf_commits);
                    }
                }
            }
            for txt in commits {
                send_unicode_text(&txt);
            }
            sync_ui_windows();
            0
        }
        WM_MOUSEMOVE => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let mut move_win: Option<(i32, i32)> = None;
            let (zone, _) = hit_test_cand(x, y);
            track_mouse_leave(hwnd);

            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if let Some((dx, dy)) = st.dragging_cand {
                        if (GetAsyncKeyState(0x01) as u16 & 0x8000) != 0 {
                            let mut pt = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            let (cw, ch) = cand_cur_size();
                            st.cand_pos = clamp_to_work_area(pt.x - dx, pt.y - dy, cw, ch);
                            move_win = Some(st.cand_pos);
                        } else {
                            st.dragging_cand = None;
                        }
                    }
                    if st.cand_hover != zone {
                        st.cand_hover = zone;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            if let Some((nx, ny)) = move_win {
                SetWindowPos(hwnd, HWND_TOPMOST, nx, ny, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE);
            }
            let cursor_id = match zone {
                CandHitZone::None => 32512,
                CandHitZone::DragHeader => 32646, // IDC_SIZEALL：空白处可拖动
                _ => 32649,
            };
            SetCursor(LoadCursorW(0, cursor_id as *const u16));
            0
        }
        WM_MOUSELEAVE => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if st.cand_hover != CandHitZone::None {
                        st.cand_hover = CandHitZone::None;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.dragging_cand = None;
                }
            }
            ReleaseCapture();
            0
        }
        WM_CAPTURECHANGED => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.dragging_cand = None;
                }
            }
            0
        }
        WM_RBUTTONDOWN => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.session.process_key(VirtualKey::CtrlRight);
                }
            }
            sync_ui_windows();
            0
        }
        WM_LBUTTONDOWN => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let mut text_to_send: Option<String> = None;
            let mut start_drag = false;
            let (zone, rect_opt) = hit_test_cand(x, y);

            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    match zone {
                        CandHitZone::DragHeader => {
                            let mut pt = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            st.dragging_cand = Some((pt.x - st.cand_pos.0, pt.y - st.cand_pos.1));
                            start_drag = true;
                        }
                        CandHitZone::PinyinSyllable(syl_idx) => {
                            let inner_offset = if let (Some(rc), Some(span)) =
                                (rect_opt, st.session.last_syllable_spans.get(syl_idx))
                            {
                                let w = (rc.right - rc.left - 10).max(1);
                                let rel_x = (x - rc.left - 5).clamp(0, w);
                                let len = span.end.saturating_sub(span.start).max(1);
                                let char_off = ((rel_x * len as i32 + w / 2) / w) as usize;
                                Some(char_off.clamp(1, len))
                            } else {
                                None
                            };
                            st.session.click_pinyin_syllable(syl_idx, inner_offset);
                        }
                        CandHitZone::PinyinEndZone => {
                            st.session.return_cursor_to_end();
                        }
                        CandHitZone::PrevSyllableBtn => {
                            st.session.process_key(VirtualKey::CtrlLeft);
                        }
                        CandHitZone::NextSyllableBtn => {
                            st.session.process_key(VirtualKey::CtrlRight);
                        }
                        CandHitZone::PrevPageBtn => {
                            st.session.process_key(VirtualKey::PageUp);
                        }
                        CandHitZone::NextPageBtn => {
                            st.session.process_key(VirtualKey::PageDown);
                        }
                        CandHitZone::CommitFullSentenceBtn => {
                            if let KeyAction::EatAndCommit { committed_text, .. } =
                                st.session.commit_full_sentence()
                            {
                                text_to_send = Some(committed_text);
                            }
                        }
                        CandHitZone::ClearPinyinBtn => {
                            st.session.process_key(VirtualKey::Escape);
                        }
                        CandHitZone::CandidateRow(idx) => {
                            if let KeyAction::EatAndCommit { committed_text, .. } =
                                st.session.process_key(VirtualKey::Digit((idx + 1) as u8))
                            {
                                text_to_send = Some(committed_text);
                            }
                        }
                        CandHitZone::AcceptGhostBtn => {
                            if let KeyAction::EatAndCommit { committed_text, .. } =
                                st.session.process_key(VirtualKey::Tab)
                            {
                                text_to_send = Some(committed_text);
                            }
                        }
                        CandHitZone::None => {}
                    }
                }
            }
            if start_drag {
                SetCapture(hwnd);
                return 0;
            }
            if let Some(txt) = text_to_send {
                send_unicode_text(&txt);
            }
            sync_ui_windows();
            0
        }
        WM_PAINT => {
            let (cw, ch) = cand_cur_size();
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc_screen = BeginPaint(hwnd, &mut ps);
            let hdc = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, cw, ch);
            let old_bm = SelectObject(hdc, hbm);
            let f = ui_fonts();
            let old_font = SelectObject(hdc, f.pinyin);

            SetBkMode(hdc, TRANSPARENT);
            fill_rect_color(hdc, 0, 0, cw, ch, UI_BG);
            draw_fallback_border(hdc, cw, ch);

            let spots: Vec<CandHotspot> = CAND_LAYOUT
                .lock()
                .map(|g| g.clone())
                .unwrap_or_default();
            let find = |z: CandHitZone| spots.iter().find(|h| h.zone == z).map(|h| h.rect);

            if let Ok(guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_ref() {
                    let session = &st.session;
                    let hover = st.cand_hover;
                    let vm = session.build_view_model();
                    let ghost = if session.input_buffer.is_empty() {
                        session.pending_ghost_text.as_deref()
                    } else {
                        None
                    };

                    if let Some(g) = ghost {
                        if let Some(rc) = find(CandHitZone::AcceptGhostBtn) {
                            if hover == CandHitZone::AcceptGhostBtn {
                                s_round(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, UI_HOVER, UI_HOVER);
                            }
                            s_text(hdc, f.small, UI_MUTED, rc.left, rc.top, rc.right - 12, rc.bottom, "Tab 采纳", T_LINE | DT_RIGHT);
                            s_text(hdc, f.cand, UI_TEXT, rc.left + 12, rc.top, rc.right - 80, rc.bottom, &ghost_label(g), T_LINE);
                        }
                    } else if vm.visible {
                        // 第 1 行：拼音音节（焦点音节浅蓝底 + 蓝字，已锁定音节绿色，光标为 2px 蓝色竖线）
                        let total_syls = vm.syllable_spans.len();
                        for (idx, syl) in vm.syllable_spans.iter().enumerate() {
                            let Some(rc) = find(CandHitZone::PinyinSyllable(idx)) else {
                                continue;
                            };
                            let is_active = vm.active_syllable_index == Some(idx);
                            let is_tail = vm.active_syllable_index.is_none()
                                && idx + 1 == total_syls
                                && vm.cursor_byte_pos >= syl.end;
                            let is_hover = hover == CandHitZone::PinyinSyllable(idx);
                            let is_pinned = session
                                .pinned_segments
                                .iter()
                                .any(|(&s, &(e, _))| syl.start >= s && syl.end <= e);
                            let (t, b) = (rc.top + 2, rc.bottom - 2);
                            if is_active {
                                s_round(hdc, rc.left, t, rc.right, b, 6, UI_ACCENT_BG, UI_ACCENT_BG);
                            } else if is_pinned {
                                s_round(hdc, rc.left, t, rc.right, b, 6, UI_PINNED_BG, UI_PINNED_BG);
                            } else if is_hover {
                                s_round(hdc, rc.left, t, rc.right, b, 6, UI_HOVER, UI_HOVER);
                            }
                            let col = if is_active {
                                UI_ACCENT
                            } else if is_pinned {
                                UI_PINNED
                            } else {
                                UI_TEXT
                            };
                            let tx = rc.left + 5;
                            s_text(hdc, f.pinyin, col, tx, rc.top, rc.right + 2, rc.bottom, &syl.code_key, T_LINE);
                            if is_active || is_tail {
                                SelectObject(hdc, f.pinyin);
                                let rel = vm
                                    .cursor_byte_pos
                                    .saturating_sub(syl.start)
                                    .min(syl.code_key.len());
                                let prefix = if syl.code_key.is_char_boundary(rel) {
                                    &syl.code_key[..rel]
                                } else {
                                    syl.code_key.as_str()
                                };
                                let cx = (tx + measure_text_width(hdc, prefix)).min(rc.right - 2);
                                fill_rect_color(hdc, cx, t + 2, cx + 2, b - 2, UI_ACCENT);
                            }
                        }
                        for (zone, glyph) in [
                            (CandHitZone::PrevSyllableBtn, "\u{E76B}"),
                            (CandHitZone::NextSyllableBtn, "\u{E76C}"),
                            (CandHitZone::ClearPinyinBtn, "\u{E711}"),
                        ] {
                            if let Some(rc) = find(zone) {
                                if hover == zone {
                                    s_round(hdc, rc.left + 1, rc.top + 1, rc.right - 1, rc.bottom - 1, 6, UI_HOVER, UI_HOVER);
                                }
                                s_text(hdc, f.icon, UI_TEXT2, rc.left, rc.top, rc.right, rc.bottom, glyph, T_CENTER);
                            }
                        }

                        // 第 2 行：整句预览（点击整句上屏，或按 ↑ 键选中后按空格上屏），手动锁定的词以绿色标出
                        if let Some(rc) = find(CandHitZone::CommitFullSentenceBtn) {
                            if vm.full_sentence_selected {
                                s_round(hdc, rc.left, rc.top + 1, rc.right, rc.bottom - 1, 6, UI_SELECTED, UI_SELECTED);
                                let mid = (rc.top + rc.bottom) / 2;
                                s_round(hdc, rc.left + 3, mid - 7, rc.left + 6, mid + 7, 3, UI_ACCENT, UI_ACCENT);
                            } else if hover == CandHitZone::CommitFullSentenceBtn {
                                s_round(hdc, rc.left, rc.top + 1, rc.right, rc.bottom - 1, 6, UI_HOVER, UI_HOVER);
                            }
                            SelectObject(hdc, f.small_b);
                            let label_w = measure_text_width(hdc, "整句");
                            let label_left = rc.left + 12;
                            s_text(hdc, f.small_b, UI_ACCENT, label_left, rc.top, label_left + label_w + 2, rc.bottom, "整句", T_LINE);
                            let hint_text = if vm.full_sentence_selected {
                                Some(("空格上屏", UI_ACCENT))
                            } else if vm.active_syllable_index.is_some() {
                                Some(("↑ 整句", UI_MUTED))
                            } else {
                                None
                            };
                            let right_reserved = if let Some((ht, _)) = hint_text {
                                SelectObject(hdc, f.small);
                                measure_text_width(hdc, ht) + 14
                            } else {
                                4
                            };
                            let mut cur_x = label_left + label_w + 10;
                            let max_x = rc.right - right_reserved;
                            let unpinned_fg = if vm.full_sentence_selected { UI_TEXT } else { UI_TEXT2 };
                            for (part, pinned) in &vm.full_sentence_parts {
                                if part.is_empty() || cur_x >= max_x {
                                    continue;
                                }
                                SelectObject(hdc, f.num);
                                let w = measure_text_width(hdc, part);
                                if *pinned {
                                    let r = (cur_x + w + 10).min(max_x);
                                    s_round(hdc, cur_x, rc.top + 3, r, rc.bottom - 3, 6, UI_PINNED_BG, UI_PINNED_BG);
                                    s_text(hdc, f.num, UI_PINNED, cur_x + 5, rc.top, r, rc.bottom, part, T_LINE);
                                    cur_x = r;
                                } else {
                                    let r = (cur_x + w + 2).min(max_x);
                                    s_text(hdc, f.num, unpinned_fg, cur_x, rc.top, r, rc.bottom, part, T_LINE);
                                    cur_x += w;
                                }
                            }
                            if let Some((ht, hcolor)) = hint_text {
                                SelectObject(hdc, f.small);
                                let hw = measure_text_width(hdc, ht);
                                s_text(hdc, f.small, hcolor, rc.right - hw - 8, rc.top, rc.right - 6, rc.bottom, ht, T_LINE);
                            }
                        }

                        fill_rect_color(hdc, 8, C_DIVIDER_Y, cw - 8, C_DIVIDER_Y + 1, UI_DIVIDER);

                        // 第 3 行：横排候选（选中项浅灰蓝底 + 左侧蓝色竖条）
                        for (idx, cand) in vm.visible_candidates.iter().enumerate() {
                            let Some(rc) = find(CandHitZone::CandidateRow(idx)) else {
                                continue;
                            };
                            let is_sel = !vm.full_sentence_selected && idx == vm.selected_candidate_index;
                            let is_hover = hover == CandHitZone::CandidateRow(idx);
                            let (t, b) = (rc.top + 3, rc.bottom - 3);
                            if is_sel {
                                s_round(hdc, rc.left, t, rc.right, b, 6, UI_SELECTED, UI_SELECTED);
                                let mid = (t + b) / 2;
                                s_round(hdc, rc.left + 3, mid - 8, rc.left + 6, mid + 8, 3, UI_ACCENT, UI_ACCENT);
                            } else if is_hover {
                                s_round(hdc, rc.left, t, rc.right, b, 6, UI_HOVER, UI_HOVER);
                            }
                            let num = format!("{}", idx + 1);
                            SelectObject(hdc, f.num);
                            let num_w = measure_text_width(hdc, &num);
                            s_text(hdc, f.num, UI_TEXT2, rc.left + 14, rc.top, rc.left + 14 + num_w + 2, rc.bottom, &num, T_LINE);
                            s_text(hdc, f.cand, UI_TEXT, rc.left + 14 + num_w + 6, rc.top, rc.right - 10, rc.bottom, &cand.text, T_LINE);
                            if cand.tag.contains("AI") {
                                s_ellipse(hdc, rc.right - 9, t + 5, rc.right - 4, t + 10, rgb(124, 58, 237));
                            }
                        }

                        let has_prev = vm.page_index > 0;
                        let has_next = (vm.page_index + 1) * vm.page_size.max(1) < vm.total_candidates;
                        for (zone, glyph, enabled) in [
                            (CandHitZone::PrevPageBtn, "◀", has_prev),
                            (CandHitZone::NextPageBtn, "▶", has_next),
                        ] {
                            if let Some(rc) = find(zone) {
                                if enabled && hover == zone {
                                    s_round(hdc, rc.left + 1, rc.top, rc.right - 1, rc.bottom, 6, UI_HOVER, UI_HOVER);
                                }
                                s_text(hdc, f.arrow, if enabled { UI_TEXT2 } else { UI_DISABLED }, rc.left, rc.top, rc.right, rc.bottom, glyph, T_CENTER);
                            }
                        }
                    }
                }
            }

            BitBlt(hdc_screen, 0, 0, cw, ch, hdc, 0, 0, SRCCOPY);
            SelectObject(hdc, old_font);
            SelectObject(hdc, old_bm);
            DeleteObject(hbm);
            DeleteDC(hdc);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

const WM_CAPTURECHANGED: u32 = 0x0215;
const DT_RIGHT: u32 = 0x0002;

/// 请求在鼠标离开窗口时收到 `WM_MOUSELEAVE`，以便清除悬停高亮
unsafe fn track_mouse_leave(hwnd: isize) {
    let mut tme = TRACKMOUSEEVENT {
        cb_size: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dw_flags: TME_LEAVE,
        hwnd_track: hwnd,
        dw_hover_time: 0,
    };
    TrackMouseEvent(&mut tme);
}

/// Win10 等不支持 DWM 圆角的系统上，自绘 1px 浅灰边框
unsafe fn draw_fallback_border(hdc: isize, w: i32, h: i32) {
    if !DWM_ROUND_OK.load(Ordering::Relaxed) {
        fill_rect_color(hdc, 0, 0, w, 1, UI_BORDER);
        fill_rect_color(hdc, 0, h - 1, w, h, UI_BORDER);
        fill_rect_color(hdc, 0, 0, 1, h, UI_BORDER);
        fill_rect_color(hdc, w - 1, 0, w, h, UI_BORDER);
    }
}

/// 右下角常驻悬浮状态栏窗口过程（支持工作区边界防越界、防下沉任务栏、贴边自动收缩悬浮球）
unsafe extern "system" fn wnd_proc_status(
    hwnd: isize,
    msg: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_ERASEBKGND => 1,
        0x0113 /* WM_TIMER */ if w_param == STATUS_VIS_TIMER_ID => {
            update_status_bar_visibility(hwnd);
            0
        }
        WM_APP_AI_READY => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.session.refresh_candidates();
                }
            }
            sync_ui_windows();
            0
        }
        WM_APP_TSF_SYNC => {
            let mut commits = Vec::new();
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if !st.pending_tsf_commits.is_empty() {
                        commits = std::mem::take(&mut st.pending_tsf_commits);
                    }
                }
            }
            for txt in commits {
                send_unicode_text(&txt);
            }
            sync_ui_windows();
            0
        }
        WM_APP_OPEN_SETTINGS => {
            show_settings_window();
            0
        }
        0x0113 /* WM_TIMER */ if w_param == STATUS_AUTOHIDE_TIMER_ID => {
            // 贴边停留到时：鼠标仍悬停或正在拖动则继续等待，否则收缩为贴边小方块
            KillTimer(hwnd, STATUS_AUTOHIDE_TIMER_ID);
            if STATUS_COLLAPSED.load(Ordering::Relaxed) {
                return 0;
            }
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetWindowRect(hwnd, &mut rc);
            let hovered = pt.x >= rc.left && pt.x < rc.right && pt.y >= rc.top && pt.y < rc.bottom;
            let mut rearm = false;
            let mut need_sync = false;
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if hovered || st.dragging_status.is_some() {
                        rearm = true;
                    } else {
                        let side = status_edge_side(st.status_pos);
                        if side != 0 {
                            st.status_pos = collapse_status_to_orb(st.status_pos, side);
                            st.status_hover = StatusHitZone::None;
                            need_sync = true;
                        }
                    }
                }
            }
            if rearm {
                SetTimer(hwnd, STATUS_AUTOHIDE_TIMER_ID, STATUS_AUTOHIDE_DELAY_MS, null());
            }
            if need_sync {
                sync_ui_windows();
            }
            0
        }
        WM_MOUSEMOVE => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let zone = hit_test_status(x, y);
            let mut move_win: Option<(i32, i32, i32, i32)> = None;
            track_mouse_leave(hwnd);

            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if let Some((dx, dy)) = st.dragging_status {
                        if (GetAsyncKeyState(0x01) as u16 & 0x8000) != 0 {
                            let mut pt = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            let sx = STATUS_DRAG_START_X.load(Ordering::Relaxed);
                            let sy = STATUS_DRAG_START_Y.load(Ordering::Relaxed);
                            if (pt.x - sx).abs() + (pt.y - sy).abs() >= 4 {
                                st.status_drag_moved = true;
                            }
                            if STATUS_COLLAPSED.load(Ordering::Relaxed) {
                                if st.status_drag_moved {
                                    // 拖动贴边小方块：立即展开成完整状态栏并继续跟随鼠标拖动
                                    STATUS_COLLAPSED.store(false, Ordering::Relaxed);
                                    STATUS_DOCK_SIDE.store(0, Ordering::Relaxed);
                                    let w = current_status_width();
                                    let np = clamp_to_work_area(
                                        pt.x - S_GRIP_R / 2,
                                        pt.y - STATUS_H / 2,
                                        w,
                                        STATUS_H,
                                    );
                                    st.status_pos = np;
                                    st.dragging_status = Some((pt.x - np.0, pt.y - np.1));
                                    move_win = Some((np.0, np.1, w, STATUS_H));
                                }
                            } else {
                                let (w, h) = current_status_size();
                                st.status_pos = clamp_to_work_area(pt.x - dx, pt.y - dy, w, h);
                                move_win = Some((st.status_pos.0, st.status_pos.1, w, h));
                            }
                        } else {
                            st.dragging_status = None;
                        }
                    }
                    if st.status_hover != zone {
                        st.status_hover = zone;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            if let Some((nx, ny, w, h)) = move_win {
                SetWindowPos(hwnd, HWND_TOPMOST, nx, ny, w, h, SWP_NOACTIVATE);
                InvalidateRect(hwnd, null(), 0);
            }
            let cursor_id = if STATUS_COLLAPSED.load(Ordering::Relaxed) {
                32649
            } else {
                match zone {
                    StatusHitZone::None => 32512,
                    StatusHitZone::DragArea => 32646, // IDC_SIZEALL
                    _ => 32649,
                }
            };
            SetCursor(LoadCursorW(0, cursor_id as *const u16));
            0
        }
        WM_MOUSELEAVE => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    if st.status_hover != StatusHitZone::None {
                        st.status_hover = StatusHitZone::None;
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
            }
            0
        }
        WM_CAPTURECHANGED => {
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.dragging_status = None;
                }
            }
            0
        }
        WM_LBUTTONUP => {
            let mut need_sync = false;
            let mut arm_pos: Option<(i32, i32)> = None;
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    let was_dragging = st.dragging_status.is_some();
                    let moved = st.status_drag_moved;
                    st.dragging_status = None;
                    st.status_drag_moved = false;
                    if was_dragging && !moved && STATUS_COLLAPSED.load(Ordering::Relaxed) {
                        // 单击贴边小方块：展开为完整状态栏（仍贴在原来的边上，可继续拖动）
                        st.status_pos = expand_status_from_orb(st.status_pos);
                        need_sync = true;
                        arm_pos = Some(st.status_pos);
                    } else if was_dragging {
                        arm_pos = Some(st.status_pos);
                    }
                }
            }
            ReleaseCapture();
            if need_sync {
                sync_ui_windows();
            }
            if let Some(p) = arm_pos {
                arm_status_autohide(hwnd, p);
            }
            0
        }
        WM_LBUTTONDOWN => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let zone = hit_test_status(x, y);
            let mut should_quit = false;
            let mut should_open_settings = false;
            let mut start_drag = false;

            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    match zone {
                        StatusHitZone::DragArea => {
                            let mut pt = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut pt);
                            STATUS_DRAG_START_X.store(pt.x, Ordering::Relaxed);
                            STATUS_DRAG_START_Y.store(pt.y, Ordering::Relaxed);
                            st.status_drag_moved = false;
                            st.dragging_status =
                                Some((pt.x - st.status_pos.0, pt.y - st.status_pos.1));
                            start_drag = true;
                        }
                        StatusHitZone::ToggleLangBtn => {
                            st.session.english_passthrough_mode =
                                !st.session.english_passthrough_mode;
                            st.session.input_buffer.clear();
                            st.session.current_candidates.clear();
                        }
                        StatusHitZone::TogglePunctBtn => {
                            st.chinese_punct_mode = !st.chinese_punct_mode;
                        }
                        StatusHitZone::OpenSettingsBtn => {
                            should_open_settings = true;
                        }
                        StatusHitZone::ExitBtn => {
                            should_quit = true;
                        }
                        StatusHitZone::None => {}
                    }
                }
            }
            if start_drag {
                KillTimer(hwnd, STATUS_AUTOHIDE_TIMER_ID);
                SetCapture(hwnd);
                return 0;
            }
            if should_open_settings {
                show_settings_window();
            }
            if should_quit {
                PostQuitMessage(0);
            } else {
                sync_ui_windows();
            }
            0
        }
        WM_PAINT => {
            let (sw, sh) = current_status_size();
            let is_collapsed = STATUS_COLLAPSED.load(Ordering::Relaxed);
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc_screen = BeginPaint(hwnd, &mut ps);
            let hdc = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, sw, sh);
            let old_bm = SelectObject(hdc, hbm);
            let f = ui_fonts();
            let old_font = SelectObject(hdc, f.status);

            SetBkMode(hdc, TRANSPARENT);
            fill_rect_color(hdc, 0, 0, sw, sh, UI_BG);
            draw_fallback_border(hdc, sw, sh);

            if let Ok(guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_ref() {
                    let is_en = st.session.english_passthrough_mode;
                    let ai_on = st.persistent_cfg.ai_config.enabled;
                    let lang_col = if is_en { UI_TEXT } else { UI_ACCENT };
                    let lang = if is_en { "英" } else { "中" };

                    if is_collapsed {
                        // 贴边小方块：只显示当前中/英状态（单击展开，拖动即展开并跟随）
                        s_text(hdc, f.status_b, lang_col, 0, 0, sw, sh, lang, T_CENTER);
                        if ai_on {
                            s_ellipse(hdc, sw - 9, 4, sw - 4, 9, rgb(16, 163, 74));
                        }
                    } else {
                        let tsf_on = st.tsf_mode_active || TSF_MODE_ACTIVE.load(Ordering::Relaxed);
                        let hover = st.status_hover;
                        let hover_bg = |r: (i32, i32), col: u32| {
                            s_round(hdc, r.0 + 1, 4, r.1 - 1, sh - 4, 6, col, col);
                        };

                        // 拖动手柄：2×3 小圆点
                        let mid = sh / 2;
                        for gx in [8, 13] {
                            for gy in [mid - 5, mid, mid + 5] {
                                s_ellipse(hdc, gx - 1, gy - 1, gx + 2, gy + 2, UI_MUTED);
                            }
                        }

                        if hover == StatusHitZone::ToggleLangBtn {
                            hover_bg(S_LANG, UI_HOVER);
                        }
                        s_text(hdc, f.status_b, lang_col, S_LANG.0, 0, S_LANG.1, sh, lang, T_CENTER);

                        if hover == StatusHitZone::TogglePunctBtn {
                            hover_bg(S_PUNCT, UI_HOVER);
                        }
                        s_text(
                            hdc,
                            f.status,
                            UI_TEXT,
                            S_PUNCT.0,
                            0,
                            S_PUNCT.1,
                            sh,
                            if st.chinese_punct_mode { "，。" } else { ", ." },
                            T_CENTER,
                        );

                        if hover == StatusHitZone::OpenSettingsBtn {
                            hover_bg(S_SET, UI_HOVER);
                        }
                        let gx = S_SET.0 + 8;
                        s_text(hdc, f.icon, UI_TEXT2, gx, 0, gx + 16, sh, "\u{E713}", T_CENTER);
                        if ai_on {
                            s_ellipse(hdc, gx + 12, mid - 9, gx + 17, mid - 4, rgb(16, 163, 74));
                        }
                        s_text(hdc, f.num, UI_TEXT, gx + 20, 0, S_SET.1 - 4, sh, "设置", T_LINE);

                        if !tsf_on {
                            if hover == StatusHitZone::ExitBtn {
                                hover_bg(S_EXIT, rgb(253, 231, 233));
                            }
                            let col = if hover == StatusHitZone::ExitBtn {
                                rgb(196, 43, 28)
                            } else {
                                UI_TEXT2
                            };
                            s_text(hdc, f.icon, col, S_EXIT.0, 0, S_EXIT.1, sh, "\u{E711}", T_CENTER);
                        }
                    }
                }
            }

            BitBlt(hdc_screen, 0, 0, sw, sh, hdc, 0, 0, SRCCOPY);
            SelectObject(hdc, old_font);
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

unsafe fn get_ctrl_text(hwnd_ctrl: isize) -> String {
    if hwnd_ctrl == 0 {
        return String::new();
    }
    let len = GetWindowTextLengthW(hwnd_ctrl);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 2];
    let read = GetWindowTextW(hwnd_ctrl, buf.as_mut_ptr(), (len + 1) as i32);
    if read <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..read as usize]).trim().to_string()
}

unsafe fn set_ctrl_text(hwnd_ctrl: isize, text: &str) {
    if hwnd_ctrl != 0 {
        let w = to_wide(text);
        SetWindowTextW(hwnd_ctrl, w.as_ptr());
    }
}

// ============================================================================
// 设置窗口：现代卡片式 UI（左侧导航 + 分页卡片 + 自绘开关 / 分段选择器 / 按钮 / 输入框）
// ============================================================================

#[repr(C)]
struct DRAWITEMSTRUCT {
    ctl_type: u32,
    ctl_id: u32,
    item_id: u32,
    item_action: u32,
    item_state: u32,
    hwnd_item: isize,
    hdc: isize,
    rc_item: RECT,
    item_data: usize,
}

#[repr(C)]
struct TRACKMOUSEEVENT {
    cb_size: u32,
    dw_flags: u32,
    hwnd_track: isize,
    dw_hover_time: u32,
}

#[link(name = "user32")]
extern "system" {
    fn DrawTextW(hdc: isize, lp_string: *const u16, n_count: i32, lp_rect: *mut RECT, u_format: u32) -> i32;
    fn GetWindowLongPtrW(hwnd: isize, n_index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: isize, n_index: i32, dw_new_long: isize) -> isize;
    fn AdjustWindowRectEx(lp_rect: *mut RECT, dw_style: u32, b_menu: i32, dw_ex_style: u32) -> i32;
    fn TrackMouseEvent(lp_event_track: *mut TRACKMOUSEEVENT) -> i32;
    fn ScreenToClient(h_wnd: isize, lp_point: *mut POINT) -> i32;
    fn IsDialogMessageW(h_dlg: isize, lp_msg: *mut MSG) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn CreatePen(i_style: i32, c_width: i32, color: u32) -> isize;
    fn RoundRect(hdc: isize, left: i32, top: i32, right: i32, bottom: i32, w: i32, h: i32) -> i32;
    fn Ellipse(hdc: isize, left: i32, top: i32, right: i32, bottom: i32) -> i32;
    fn SetBkColor(hdc: isize, color: u32) -> u32;
}

const WM_SETCURSOR: u32 = 0x0020;
const WM_DRAWITEM: u32 = 0x002B;
const WM_CTLCOLOREDIT: u32 = 0x0133;
const WM_CTLCOLORSTATIC: u32 = 0x0138;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_APP_SETTINGS_REFRESH: u32 = 0x8000 + 44;
const BS_OWNERDRAW: u32 = 0x000B;
const ES_PASSWORD: u32 = 0x0020;
const SS_NOPREFIX: u32 = 0x0080;
const EN_SETFOCUS: usize = 0x0100;
const EN_KILLFOCUS: usize = 0x0200;
const BN_CLICKED: usize = 0;
const BN_DOUBLECLICKED: usize = 5;
const GWLP_USERDATA: i32 = -21;
const ODS_SELECTED: u32 = 0x0001;
const TME_LEAVE: u32 = 0x0002;
const PS_SOLID: i32 = 0;
const IDC_HAND: usize = 32649;
const DT_CENTER: u32 = 0x0001;
const DT_VCENTER: u32 = 0x0004;
const DT_WORDBREAK: u32 = 0x0010;
const DT_SINGLELINE: u32 = 0x0020;
const DT_NOPREFIX: u32 = 0x0800;
const DT_END_ELLIPSIS: u32 = 0x8000;
/// 单行、垂直居中、超长省略
const T_LINE: u32 = DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS;
const T_CENTER: u32 = DT_SINGLELINE | DT_VCENTER | DT_CENTER;
const WS_MINIMIZEBOX: u32 = 0x0002_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;

// 配色（浅色 Fluent 风格）
const C_BG: u32 = rgb(245, 246, 248);
const C_SIDEBAR: u32 = rgb(237, 239, 243);
const C_CARD: u32 = rgb(255, 255, 255);
const C_BORDER: u32 = rgb(227, 230, 235);
const C_INPUT_BORDER: u32 = rgb(206, 211, 218);
const C_TEXT: u32 = rgb(28, 32, 38);
const C_TEXT2: u32 = rgb(92, 100, 112);
const C_MUTED: u32 = rgb(138, 145, 156);
const C_ACCENT: u32 = rgb(37, 99, 235);
const C_ACCENT_DARK: u32 = rgb(29, 78, 216);
const C_ACCENT_SOFT: u32 = rgb(234, 241, 254);
const C_TOGGLE_OFF: u32 = rgb(198, 203, 211);
const C_GREEN: u32 = rgb(22, 150, 75);
const C_GREEN_SOFT: u32 = rgb(224, 246, 233);
const C_HOVER: u32 = rgb(226, 229, 235);
const C_PRESSED: u32 = rgb(240, 242, 245);

// 布局（客户区坐标）
const SET_CW: i32 = 800;
const SET_CH: i32 = 600;
const SIDEBAR_W: i32 = 184;
const FOOTER_Y: i32 = 540;
const CARD_L: i32 = 204;
const CARD_R: i32 = 780;
const IN_L: i32 = 222;
const IN_R: i32 = 762;
const FIELD_L: i32 = 318;
const NAV_Y0: i32 = 92;
const NAV_STEP: i32 = 44;
const NAV_H: i32 = 38;
const SEG_Y: i32 = 202;
const ACT_Y: i32 = 364;
const PAGE_ALWAYS: u8 = 255;

const ID_SEG_PROVIDER_BASE: isize = 1100;
const SEG_LABELS: [&str; 5] = ["本地 Qwen", "ChatGPT", "Claude", "Gemini", "自定义"];
const EDIT_ROWS: [(isize, i32, &str); 3] = [
    (ID_EDT_API_BASE, 242, "接口地址"),
    (ID_EDT_MODEL, 282, "模型名称"),
    (ID_EDT_API_KEY, 322, "API Key"),
];
/// 侧栏导航：(Segoe MDL2 图标, 名称)
const NAV_ITEMS: [(&str, &str); 4] = [
    ("\u{E945}", "AI 模型"),
    ("\u{E8C1}", "拼音与纠错"),
    ("\u{E765}", "输入法模式"),
    ("\u{E946}", "快捷键与关于"),
];
const PAGE_HEADERS: [(&str, &str); 4] = [
    ("AI 模型", "接入本地或云端大模型对候选词做语义重排；关闭时仍由端侧轻量模型辅助选词。"),
    ("拼音与纠错", "声母、韵母或按键顺序输错时也能给出正确候选，精确拼写的结果始终优先。"),
    ("输入法模式", "选择输入法接入 Windows 的方式，两种模式共用词库与设置，可随时切换。"),
    ("快捷键与关于", "常用操作速查，以及版本与词库信息。"),
];
const SHORTCUT_ROWS: [(&str, &str); 9] = [
    ("Shift（单击）", "切换中文 / 英文"),
    ("Ctrl + Space", "切换中文 / 英文"),
    ("空格  /  1 – 9", "上屏高亮候选  /  选择对应候选"),
    ("↑  ↓", "在候选词之间移动高亮"),
    ("←  →", "在拼音音节之间移动光标，可原位改错"),
    ("Backspace / Delete", "删除光标前 / 后的字母"),
    ("Enter", "直接上屏原始字母"),
    ("Esc", "取消当前输入"),
    ("，。？！；：", "整句上屏并输入中文标点"),
];

#[derive(Clone, Copy)]
struct SettingsRes {
    f_body: isize,
    f_body_bold: isize,
    f_small: isize,
    f_heading: isize,
    f_title: isize,
    f_brand: isize,
    f_icon: isize,
    br_white: isize,
}

static SETTINGS_RES: Mutex<SettingsRes> = Mutex::new(SettingsRes {
    f_body: 0,
    f_body_bold: 0,
    f_small: 0,
    f_heading: 0,
    f_title: 0,
    f_brand: 0,
    f_icon: 0,
    br_white: 0,
});
static SETTINGS_PAGE: AtomicUsize = AtomicUsize::new(0);
static NAV_HOVER: AtomicI32 = AtomicI32::new(-1);
static PROVIDER_SEL: AtomicUsize = AtomicUsize::new(0);
static FOCUSED_EDIT: AtomicIsize = AtomicIsize::new(0);
static TSF_REGISTERED: AtomicBool = AtomicBool::new(false);
static TSF_OP_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static TEST_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static LEXICON_ENTRY_COUNT: AtomicUsize = AtomicUsize::new(0);
static SETTINGS_PAGE_CTRLS: Mutex<Vec<(isize, u8)>> = Mutex::new(Vec::new());
static SETTINGS_HAND_CTRLS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

// 内置模型卡片状态（0 = 无，1 = 0.5B，2 = 1.5B）。绘制时只读原子量，避免在 WM_DRAWITEM 中争用 GLOBAL_STATE 锁
static ACTIVE_BUILTIN_TIER: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_BUILTIN_ENABLED: AtomicBool = AtomicBool::new(false);
static DOWNLOADING_TIER: AtomicUsize = AtomicUsize::new(0);
static MODEL_TILE_HWNDS: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];

fn tier_slot(t: BuiltinModelTier) -> usize {
    match t {
        BuiltinModelTier::Qwen05B => 1,
        BuiltinModelTier::Qwen15B => 2,
    }
}

fn tile_tier(id: isize) -> Option<BuiltinModelTier> {
    match id {
        ID_BTN_PRESET_QWEN05B => Some(BuiltinModelTier::Qwen05B),
        ID_BTN_PRESET_QWEN15B => Some(BuiltinModelTier::Qwen15B),
        _ => None,
    }
}

unsafe fn invalidate_model_tiles() {
    for h in &MODEL_TILE_HWNDS {
        let hw = h.load(Ordering::Relaxed);
        if hw != 0 {
            InvalidateRect(hw, null(), 0);
        }
    }
}

/// 根据当前配置刷新「内置本地模型」卡片的选中态
unsafe fn refresh_builtin_tier_state(cfg: &ImePersistentConfig) {
    let ai = &cfg.ai_config;
    let slot = if ai.provider == AiProvider::LocalQwen && ai.api_base.contains("18434") {
        BuiltinModelTier::from_model_id(&ai.model).map(tier_slot).unwrap_or(0)
    } else {
        0
    };
    ACTIVE_BUILTIN_TIER.store(slot, Ordering::Relaxed);
    ACTIVE_BUILTIN_ENABLED.store(ai.enabled, Ordering::Relaxed);
    invalidate_model_tiles();
}

fn sres() -> SettingsRes {
    *SETTINGS_RES.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_ctrls_snapshot() -> SettingsControls {
    GLOBAL_STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|st| st.settings_ctrls))
        .unwrap_or_default()
}

fn fmt_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

unsafe fn create_settings_resources() {
    let ui = to_wide("Microsoft YaHei UI");
    let icon = to_wide("Segoe MDL2 Assets");
    let mk = |h: i32, w: i32, face: &Vec<u16>| CreateFontW(h, 0, 0, 0, w, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
    let res = SettingsRes {
        f_body: mk(-14, 400, &ui),
        f_body_bold: mk(-14, 700, &ui),
        f_small: mk(-12, 400, &ui),
        f_heading: mk(-15, 700, &ui),
        f_title: mk(-22, 700, &ui),
        f_brand: mk(-16, 700, &ui),
        f_icon: mk(-16, 400, &icon),
        br_white: CreateSolidBrush(C_CARD),
    };
    if let Ok(mut g) = SETTINGS_RES.lock() {
        *g = res;
    }
}

// ---------------------------------------------------------------------------
// 绘图辅助
// ---------------------------------------------------------------------------

unsafe fn s_round(hdc: isize, l: i32, t: i32, r: i32, b: i32, radius: i32, fill: u32, border: u32) {
    let br = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, border);
    let ob = SelectObject(hdc, br);
    let op = SelectObject(hdc, pen);
    RoundRect(hdc, l, t, r, b, radius, radius);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    DeleteObject(br);
    DeleteObject(pen);
}

unsafe fn s_ellipse(hdc: isize, l: i32, t: i32, r: i32, b: i32, fill: u32) {
    let br = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, fill);
    let ob = SelectObject(hdc, br);
    let op = SelectObject(hdc, pen);
    Ellipse(hdc, l, t, r, b);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    DeleteObject(br);
    DeleteObject(pen);
}

unsafe fn s_text(hdc: isize, font: isize, color: u32, l: i32, t: i32, r: i32, b: i32, s: &str, flags: u32) {
    let old = SelectObject(hdc, font);
    SetTextColor(hdc, color);
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut rc = RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    };
    DrawTextW(hdc, w.as_ptr(), w.len() as i32, &mut rc, flags | DT_NOPREFIX);
    SelectObject(hdc, old);
}

unsafe fn s_card(hdc: isize, t: i32, b: i32) {
    s_round(hdc, CARD_L, t, CARD_R, b, 12, C_CARD, C_BORDER);
}

/// 小号胶囊标签（如「免管理员」「当前使用」）
unsafe fn s_pill(hdc: isize, l: i32, t: i32, r: i32, b: i32, text: &str, fill: u32, fg: u32, dot: bool) {
    let res = sres();
    s_round(hdc, l, t, r, b, b - t, fill, fill);
    if dot {
        let cy = (t + b) / 2;
        s_ellipse(hdc, l + 10, cy - 3, l + 17, cy + 4, fg);
        s_text(hdc, res.f_small, fg, l + 20, t, r - 4, b, text, T_CENTER);
    } else {
        s_text(hdc, res.f_small, fg, l, t, r, b, text, T_CENTER);
    }
}

fn nav_hit(x: i32, y: i32) -> Option<usize> {
    if !(12..SIDEBAR_W - 12).contains(&x) || y < NAV_Y0 {
        return None;
    }
    let idx = ((y - NAV_Y0) / NAV_STEP) as usize;
    if idx < NAV_ITEMS.len() && (y - NAV_Y0) % NAV_STEP < NAV_H {
        Some(idx)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 窗口整体绘制（双缓冲）
// ---------------------------------------------------------------------------

unsafe fn paint_settings(hwnd: isize) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc_screen = BeginPaint(hwnd, &mut ps);
    let hdc = CreateCompatibleDC(hdc_screen);
    let hbm = CreateCompatibleBitmap(hdc_screen, SET_CW, SET_CH);
    let old_bm = SelectObject(hdc, hbm);
    SetBkMode(hdc, TRANSPARENT);
    let r = sres();
    let page = SETTINGS_PAGE.load(Ordering::Relaxed).min(NAV_ITEMS.len() - 1);
    let reg = TSF_REGISTERED.load(Ordering::Relaxed);

    fill_rect_color(hdc, 0, 0, SET_CW, SET_CH, C_BG);

    // ---- 侧栏 ----
    fill_rect_color(hdc, 0, 0, SIDEBAR_W, SET_CH, C_SIDEBAR);
    fill_rect_color(hdc, SIDEBAR_W - 1, 0, SIDEBAR_W, SET_CH, C_BORDER);
    s_round(hdc, 20, 22, 52, 54, 10, C_ACCENT, C_ACCENT);
    s_text(hdc, r.f_heading, C_CARD, 20, 22, 52, 54, "拼", T_CENTER);
    s_text(hdc, r.f_brand, C_TEXT, 62, 20, SIDEBAR_W - 8, 40, "Rust AI 拼音", T_LINE);
    s_text(hdc, r.f_small, C_MUTED, 62, 39, SIDEBAR_W - 8, 57, "设置中心", T_LINE);

    let hover = NAV_HOVER.load(Ordering::Relaxed);
    for (i, (icon, label)) in NAV_ITEMS.iter().enumerate() {
        let t = NAV_Y0 + i as i32 * NAV_STEP;
        let (l, rr, b) = (12, SIDEBAR_W - 12, t + NAV_H);
        let selected = i == page;
        if selected {
            s_round(hdc, l, t, rr, b, 10, C_CARD, C_BORDER);
            s_round(hdc, l + 5, t + 11, l + 9, b - 11, 4, C_ACCENT, C_ACCENT);
        } else if hover == i as i32 {
            s_round(hdc, l, t, rr, b, 10, C_HOVER, C_HOVER);
        }
        let icon_col = if selected { C_ACCENT } else { C_TEXT2 };
        s_text(hdc, r.f_icon, icon_col, l + 18, t, l + 42, b, icon, T_LINE);
        s_text(
            hdc,
            if selected { r.f_body_bold } else { r.f_body },
            if selected { C_TEXT } else { C_TEXT2 },
            l + 46,
            t,
            rr - 6,
            b,
            label,
            T_LINE,
        );
    }

    s_text(hdc, r.f_small, C_MUTED, 24, SET_CH - 78, SIDEBAR_W - 10, SET_CH - 60, "当前输入模式", T_LINE);
    s_ellipse(hdc, 24, SET_CH - 51, 32, SET_CH - 43, C_GREEN);
    s_text(
        hdc,
        r.f_body,
        C_TEXT,
        40,
        SET_CH - 58,
        SIDEBAR_W - 10,
        SET_CH - 36,
        if reg { "系统 TSF 输入法" } else { "全局钩子模式" },
        T_LINE,
    );
    s_text(hdc, r.f_small, C_MUTED, 24, SET_CH - 32, SIDEBAR_W - 10, SET_CH - 14, "版本 0.1.0", T_LINE);

    // ---- 页头 ----
    let (title, sub) = PAGE_HEADERS[page];
    s_text(hdc, r.f_title, C_TEXT, CARD_L, 16, CARD_R, 48, title, T_LINE);
    s_text(hdc, r.f_small, C_TEXT2, CARD_L, 50, CARD_R, 70, sub, T_LINE);

    match page {
        0 => paint_page_ai(hdc, &r),
        1 => paint_page_fuzzy(hdc, &r),
        2 => paint_page_mode(hdc, &r, reg),
        _ => paint_page_about(hdc, &r),
    }

    // ---- 底栏 ----
    fill_rect_color(hdc, SIDEBAR_W, FOOTER_Y, SET_CW, SET_CH, C_CARD);
    fill_rect_color(hdc, SIDEBAR_W, FOOTER_Y, SET_CW, FOOTER_Y + 1, C_BORDER);

    BitBlt(hdc_screen, 0, 0, SET_CW, SET_CH, hdc, 0, 0, SRCCOPY);
    SelectObject(hdc, old_bm);
    DeleteObject(hbm);
    DeleteDC(hdc);
    EndPaint(hwnd, &ps);
}

unsafe fn paint_page_ai(hdc: isize, r: &SettingsRes) {
    // 卡片 1：总开关（开关控件铺满卡片内部）
    s_card(hdc, 86, 146);

    // 卡片 2：服务配置
    s_card(hdc, 158, 408);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 168, IN_R, 192, "服务配置", T_LINE);
    s_text(hdc, r.f_body, C_TEXT2, IN_L, SEG_Y, FIELD_L - 8, SEG_Y + 32, "服务商", T_LINE);
    let focused = FOCUSED_EDIT.load(Ordering::Relaxed);
    for (id, y, label) in EDIT_ROWS {
        s_text(hdc, r.f_body, C_TEXT2, IN_L, y, FIELD_L - 8, y + 32, label, T_LINE);
        if focused == id {
            s_round(hdc, FIELD_L, y, IN_R, y + 32, 8, C_CARD, C_ACCENT);
            s_round(hdc, FIELD_L + 1, y + 1, IN_R - 1, y + 31, 7, C_CARD, C_ACCENT);
        } else {
            s_round(hdc, FIELD_L, y, IN_R, y + 32, 8, C_CARD, C_INPUT_BORDER);
        }
    }
    s_text(hdc, r.f_body, C_TEXT2, IN_L, ACT_Y, FIELD_L - 8, ACT_Y + 30, "快速填入", T_LINE);

    // 卡片 3：内置本地模型
    s_card(hdc, 420, 528);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 432, IN_L + 110, 454, "内置本地模型", T_LINE);
    s_pill(hdc, IN_L + 104, 433, IN_L + 204, 453, "免安装 Ollama", C_GREEN_SOFT, C_GREEN, false);
    s_text(
        hdc,
        r.f_small,
        C_MUTED,
        IN_L,
        456,
        IN_R,
        476,
        "自动下载 llama.cpp（Vulkan GPU 加速）与 Qwen2.5 GGUF 模型，后台运行于 127.0.0.1:18434",
        T_LINE,
    );
}

unsafe fn paint_page_fuzzy(hdc: isize, r: &SettingsRes) {
    s_card(hdc, 86, 382);
    for i in 1..5 {
        let y = 96 + i * 56 - 4;
        fill_rect_color(hdc, IN_L, y, IN_R, y + 1, C_BORDER);
    }

    s_card(hdc, 394, 474);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 410, IN_R - 130, 432, "个人输入记忆", T_LINE);
    s_text(
        hdc,
        r.f_small,
        C_MUTED,
        IN_L,
        436,
        IN_R - 130,
        456,
        "常选的字词会自动提升为首选，数据仅保存在本机 user_dict.json",
        T_LINE,
    );
}

unsafe fn paint_page_mode(hdc: isize, r: &SettingsRes, reg: bool) {
    // 卡片 A：全局钩子模式
    s_card(hdc, 86, 196);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 100, IN_L + 110, 122, "全局钩子模式", T_LINE);
    s_pill(hdc, IN_L + 104, 101, IN_L + 176, 121, "免管理员", C_ACCENT_SOFT, C_ACCENT, false);
    s_text(
        hdc,
        r.f_small,
        C_TEXT2,
        IN_L,
        130,
        IN_R - 136,
        186,
        "启动即可在所有窗口中打字，无需注册或管理员权限。通过低级键盘钩子拦截按键并模拟输入上屏；以管理员身份运行的窗口可能无法输入。",
        DT_WORDBREAK,
    );
    if !reg {
        s_pill(hdc, IN_R - 112, 125, IN_R, 157, "当前使用", C_GREEN_SOFT, C_GREEN, true);
    }

    // 卡片 B：系统 TSF 输入法
    s_card(hdc, 208, 318);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 222, IN_L + 130, 244, "系统 TSF 输入法", T_LINE);
    s_pill(hdc, IN_L + 124, 223, IN_L + 214, 243, "Win + Space", C_ACCENT_SOFT, C_ACCENT, false);
    s_text(
        hdc,
        r.f_small,
        C_TEXT2,
        IN_L,
        252,
        IN_R - 136,
        308,
        "注册为 Windows 正式输入法，出现在系统语言栏。使用 TSF 原生接口获取光标位置并直接写入文本，兼容性最好。首次注册需要一次管理员授权。",
        DT_WORDBREAK,
    );
    if reg {
        s_pill(hdc, IN_R - 112, 247, IN_R, 279, "当前使用", C_GREEN_SOFT, C_GREEN, true);
    }

    // 卡片 C：说明
    s_card(hdc, 330, 440);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 344, IN_R, 366, "切换说明", T_LINE);
    s_text(
        hdc,
        r.f_small,
        C_TEXT2,
        IN_L,
        372,
        IN_R,
        432,
        "两种模式共用同一套词库、个人输入记忆与 AI 设置。注册 TSF 后，按 Win + Space 在系统输入法列表中选择「Rust AI 拼音输入法」即可使用；此时全局钩子会自动停用，避免按键被重复处理。",
        DT_WORDBREAK,
    );
}

unsafe fn paint_page_about(hdc: isize, r: &SettingsRes) {
    s_card(hdc, 86, 206);
    s_round(hdc, IN_L, 104, IN_L + 52, 156, 14, C_ACCENT, C_ACCENT);
    s_text(hdc, r.f_title, C_CARD, IN_L, 104, IN_L + 52, 156, "拼", T_CENTER);
    let tx = IN_L + 70;
    s_text(hdc, r.f_brand, C_TEXT, tx, 100, IN_R, 124, "Rust AI 拼音输入法", T_LINE);
    s_text(hdc, r.f_small, C_TEXT2, tx, 128, IN_R, 146, "版本 0.1.0 · Rust 原生实现 · 本地优先，输入数据不出本机", T_LINE);
    let count = LEXICON_ENTRY_COUNT.load(Ordering::Relaxed);
    s_text(
        hdc,
        r.f_small,
        C_MUTED,
        tx,
        150,
        IN_R,
        168,
        &format!("词库 {} 词条 · 雾凇拼音 + 白霜词库 + 万象 Bigram + 英文词典", fmt_thousands(count)),
        T_LINE,
    );
    let data_dir = ImePersistentConfig::default_config_path()
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    s_text(hdc, r.f_small, C_MUTED, tx, 172, IN_R, 190, &format!("数据目录：{data_dir}"), T_LINE);

    s_card(hdc, 218, 528);
    s_text(hdc, r.f_heading, C_TEXT, IN_L, 230, IN_R, 254, "快捷键", T_LINE);
    for (i, (key, action)) in SHORTCUT_ROWS.iter().enumerate() {
        let y = 264 + i as i32 * 28;
        s_round(hdc, IN_L, y, IN_L + 150, y + 22, 6, C_BG, C_BORDER);
        s_text(hdc, r.f_small, C_TEXT, IN_L, y, IN_L + 150, y + 22, key, T_CENTER);
        s_text(hdc, r.f_body, C_TEXT2, IN_L + 168, y, IN_R, y + 22, action, T_LINE);
    }
}

// ---------------------------------------------------------------------------
// 自绘控件（开关 / 分段选择器 / 按钮）
// ---------------------------------------------------------------------------

fn is_toggle_id(id: isize) -> bool {
    matches!(
        id,
        ID_CHK_AI_ENABLED | ID_CHK_Z_ZH | ID_CHK_AN_ANG | ID_CHK_TYPO | ID_CHK_N_L | ID_CHK_F_H
    )
}

unsafe fn set_toggle(hwnd_ctrl: isize, on: bool) {
    if hwnd_ctrl != 0 {
        SetWindowLongPtrW(hwnd_ctrl, GWLP_USERDATA, on as isize);
        InvalidateRect(hwnd_ctrl, null(), 0);
    }
}

unsafe fn get_toggle(hwnd_ctrl: isize) -> bool {
    hwnd_ctrl != 0 && GetWindowLongPtrW(hwnd_ctrl, GWLP_USERDATA) != 0
}

unsafe fn set_provider_sel(ctrls: &SettingsControls, idx: usize) {
    PROVIDER_SEL.store(idx.min(4), Ordering::Relaxed);
    for h in ctrls.seg_provider {
        if h != 0 {
            InvalidateRect(h, null(), 0);
        }
    }
}

fn get_provider_sel() -> usize {
    PROVIDER_SEL.load(Ordering::Relaxed).min(4)
}

unsafe fn get_window_text_string(h: isize) -> String {
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 2];
    let n = GetWindowTextW(h, buf.as_mut_ptr(), len + 1);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn settings_draw_item(dis: &DRAWITEMSTRUCT) {
    let r = sres();
    let hdc = dis.hdc;
    let rc = dis.rc_item;
    let id = dis.ctl_id as isize;
    let pressed = dis.item_state & ODS_SELECTED != 0;
    let text = get_window_text_string(dis.hwnd_item);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

    SetBkMode(hdc, TRANSPARENT);
    fill_rect_struct(hdc, &rc, C_CARD);

    if is_toggle_id(id) {
        let on = get_toggle(dis.hwnd_item);
        let (title, desc) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        let ty = rc.top + if h >= 48 { 4 } else { 2 };
        s_text(hdc, r.f_body, C_TEXT, rc.left, ty, rc.right - 64, ty + 22, title, T_LINE);
        s_text(hdc, r.f_small, C_MUTED, rc.left, ty + 22, rc.right - 64, ty + 42, desc, T_LINE);
        let (sw, sh) = (44, 24);
        let sx = rc.right - sw;
        let sy = rc.top + (h - sh) / 2;
        let track = if on { C_ACCENT } else { C_TOGGLE_OFF };
        s_round(hdc, sx, sy, sx + sw, sy + sh, sh, track, track);
        let kx = if on { sx + sw - 21 } else { sx + 3 };
        s_ellipse(hdc, kx, sy + 3, kx + 18, sy + 21, C_CARD);
        return;
    }

    if (ID_SEG_PROVIDER_BASE..ID_SEG_PROVIDER_BASE + 5).contains(&id) {
        let selected = (id - ID_SEG_PROVIDER_BASE) as usize == get_provider_sel();
        let (fill, border, fg, font) = if selected {
            (C_ACCENT_SOFT, C_ACCENT, C_ACCENT, r.f_body_bold)
        } else if pressed {
            (C_PRESSED, C_INPUT_BORDER, C_TEXT, r.f_body)
        } else {
            (C_CARD, C_INPUT_BORDER, C_TEXT2, r.f_body)
        };
        s_round(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, fill, border);
        s_text(hdc, font, fg, rc.left, rc.top, rc.left + w, rc.bottom, &text, T_CENTER);
        return;
    }

    if let Some(tier) = tile_tier(id) {
        let slot = tier_slot(tier);
        let active = ACTIVE_BUILTIN_TIER.load(Ordering::Relaxed) == slot;
        let enabled = ACTIVE_BUILTIN_ENABLED.load(Ordering::Relaxed);
        let downloading = DOWNLOADING_TIER.load(Ordering::Relaxed) == slot;
        let downloaded = ManagedLocalLlm::is_tier_downloaded(tier);
        let size_txt = match tier {
            BuiltinModelTier::Qwen05B => "398 MB",
            BuiltinModelTier::Qwen15B => "986 MB",
        };

        let (fill, border, title_col, title_font) = if active || downloading {
            (C_ACCENT_SOFT, C_ACCENT, C_ACCENT, r.f_body_bold)
        } else if pressed {
            (C_PRESSED, C_INPUT_BORDER, C_TEXT, r.f_body_bold)
        } else {
            (C_CARD, C_INPUT_BORDER, C_TEXT, r.f_body_bold)
        };
        s_round(hdc, rc.left, rc.top, rc.right, rc.bottom, 10, fill, border);
        if active || downloading {
            // 选中态使用 2px 强调色描边
            s_round(hdc, rc.left + 1, rc.top + 1, rc.right - 1, rc.bottom - 1, 9, fill, border);
        }

        let (sub, sub_col) = if downloading {
            ("正在下载 / 启动…".to_string(), C_ACCENT)
        } else if active && enabled {
            ("使用中 · GPU 加速".to_string(), C_GREEN)
        } else if active {
            ("已选择 · 大模型重排未开启".to_string(), C_TEXT2)
        } else if downloaded {
            ("已下载 · 点击切换到此模型".to_string(), C_MUTED)
        } else {
            (format!("点击下载并启用 · {size_txt}"), C_MUTED)
        };

        let badge_w = if active { 30 } else { 0 };
        s_text(hdc, title_font, title_col, rc.left + 14, rc.top + 3, rc.right - 10 - badge_w, rc.top + 21, &text, T_LINE);
        s_text(hdc, r.f_small, sub_col, rc.left + 14, rc.top + 20, rc.right - 10 - badge_w, rc.bottom - 3, &sub, T_LINE);

        if active {
            // 右侧圆形勾选徽标
            let cy = (rc.top + rc.bottom) / 2;
            let cx = rc.right - 22;
            let badge_col = if enabled { C_ACCENT } else { C_TOGGLE_OFF };
            s_ellipse(hdc, cx - 10, cy - 10, cx + 10, cy + 10, badge_col);
            s_text(hdc, r.f_body_bold, C_CARD, cx - 10, cy - 10, cx + 10, cy + 10, "✓", T_CENTER);
        }
        return;
    }

    let primary = matches!(id, ID_BTN_SAVE | ID_BTN_REG_TSF);
    let (fill, border, fg) = match (primary, pressed) {
        (true, false) => (C_ACCENT, C_ACCENT, C_CARD),
        (true, true) => (C_ACCENT_DARK, C_ACCENT_DARK, C_CARD),
        (false, false) => (C_CARD, C_INPUT_BORDER, C_TEXT),
        (false, true) => (C_PRESSED, C_INPUT_BORDER, C_TEXT),
    };
    s_round(hdc, rc.left, rc.top, rc.right, rc.bottom, 8, fill, border);
    s_text(
        hdc,
        if primary { r.f_body_bold } else { r.f_body },
        fg,
        rc.left + 4,
        rc.top,
        rc.right - 4,
        rc.bottom,
        &text,
        T_CENTER,
    );
}

// ---------------------------------------------------------------------------
// 设置值 <-> 控件
// ---------------------------------------------------------------------------

unsafe fn populate_settings_controls(ctrls: &SettingsControls, cfg: &ImePersistentConfig) {
    refresh_builtin_tier_state(cfg);
    set_toggle(ctrls.chk_ai_enabled, cfg.ai_config.enabled);
    let sel_idx = AiProvider::ALL
        .iter()
        .position(|&p| p == cfg.ai_config.provider)
        .unwrap_or(0);
    set_provider_sel(ctrls, sel_idx);
    set_ctrl_text(ctrls.edt_api_base, &cfg.ai_config.api_base);
    set_ctrl_text(ctrls.edt_model, &cfg.ai_config.model);
    set_ctrl_text(ctrls.edt_api_key, &cfg.ai_config.api_key);

    let f = &cfg.fuzzy_config;
    set_toggle(ctrls.chk_z_zh, f.z_zh && f.c_ch && f.s_sh);
    set_toggle(ctrls.chk_an_ang, f.an_ang && f.en_eng && f.in_ing);
    set_toggle(ctrls.chk_typo, f.typo_correction);
    set_toggle(ctrls.chk_n_l, f.n_l);
    set_toggle(ctrls.chk_f_h, f.f_h);
}

unsafe fn read_settings_controls_into_cfg(ctrls: &SettingsControls, cfg: &mut ImePersistentConfig) {
    cfg.ai_config.enabled = get_toggle(ctrls.chk_ai_enabled);
    cfg.ai_config.provider = AiProvider::ALL[get_provider_sel()];
    cfg.ai_config.api_base = get_ctrl_text(ctrls.edt_api_base);
    cfg.ai_config.model = get_ctrl_text(ctrls.edt_model);
    cfg.ai_config.api_key = get_ctrl_text(ctrls.edt_api_key);
    cfg.ai_config.save_current_to_profile();

    let z_zh = get_toggle(ctrls.chk_z_zh);
    let an_ang = get_toggle(ctrls.chk_an_ang);
    cfg.fuzzy_config.z_zh = z_zh;
    cfg.fuzzy_config.c_ch = z_zh;
    cfg.fuzzy_config.s_sh = z_zh;
    cfg.fuzzy_config.an_ang = an_ang;
    cfg.fuzzy_config.en_eng = an_ang;
    cfg.fuzzy_config.in_ing = an_ang;
    cfg.fuzzy_config.typo_correction = get_toggle(ctrls.chk_typo);
    cfg.fuzzy_config.n_l = get_toggle(ctrls.chk_n_l);
    cfg.fuzzy_config.f_h = get_toggle(ctrls.chk_f_h);
}

/// 使用 GDI 渲染带圆角蓝底与白色「拼」字的 32 位 BGRA 图标像素（Top-Down）
unsafe fn render_ime_icon_bgra(size: i32) -> Vec<u32> {
    let bmi = BITMAPINFOHEADER {
        bi_size: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        bi_width: size,
        bi_height: -size, // Top-down DIB
        bi_planes: 1,
        bi_bit_count: 32,
        bi_compression: 0,
        bi_size_image: (size * size * 4) as u32,
        bi_x_pels_per_meter: 0,
        bi_y_pels_per_meter: 0,
        bi_clr_used: 0,
        bi_clr_important: 0,
    };
    let hdc = CreateCompatibleDC(0);
    let mut bits_ptr: *mut u32 = std::ptr::null_mut();
    let hbm = CreateDIBSection(hdc, &bmi, 0, &mut bits_ptr, 0, 0);
    if hbm == 0 || bits_ptr.is_null() {
        if hdc != 0 {
            DeleteDC(hdc);
        }
        return vec![0xFF25_63EB; (size * size) as usize];
    }
    let old_bm = SelectObject(hdc, hbm);
    let count = (size * size) as usize;
    let slice = std::slice::from_raw_parts_mut(bits_ptr, count);
    slice.fill(0);

    let font_face = to_wide("Microsoft YaHei UI");
    let font_h = -((size * 68) / 100).max(11);
    let h_font = CreateFontW(font_h, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, font_face.as_ptr());
    let old_f = SelectObject(hdc, h_font);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(255, 255, 255));
    let mut rc_txt = RECT {
        left: 0,
        top: 0,
        right: size,
        bottom: size,
    };
    let wide_pin: Vec<u16> = OsStr::new("拼").encode_wide().collect();
    DrawTextW(
        hdc,
        wide_pin.as_ptr(),
        wide_pin.len() as i32,
        &mut rc_txt,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    GdiFlush();

    let rad = (size as f32) * 0.22;
    let half = (size as f32) * 0.5;
    let box_ext = half - rad - 0.5;
    let mut out = vec![0u32; count];

    for y in 0..size {
        let fy = (y as f32 + 0.5 - half).abs() - box_ext;
        let t_grad = (y as u32 * 255) / (size.max(1) as u32);
        let bg_r = (37 * (255 - t_grad) + 29 * t_grad) / 255;
        let bg_g = (99 * (255 - t_grad) + 78 * t_grad) / 255;
        let bg_b = (235 * (255 - t_grad) + 216 * t_grad) / 255;

        for x in 0..size {
            let fx = (x as f32 + 0.5 - half).abs() - box_ext;
            let dist = (fx.max(0.0).powi(2) + fy.max(0.0).powi(2)).sqrt()
                + fx.max(fy).min(0.0)
                - rad;
            if dist >= 0.5 {
                out[(y * size + x) as usize] = 0;
                continue;
            }
            let edge_alpha = if dist <= -0.5 {
                255u32
            } else {
                ((0.5 - dist) * 255.0).clamp(0.0, 255.0) as u32
            };
            let raw = slice[(y * size + x) as usize];
            let lum = ((raw & 0xFF)
                .max((raw >> 8) & 0xFF)
                .max((raw >> 16) & 0xFF))
                .min(255);
            let r = (bg_r * (255 - lum) + 255 * lum) / 255;
            let g = (bg_g * (255 - lum) + 255 * lum) / 255;
            let b = (bg_b * (255 - lum) + 255 * lum) / 255;
            // Premultiplied / straight ARGB in BGRA little-endian u32: 0xAARRGGBB
            out[(y * size + x) as usize] = (edge_alpha << 24) | (r << 16) | (g << 8) | b;
        }
    }

    SelectObject(hdc, old_f);
    DeleteObject(h_font);
    SelectObject(hdc, old_bm);
    DeleteObject(hbm);
    DeleteDC(hdc);
    out
}

/// 创建窗口标题栏与任务栏使用的 Win32 `HICON`（蓝底圆角「拼」字图标）
unsafe fn create_ime_hicon(size: i32) -> isize {
    let pixels = render_ime_icon_bgra(size);
    let bmi = BITMAPINFOHEADER {
        bi_size: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        bi_width: size,
        bi_height: -size,
        bi_planes: 1,
        bi_bit_count: 32,
        bi_compression: 0,
        bi_size_image: (size * size * 4) as u32,
        bi_x_pels_per_meter: 0,
        bi_y_pels_per_meter: 0,
        bi_clr_used: 0,
        bi_clr_important: 0,
    };
    let hdc = CreateCompatibleDC(0);
    let mut bits_ptr: *mut u32 = std::ptr::null_mut();
    let hbm_color = CreateDIBSection(hdc, &bmi, 0, &mut bits_ptr, 0, 0);
    if hbm_color == 0 || bits_ptr.is_null() {
        if hdc != 0 {
            DeleteDC(hdc);
        }
        return 0;
    }
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits_ptr, pixels.len());
    let hbm_mask = CreateBitmap(size, size, 1, 1, null());
    let ii = ICONINFO {
        f_icon: 1,
        x_hotspot: 0,
        y_hotspot: 0,
        hbm_mask,
        hbm_color,
    };
    let h_icon = CreateIconIndirect(&ii);
    if hbm_mask != 0 {
        DeleteObject(hbm_mask);
    }
    DeleteObject(hbm_color);
    DeleteDC(hdc);
    h_icon
}

/// 内置多尺寸（16~256px）输入法图标，与构建脚本嵌入 exe 资源的图标为同一文件
const EMBEDDED_IME_ICO: &[u8] = include_bytes!("../../assets/rust_ai_ime.ico");

/// 写出供 Windows TSF 语言栏 (`Win + Space`) 使用的多尺寸 `.ico` 图标文件（内容未变化时不重复写入）
unsafe fn ensure_ime_ico_file(target_dir: &std::path::Path) -> Option<PathBuf> {
    let ico_path = target_dir.join("rust_ai_ime.ico");
    if std::fs::read(&ico_path).map(|b| b == EMBEDDED_IME_ICO).unwrap_or(false) {
        return Some(ico_path);
    }
    std::fs::write(&ico_path, EMBEDDED_IME_ICO).ok().map(|_| ico_path)
}

/// 从 exe 内嵌资源（图标 ID 1）加载指定尺寸的 HICON；若资源缺失则回退到 GDI 实时绘制
unsafe fn load_ime_hicon(size: i32) -> isize {
    const IMAGE_ICON: u32 = 1;
    let h = LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, size, size, 0);
    if h != 0 {
        h
    } else {
        create_ime_hicon(size)
    }
}

/// 查找并确保 `ime_tsf_win.dll` 及图标已部署到 `%LOCALAPPDATA%\RustAiPinyinIME\`
fn ensure_tsf_dll_installed() -> Option<PathBuf> {
    let target_dir = std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("RustAiPinyinIME"))
        .unwrap_or_else(|_| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&target_dir);
    ensure_appcontainer_dir_acl(&target_dir);
    unsafe {
        let _ = ensure_ime_ico_file(&target_dir);
    }
    let installed_dll = target_dir.join("ime_tsf_win.dll");

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("ime_tsf_win.dll"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("ime_tsf_win.dll"));
        candidates.push(cwd.join("dist").join("ime_tsf_win.dll"));
    }

    for src in candidates {
        if src.exists() {
            if src != installed_dll {
                if std::fs::copy(&src, &installed_dll).is_err() {
                    // 若旧版 DLL 已被宿主进程加载占用，在 NTFS 下可先将其重命名为 .old 再写入新 DLL
                    let old_bak = target_dir.join(format!(
                        "ime_tsf_win.{}.old",
                        std::process::id()
                    ));
                    let _ = std::fs::remove_file(&old_bak);
                    let _ = std::fs::rename(&installed_dll, &old_bak);
                    let _ = std::fs::copy(&src, &installed_dll);
                }
            }
            if installed_dll.exists() {
                return Some(installed_dll);
            }
            return Some(src);
        }
    }
    if installed_dll.exists() {
        Some(installed_dll)
    } else {
        None
    }
}

/// 切换设置页：只显示属于当前页的子控件，并按 TSF 注册状态显示对应的模式按钮
unsafe fn set_settings_page(hwnd: isize, page: usize) {
    let page = page.min(NAV_ITEMS.len() - 1);
    SETTINGS_PAGE.store(page, Ordering::Relaxed);
    let reg = TSF_REGISTERED.load(Ordering::Relaxed);
    let ctrls = settings_ctrls_snapshot();
    let list = SETTINGS_PAGE_CTRLS
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    for (h, p) in list {
        let mut visible = p == PAGE_ALWAYS || p as usize == page;
        if h == ctrls.btn_reg_tsf {
            visible &= !reg;
        }
        if h == ctrls.btn_unreg_tsf {
            visible &= reg;
        }
        ShowWindow(h, if visible { SW_SHOW } else { SW_HIDE });
    }
    InvalidateRect(hwnd, null(), 0);
}

unsafe fn show_settings_window() {
    let mut hwnd_settings = 0;
    let mut ctrls = SettingsControls::default();
    let mut cfg_snapshot = ImePersistentConfig::default();

    if let Ok(guard) = GLOBAL_STATE.lock() {
        if let Some(st) = guard.as_ref() {
            hwnd_settings = st.hwnd_settings;
            ctrls = st.settings_ctrls;
            cfg_snapshot = st.persistent_cfg.clone();
        }
    }
    if hwnd_settings != 0 {
        TSF_REGISTERED.store(is_tsf_com_registered(), Ordering::Relaxed);
        populate_settings_controls(&ctrls, &cfg_snapshot);
        set_settings_page(hwnd_settings, SETTINGS_PAGE.load(Ordering::Relaxed));
        ShowWindow(hwnd_settings, SW_SHOW);
        BringWindowToTop(hwnd_settings);
        SetForegroundWindow(hwnd_settings);
    }
}

/// 在后台线程中以管理员权限执行 regsvr32（注册或注销 TSF 组件），完成后通知设置窗口刷新
unsafe fn run_tsf_registration(hwnd: isize, ctrls: SettingsControls, register: bool) {
    if TSF_OP_IN_PROGRESS.swap(true, Ordering::SeqCst) {
        set_ctrl_text(ctrls.lbl_status, "正在处理上一次的注册请求，请稍候…");
        return;
    }
    let Some(dll_path) = ensure_tsf_dll_installed() else {
        TSF_OP_IN_PROGRESS.store(false, Ordering::SeqCst);
        set_ctrl_text(ctrls.lbl_status, "未找到 ime_tsf_win.dll，请确认安装目录中包含该组件。");
        return;
    };
    set_ctrl_text(
        ctrls.lbl_status,
        if register {
            "正在请求管理员权限注册系统输入法，请在弹出的 UAC 窗口中点击「是」…"
        } else {
            "正在请求管理员权限注销系统输入法，请在弹出的 UAC 窗口中点击「是」…"
        },
    );
    std::thread::spawn(move || unsafe {
        let dll_str = dll_path.to_string_lossy().replace('\'', "''");
        let args = if register {
            format!("'/s','\"{dll_str}\"'")
        } else {
            format!("'/u','/s','\"{dll_str}\"'")
        };
        let ps_cmd = format!("Start-Process regsvr32.exe -ArgumentList {args} -Verb RunAs -Wait");
        let _ = std::process::Command::new("powershell")
            .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
            .status();

        let registered = is_tsf_com_registered();
        if registered {
            let _ = activate_tsf_profile_for_current_user();
        }
        TSF_REGISTERED.store(registered, Ordering::Relaxed);
        TSF_MODE_ACTIVE.store(registered, Ordering::Relaxed);
        let mut hwnd_cand = 0;
        if let Ok(mut guard) = GLOBAL_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.tsf_mode_active = registered;
                hwnd_cand = st.hwnd_cand;
            }
        }
        let msg = match (register, registered) {
            (true, true) => "已注册为系统输入法。按 Win + Space 选择「Rust AI 拼音输入法」即可使用，全局钩子已自动停用。",
            (true, false) => "注册未完成（可能取消了管理员授权），继续使用全局钩子模式。",
            (false, false) => "已切换回全局钩子模式，无需切换系统输入法即可直接打字。",
            (false, true) => "注销未完成（可能取消了管理员授权），系统输入法仍处于注册状态。",
        };
        set_ctrl_text(ctrls.lbl_status, msg);
        TSF_OP_IN_PROGRESS.store(false, Ordering::SeqCst);
        PostMessageW(hwnd, WM_APP_SETTINGS_REFRESH, 0, 0);
        if hwnd_cand != 0 {
            PostMessageW(hwnd_cand, WM_APP_TSF_SYNC, 0, 0);
        }
    });
}

/// 设置窗口过程
unsafe extern "system" fn wnd_proc_settings(
    hwnd: isize,
    msg: u32,
    w_param: usize,
    l_param: isize,
) -> isize {
    match msg {
        WM_CREATE => {
            create_settings_resources();
            let r = sres();
            let h_inst = GetModuleHandleW(null());

            let mk = |cls: &str, text: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: isize, page: u8, font: isize| -> isize {
                let c_w = to_wide(cls);
                let t_w = to_wide(text);
                let hc = CreateWindowExW(0, c_w.as_ptr(), t_w.as_ptr(), WS_CHILD | style, x, y, w, h, hwnd, id, h_inst, null());
                SendMessageW(hc, WM_SETFONT, font as usize, 0);
                if let Ok(mut g) = SETTINGS_PAGE_CTRLS.lock() {
                    g.push((hc, page));
                }
                if cls == "BUTTON" {
                    if let Ok(mut g) = SETTINGS_HAND_CTRLS.lock() {
                        g.push(hc);
                    }
                }
                hc
            };
            let btn = |text: &str, x: i32, y: i32, w: i32, h: i32, id: isize, page: u8| -> isize {
                mk("BUTTON", text, BS_OWNERDRAW | WS_TABSTOP, x, y, w, h, id, page, r.f_body)
            };
            let edit = |y: i32, id: isize, extra: u32| -> isize {
                mk("EDIT", "", ES_AUTOHSCROLL | WS_TABSTOP | extra, FIELD_L + 10, y + 6, IN_R - FIELD_L - 20, 20, id, 0, r.f_body)
            };

            // ---- 第 1 页：AI 模型 ----
            let chk_ai_enabled = btn(
                "启用大模型智能选词重排\n在后台异步运行，不阻塞打字；端侧轻量模型始终开启",
                IN_L,
                94,
                IN_R - IN_L,
                44,
                ID_CHK_AI_ENABLED,
                0,
            );
            let mut seg_provider = [0isize; 5];
            for (i, label) in SEG_LABELS.iter().enumerate() {
                seg_provider[i] = btn(label, FIELD_L + i as i32 * 90, SEG_Y, 84, 32, ID_SEG_PROVIDER_BASE + i as isize, 0);
            }
            let edt_api_base = edit(EDIT_ROWS[0].1, ID_EDT_API_BASE, 0);
            let edt_model = edit(EDIT_ROWS[1].1, ID_EDT_MODEL, 0);
            let edt_api_key = edit(EDIT_ROWS[2].1, ID_EDT_API_KEY, ES_PASSWORD);
            btn("DeepSeek", FIELD_L, ACT_Y, 84, 30, ID_BTN_PRESET_DEEPSEEK, 0);
            btn("硅基流动", FIELD_L + 90, ACT_Y, 84, 30, ID_BTN_PRESET_SILICON, 0);
            btn("探测本机服务", IN_R - 232, ACT_Y, 112, 30, ID_BTN_PROBE_LOCAL, 0);
            btn("测试连接", IN_R - 112, ACT_Y, 112, 30, ID_BTN_TEST_CONN, 0);
            let tile_05 = btn("Qwen2.5-0.5B · 极速版", IN_L, 480, 264, 40, ID_BTN_PRESET_QWEN05B, 0);
            let tile_15 = btn("Qwen2.5-1.5B · 推荐", IN_L + 276, 480, 264, 40, ID_BTN_PRESET_QWEN15B, 0);
            MODEL_TILE_HWNDS[0].store(tile_05, Ordering::Relaxed);
            MODEL_TILE_HWNDS[1].store(tile_15, Ordering::Relaxed);

            // ---- 第 2 页：拼音与纠错 ----
            let fuzzy_rows: [(&str, isize); 5] = [
                ("平翘舌模糊（z / c / s 与 zh / ch / sh）\n例：zidao → 知道，sihou → 时候", ID_CHK_Z_ZH),
                ("前后鼻音模糊（an / en / in 与 ang / eng / ing）\n例：zengde → 真的，cenxu → 程序", ID_CHK_AN_ANG),
                ("常见按键手误纠正\n自动纠正 ign → ing、agn → ang、uei → ui 等，例：beijign → 北京", ID_CHK_TYPO),
                ("鼻边音模糊（n 与 l）\n方言区可选；默认关闭，避免输入 ni 时混入「李」", ID_CHK_N_L),
                ("唇齿音 / 舌根音模糊（f 与 h）\n方言区可选；默认关闭", ID_CHK_F_H),
            ];
            let mut fuzzy_hwnds = [0isize; 5];
            for (i, (text, id)) in fuzzy_rows.iter().enumerate() {
                fuzzy_hwnds[i] = btn(text, IN_L, 96 + i as i32 * 56, IN_R - IN_L, 48, *id, 1);
            }
            btn("清空记忆", IN_R - 112, 418, 112, 32, ID_BTN_CLEAR_MEM, 1);

            // ---- 第 3 页：输入法模式 ----
            let btn_unreg_tsf = btn("切换到此模式", IN_R - 112, 125, 112, 32, ID_BTN_UNREG_TSF, 2);
            let btn_reg_tsf = btn("注册并启用", IN_R - 112, 247, 112, 32, ID_BTN_REG_TSF, 2);

            // ---- 底栏（所有页面共享） ----
            let lbl_status = mk(
                "STATIC",
                "修改设置后，点击「保存设置」立即生效。",
                SS_NOPREFIX | 0x2000 /* SS_EDITCONTROL */ | 0x4000 /* SS_ENDELLIPSIS */,
                CARD_L,
                FOOTER_Y + 11,
                CARD_R - 200 - CARD_L - 14,
                40,
                ID_LBL_STATUS,
                PAGE_ALWAYS,
                r.f_small,
            );
            btn("关闭", CARD_R - 200, FOOTER_Y + 14, 96, 32, ID_BTN_CLOSE, PAGE_ALWAYS);
            btn("保存设置", CARD_R - 96, FOOTER_Y + 14, 96, 32, ID_BTN_SAVE, PAGE_ALWAYS);

            let ctrls = SettingsControls {
                chk_ai_enabled,
                seg_provider,
                edt_api_base,
                edt_model,
                edt_api_key,
                chk_z_zh: fuzzy_hwnds[0],
                chk_an_ang: fuzzy_hwnds[1],
                chk_typo: fuzzy_hwnds[2],
                chk_n_l: fuzzy_hwnds[3],
                chk_f_h: fuzzy_hwnds[4],
                lbl_status,
                btn_reg_tsf,
                btn_unreg_tsf,
            };

            let mut init_cfg = ImePersistentConfig::default();
            if let Ok(mut guard) = GLOBAL_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.settings_ctrls = ctrls;
                    init_cfg = st.persistent_cfg.clone();
                }
            }
            TSF_REGISTERED.store(is_tsf_com_registered(), Ordering::Relaxed);
            populate_settings_controls(&ctrls, &init_cfg);
            set_settings_page(hwnd, 0);
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            paint_settings(hwnd);
            0
        }
        WM_DRAWITEM => {
            if l_param != 0 {
                settings_draw_item(&*(l_param as *const DRAWITEMSTRUCT));
            }
            1
        }
        WM_CTLCOLORSTATIC => {
            SetBkColor(w_param as isize, C_CARD);
            SetTextColor(w_param as isize, C_TEXT2);
            sres().br_white
        }
        WM_CTLCOLOREDIT => {
            SetBkColor(w_param as isize, C_CARD);
            SetTextColor(w_param as isize, C_TEXT);
            sres().br_white
        }
        WM_SETCURSOR => {
            let child = w_param as isize;
            let over_button = SETTINGS_HAND_CTRLS
                .lock()
                .map(|g| g.contains(&child))
                .unwrap_or(false);
            let mut over_nav = false;
            if child == hwnd {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                ScreenToClient(hwnd, &mut pt);
                over_nav = nav_hit(pt.x, pt.y).is_some();
            }
            if over_button || over_nav {
                SetCursor(LoadCursorW(0, IDC_HAND as *const u16));
                1
            } else {
                DefWindowProcW(hwnd, msg, w_param, l_param)
            }
        }
        WM_MOUSEMOVE => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            let hover = nav_hit(x, y).map(|i| i as i32).unwrap_or(-1);
            if NAV_HOVER.swap(hover, Ordering::Relaxed) != hover {
                let rc = RECT {
                    left: 0,
                    top: 0,
                    right: SIDEBAR_W,
                    bottom: SET_CH,
                };
                InvalidateRect(hwnd, &rc, 0);
            }
            let mut tme = TRACKMOUSEEVENT {
                cb_size: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dw_flags: TME_LEAVE,
                hwnd_track: hwnd,
                dw_hover_time: 0,
            };
            TrackMouseEvent(&mut tme);
            0
        }
        WM_MOUSELEAVE => {
            if NAV_HOVER.swap(-1, Ordering::Relaxed) != -1 {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_LBUTTONDOWN => {
            let x = (l_param & 0xFFFF) as i16 as i32;
            let y = ((l_param >> 16) & 0xFFFF) as i16 as i32;
            if let Some(idx) = nav_hit(x, y) {
                if idx != SETTINGS_PAGE.load(Ordering::Relaxed) {
                    set_settings_page(hwnd, idx);
                }
            }
            0
        }
        WM_APP_SETTINGS_REFRESH => {
            set_settings_page(hwnd, SETTINGS_PAGE.load(Ordering::Relaxed));
            0
        }
        WM_COMMAND => {
            let id = (w_param & 0xFFFF) as isize;
            let notify_code = (w_param >> 16) & 0xFFFF;
            let ctrls = settings_ctrls_snapshot();

            // 输入框获得 / 失去焦点：刷新高亮描边
            if matches!(id, ID_EDT_API_BASE | ID_EDT_MODEL | ID_EDT_API_KEY) {
                if notify_code == EN_SETFOCUS {
                    FOCUSED_EDIT.store(id, Ordering::Relaxed);
                    InvalidateRect(hwnd, null(), 0);
                } else if notify_code == EN_KILLFOCUS {
                    let _ = FOCUSED_EDIT.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
                    InvalidateRect(hwnd, null(), 0);
                }
                return 0;
            }

            // 开关：单击 / 快速连击都切换一次
            if is_toggle_id(id) {
                if notify_code == BN_CLICKED || notify_code == BN_DOUBLECLICKED {
                    let h = l_param;
                    set_toggle(h, !get_toggle(h));
                }
                return 0;
            }

            if notify_code != BN_CLICKED {
                return 0;
            }

            // 服务商分段选择器
            if (ID_SEG_PROVIDER_BASE..ID_SEG_PROVIDER_BASE + 5).contains(&id) {
                let idx = (id - ID_SEG_PROVIDER_BASE) as usize;
                if idx == get_provider_sel() {
                    return 0;
                }
                let next_prov = AiProvider::ALL[idx];
                let cur_base = get_ctrl_text(ctrls.edt_api_base);
                let cur_model = get_ctrl_text(ctrls.edt_model);
                let cur_key = get_ctrl_text(ctrls.edt_api_key);
                let mut next_cfg = None;
                if let Ok(mut guard) = GLOBAL_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.persistent_cfg.ai_config.api_base = cur_base;
                        st.persistent_cfg.ai_config.model = cur_model;
                        st.persistent_cfg.ai_config.api_key = cur_key;
                        st.persistent_cfg.ai_config.switch_provider(next_prov);
                        next_cfg = Some((
                            st.persistent_cfg.ai_config.api_base.clone(),
                            st.persistent_cfg.ai_config.model.clone(),
                            st.persistent_cfg.ai_config.api_key.clone(),
                        ));
                    }
                }
                set_provider_sel(&ctrls, idx);
                if let Some((base_s, model_s, key_s)) = next_cfg {
                    set_ctrl_text(ctrls.edt_api_base, &base_s);
                    set_ctrl_text(ctrls.edt_model, &model_s);
                    set_ctrl_text(ctrls.edt_api_key, &key_s);
                }
                set_ctrl_text(
                    ctrls.lbl_status,
                    &format!(
                        "已切换到「{}」，确认接口地址、模型与 Key 后点击「保存设置」。",
                        next_prov.display_name()
                    ),
                );
                return 0;
            }

            match id {
                ID_BTN_PRESET_QWEN15B | ID_BTN_PRESET_QWEN05B => {
                    let tier = if id == ID_BTN_PRESET_QWEN15B {
                        BuiltinModelTier::Qwen15B
                    } else {
                        BuiltinModelTier::Qwen05B
                    };
                    if ACTIVE_BUILTIN_TIER.load(Ordering::Relaxed) == tier_slot(tier)
                        && ACTIVE_BUILTIN_ENABLED.load(Ordering::Relaxed)
                        && ManagedLocalLlm::is_server_running()
                    {
                        set_ctrl_text(
                            ctrls.lbl_status,
                            &format!("当前正在使用 {}，无需重复启用。", tier.short_name()),
                        );
                        return 0;
                    }
                    if DOWNLOAD_IN_PROGRESS.swap(true, Ordering::SeqCst) {
                        set_ctrl_text(ctrls.lbl_status, "内置模型正在后台下载或启动，请稍候…");
                        return 0;
                    }
                    DOWNLOADING_TIER.store(tier_slot(tier), Ordering::Relaxed);
                    invalidate_model_tiles();
                    set_provider_sel(&ctrls, 0);
                    set_ctrl_text(ctrls.edt_api_base, MANAGED_LLM_BASE_URL);
                    set_ctrl_text(ctrls.edt_model, tier.model_id());
                    set_toggle(ctrls.chk_ai_enabled, true);

                    let lbl = ctrls.lbl_status;
                    std::thread::spawn(move || {
                        let res = ManagedLocalLlm::download_and_start(tier, |m| unsafe {
                            set_ctrl_text(lbl, m);
                        });
                        DOWNLOAD_IN_PROGRESS.store(false, Ordering::SeqCst);
                        DOWNLOADING_TIER.store(0, Ordering::Relaxed);
                        unsafe {
                            match res {
                                Ok(()) => {
                                    if let Ok(mut guard) = GLOBAL_STATE.lock() {
                                        if let Some(st) = guard.as_mut() {
                                            st.persistent_cfg.ai_config.provider = AiProvider::LocalQwen;
                                            st.persistent_cfg.ai_config.api_base = MANAGED_LLM_BASE_URL.to_string();
                                            st.persistent_cfg.ai_config.model = tier.model_id().to_string();
                                            st.persistent_cfg.ai_config.enabled = true;
                                            st.persistent_cfg.ai_config.save_current_to_profile();
                                            st.session.apply_persistent_config(&st.persistent_cfg);
                                            let _ = st.persistent_cfg.save();
                                        }
                                    }
                                    ACTIVE_BUILTIN_TIER.store(tier_slot(tier), Ordering::Relaxed);
                                    ACTIVE_BUILTIN_ENABLED.store(true, Ordering::Relaxed);
                                    invalidate_model_tiles();
                                    sync_ui_windows();
                                }
                                Err(e) => {
                                    invalidate_model_tiles();
                                    set_ctrl_text(lbl, &format!("内置模型下载或启动失败：{e}"));
                                }
                            }
                        }
                    });
                }
                ID_BTN_PRESET_DEEPSEEK => {
                    set_provider_sel(&ctrls, 4);
                    set_ctrl_text(ctrls.edt_api_base, "https://api.deepseek.com/v1");
                    set_ctrl_text(ctrls.edt_model, "deepseek-chat");
                    set_ctrl_text(ctrls.lbl_status, "已填入 DeepSeek 接口，请在「API Key」中填写你的密钥。");
                }
                ID_BTN_PRESET_SILICON => {
                    set_provider_sel(&ctrls, 4);
                    set_ctrl_text(ctrls.edt_api_base, "https://api.siliconflow.cn/v1");
                    set_ctrl_text(ctrls.edt_model, "Qwen/Qwen2.5-7B-Instruct");
                    set_ctrl_text(ctrls.lbl_status, "已填入硅基流动 Qwen2.5-7B 接口，请在「API Key」中填写你的密钥。");
                }
                ID_BTN_PROBE_LOCAL => {
                    set_ctrl_text(ctrls.lbl_status, "正在探测本机 Ollama / llama.cpp 服务…");
                    std::thread::spawn(move || unsafe {
                        let endpoints = OpenAiCompatibleClient::probe_local_endpoints();
                        let mut summary = Vec::new();
                        let mut found_url = None;
                        for ep in endpoints {
                            if ep.online {
                                summary.push(format!("{} 在线", ep.name));
                                if found_url.is_none() {
                                    found_url = Some(ep.base_url);
                                }
                            } else {
                                summary.push(format!("{} 未启动", ep.name));
                            }
                        }
                        if let Some(url) = found_url {
                            set_ctrl_text(ctrls.edt_api_base, &url);
                            set_toggle(ctrls.chk_ai_enabled, true);
                        }
                        set_ctrl_text(ctrls.lbl_status, &summary.join("  ·  "));
                    });
                }
                ID_BTN_TEST_CONN => {
                    if TEST_IN_PROGRESS.swap(true, Ordering::SeqCst) {
                        return 0;
                    }
                    set_ctrl_text(ctrls.lbl_status, "正在测试大模型连接与推理延迟…");
                    let mut temp_cfg = GLOBAL_STATE
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(|st| st.persistent_cfg.clone()))
                        .unwrap_or_default();
                    read_settings_controls_into_cfg(&ctrls, &mut temp_cfg);
                    let ai_cfg = temp_cfg.ai_config;
                    std::thread::spawn(move || unsafe {
                        let client = OpenAiCompatibleClient::new(ai_cfg);
                        let text = match client.test_connection() {
                            Ok((reply, elapsed)) => format!(
                                "连接成功，耗时 {:.0} ms。测试语境「微积分傅里叶 + jishu」→ 模型返回「{}」",
                                elapsed.as_secs_f64() * 1000.0,
                                reply
                            ),
                            Err(e) => format!("连接失败：{e}"),
                        };
                        TEST_IN_PROGRESS.store(false, Ordering::SeqCst);
                        set_ctrl_text(ctrls.lbl_status, &text);
                    });
                }
                ID_BTN_REG_TSF => run_tsf_registration(hwnd, ctrls, true),
                ID_BTN_UNREG_TSF => run_tsf_registration(hwnd, ctrls, false),
                ID_BTN_CLEAR_MEM => {
                    if let Ok(mut guard) = GLOBAL_STATE.lock() {
                        if let Some(st) = guard.as_mut() {
                            st.session.core.user_dict = ime_core::UserDict::new();
                            let _ = std::fs::remove_file(ime_core::UserDict::default_storage_path());
                        }
                    }
                    set_ctrl_text(ctrls.lbl_status, "已清空个人输入记忆（词频与搭配习惯）。");
                }
                ID_BTN_SAVE => {
                    let mut updated_cfg = GLOBAL_STATE
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(|st| st.persistent_cfg.clone()))
                        .unwrap_or_default();
                    read_settings_controls_into_cfg(&ctrls, &mut updated_cfg);
                    let mut saved_msg = None;
                    if let Ok(mut guard) = GLOBAL_STATE.lock() {
                        if let Some(st) = guard.as_mut() {
                            st.persistent_cfg = updated_cfg;
                            st.session.apply_persistent_config(&st.persistent_cfg);
                            refresh_builtin_tier_state(&st.persistent_cfg);
                            let ok = st.persistent_cfg.save().is_ok();
                            saved_msg = Some(if ok {
                                format!(
                                    "设置已保存并生效。当前：{} · {}，大模型重排{}。",
                                    st.persistent_cfg.ai_config.provider.short_tag(),
                                    st.persistent_cfg.ai_config.model,
                                    if st.persistent_cfg.ai_config.enabled { "已开启" } else { "已关闭" }
                                )
                            } else {
                                "设置已生效，但写入 ime_config.json 失败，请检查磁盘权限。".to_string()
                            });
                        }
                    }
                    if let Some(m) = saved_msg {
                        set_ctrl_text(ctrls.lbl_status, &m);
                    }
                    sync_ui_windows();
                }
                // IDCANCEL（Esc）与「关闭」按钮
                2 | ID_BTN_CLOSE => {
                    ShowWindow(hwnd, SW_HIDE);
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            ShowWindow(hwnd, SW_HIDE);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let is_self_test = args.iter().any(|a| a == "--self-test");

    // 单实例保护：在加载 57 万词库之前立即检查命名互斥量，彻底杜绝多进程与双状态栏重叠
    if !is_self_test {
        unsafe {
            let default_desk = to_wide("Default");
            let h_desk = OpenDesktopW(default_desk.as_ptr(), 0, 0, 0x01FF);
            if h_desk != 0 {
                SetThreadDesktop(h_desk);
            }

            let mutex_name = to_wide("Local\\RustAiPinyinImeGlobalServerSingletonMutex_v1");
            let h_mutex = CreateMutexW(null(), 1, mutex_name.as_ptr());
            if h_mutex != 0 && GetLastError() == 183 /* ERROR_ALREADY_EXISTS */ {
                if args.iter().any(|a| a == "--open-settings") {
                    let cls = to_wide("RustAiGlobalImeStatusPillV1");
                    let hwnd_existing = FindWindowW(cls.as_ptr(), null());
                    if hwnd_existing != 0 {
                        PostMessageW(hwnd_existing, WM_APP_OPEN_SETTINGS, 0, 0);
                    }
                }
                return Ok(());
            }
        }
    }

    // 词库查找根目录：可执行文件所在目录（`load_or_build_default` 还会依次尝试其上级目录、当前工作目录与 %LOCALAPPDATA%\RustAiPinyinIME）
    let fallback_root = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let lexicon = CompiledLexicon::load_or_build_default(&fallback_root)?;
    let entry_count = lexicon.entries.len();
    LEXICON_ENTRY_COUNT.store(entry_count, Ordering::Relaxed);
    let persistent_cfg = ImePersistentConfig::load_or_default();
    let mut core = ImeCoreEngine::new(lexicon);
    core.fuzzy_config = persistent_cfg.fuzzy_config.clone();
    let ai = DualModeAiOrchestrator::new(persistent_cfg.ai_config.clone());
    if !is_self_test {
        // 仅真实运行的全局服务读写个人记忆词库，并在已配置内置本地模型时自动拉起后台推理引擎
        core.enable_persistence(ime_core::UserDict::default_storage_path());
        ManagedLocalLlm::auto_start_if_configured(
            &persistent_cfg.ai_config.api_base,
            &persistent_cfg.ai_config.model,
            persistent_cfg.ai_config.enabled,
        );
    }
    let mut session = TsfKeySession::new(core, ai);
    // 全局版关闭演示用的 Ghost Text 续写，避免在真实应用里劫持 Tab 键（表单切换、代码缩进等）
    session.ghost_text_enabled = false;

    if is_self_test {
        println!("=== [ime-global-service Self-Test] ===");
        println!("  - 全局词库加载词条数: {}", entry_count);
        assert!(entry_count > 500_000);
        return Ok(());
    }

    unsafe {
        let default_desk = to_wide("Default");
        let h_desk = OpenDesktopW(default_desk.as_ptr(), 0, 0, 0x01FF);
        if h_desk != 0 {
            SetThreadDesktop(h_desk);
        }

        let tsf_registered = is_tsf_com_registered();
        TSF_MODE_ACTIVE.store(tsf_registered, Ordering::Relaxed);
        if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
            let dir = PathBuf::from(appdata).join("RustAiPinyinIME");
            let _ = std::fs::create_dir_all(&dir);
            ensure_appcontainer_dir_acl(&dir);
            let _ = ensure_ime_ico_file(&dir);
        }
        if tsf_registered {
            let _ = activate_tsf_profile_for_current_user();
        }

        let init_status_w = current_status_width();
        let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1280);
        let screen_h = GetSystemMetrics(SM_CYSCREEN).max(800);
        let wa = get_work_area();
        let init_status_pos = clamp_to_work_area(
            wa.right - init_status_w - 20,
            wa.bottom - STATUS_H - 12,
            init_status_w,
            STATUS_H,
        );
        let init_cand_pos = clamp_to_work_area(
            (screen_w - CAND_W) / 2,
            wa.bottom - CAND_H - 64,
            CAND_W,
            CAND_H,
        );

        *GLOBAL_STATE.lock().unwrap() = Some(GlobalImeState {
            session,
            persistent_cfg,
            chinese_punct_mode: true,
            shift_tap_armed: false,
            tsf_mode_active: tsf_registered,
            eaten_vks: [false; 256],
            hwnd_cand: 0,
            hwnd_status: 0,
            hwnd_settings: 0,
            settings_ctrls: SettingsControls::default(),
            cand_hover: CandHitZone::None,
            status_hover: StatusHitZone::None,
            cand_pos: init_cand_pos,
            status_pos: init_status_pos,
            dragging_status: None,
            status_drag_moved: false,
            dragging_cand: None,
            last_fg_exe: String::new(),
            pending_tsf_commits: Vec::new(),
        });

        let h_instance = GetModuleHandleW(null());
        let h_cursor = LoadCursorW(0, 32512 as *const u16);
        let h_icon_big = load_ime_hicon(GetSystemMetrics(11 /* SM_CXICON */).max(32));
        let h_icon_sm = load_ime_hicon(GetSystemMetrics(49 /* SM_CXSMICON */).max(16));

        let cand_class = to_wide("RustAiGlobalImeCandOverlayV1");
        let wc_cand = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 3,
            lpfn_wnd_proc: wnd_proc_cand,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: h_icon_big,
            h_cursor,
            hbr_background: CreateSolidBrush(rgb(248, 250, 253)),
            lpsz_menu_name: null(),
            lpsz_class_name: cand_class.as_ptr(),
            h_icon_sm,
        };
        RegisterClassExW(&wc_cand);

        let status_class = to_wide("RustAiGlobalImeStatusPillV1");
        let wc_status = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 3,
            lpfn_wnd_proc: wnd_proc_status,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: h_icon_big,
            h_cursor,
            hbr_background: CreateSolidBrush(rgb(248, 250, 253)),
            lpsz_menu_name: null(),
            lpsz_class_name: status_class.as_ptr(),
            h_icon_sm,
        };
        RegisterClassExW(&wc_status);

        let settings_class = to_wide("RustAiGlobalImeSettingsWinV1");
        let wc_settings = WNDCLASSEXW {
            cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 3,
            lpfn_wnd_proc: wnd_proc_settings,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance,
            h_icon: h_icon_big,
            h_cursor,
            hbr_background: CreateSolidBrush(C_BG),
            lpsz_menu_name: null(),
            lpsz_class_name: settings_class.as_ptr(),
            h_icon_sm,
        };
        RegisterClassExW(&wc_settings);

        let hwnd_cand = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            cand_class.as_ptr(),
            to_wide("RustAI拼音候选窗").as_ptr(),
            WS_POPUP,
            init_cand_pos.0,
            init_cand_pos.1,
            CAND_W,
            CAND_H,
            0,
            0,
            h_instance,
            null(),
        );
        enable_win11_round_corners(hwnd_cand);

        let hwnd_status = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            status_class.as_ptr(),
            to_wide("RustAI拼音状态栏").as_ptr(),
            WS_POPUP | WS_VISIBLE,
            init_status_pos.0,
            init_status_pos.1,
            init_status_w,
            STATUS_H,
            0,
            0,
            h_instance,
            null(),
        );
        enable_win11_round_corners(hwnd_status);

        let settings_style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut wr = RECT {
            left: 0,
            top: 0,
            right: SET_CW,
            bottom: SET_CH,
        };
        AdjustWindowRectEx(&mut wr, settings_style, 0, WS_EX_APPWINDOW);
        let (set_ow, set_oh) = (wr.right - wr.left, wr.bottom - wr.top);
        let hwnd_settings = CreateWindowExW(
            WS_EX_APPWINDOW,
            settings_class.as_ptr(),
            to_wide("Rust AI 拼音输入法 · 设置").as_ptr(),
            settings_style,
            (screen_w - set_ow) / 2,
            (screen_h - set_oh) / 2,
            set_ow,
            set_oh,
            0,
            0,
            h_instance,
            null(),
        );
        if h_icon_big != 0 {
            SendMessageW(hwnd_settings, WM_SETICON, 1, h_icon_big);
        }
        if h_icon_sm != 0 {
            SendMessageW(hwnd_settings, WM_SETICON, 0, h_icon_sm);
        }

        if let Ok(mut guard) = GLOBAL_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.hwnd_cand = hwnd_cand;
                st.hwnd_status = hwnd_status;
                st.hwnd_settings = hwnd_settings;
                st.session.ai.set_async_notify_callback(Arc::new(move || {
                    PostMessageW(hwnd_cand, WM_APP_AI_READY, 0, 0);
                }));
            }
        }

        ShowWindow(hwnd_status, SW_SHOWNOACTIVATE);
        SetTimer(hwnd_status, STATUS_VIS_TIMER_ID, 250, null());
        spawn_tsf_ipc_server(hwnd_cand);
        if args.iter().any(|a| a == "--open-settings") {
            show_settings_window();
        }

        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, low_level_keyboard_proc, h_instance, 0);
        let mouse_hook = SetWindowsHookExW(14 /* WH_MOUSE_LL */, low_level_mouse_proc, h_instance, 0);
        LL_HOOKS_INSTALLED.store(hook != 0 && mouse_hook != 0, Ordering::Relaxed);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            // 让设置窗口支持 Tab 切换焦点、Esc 关闭等标准对话框键盘行为
            if hwnd_settings != 0 && IsDialogMessageW(hwnd_settings, &mut msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if hook != 0 {
            UnhookWindowsHookEx(hook);
        }
        if mouse_hook != 0 {
            UnhookWindowsHookEx(mouse_hook);
        }
        ManagedLocalLlm::stop_server();
    }
    Ok(())
}
