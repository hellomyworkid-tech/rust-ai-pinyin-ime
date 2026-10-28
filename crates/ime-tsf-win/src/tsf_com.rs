//! `tsf_com`: Windows TSF (Text Services Framework) 原生 COM 文本服务实现
//!
//! 架构（对齐 Rime 小狼毫 Weasel / 搜狗输入法客户端-服务端架构）：
//! 1. **进程内轻量 COM 客户端 (`ime_tsf_win.dll`)**：
//!    - 实现 `IClassFactory`、`ITfTextInputProcessorEx`、`ITfKeyEventSink`、`ITfEditSession` 原生 COM 接口虚表；
//!    - 实现 `DllRegisterServer` / `DllUnregisterServer`：写入 `HKLM\SOFTWARE\Classes\CLSID` 注册表项，
//!      并调用系统 `ITfInputProcessorProfiles`、`ITfInputProcessorProfileMgr` 与 `ITfCategoryMgr`
//!      将「Rust AI 拼音输入法」正式注册到 Windows 语言栏（`Win + Space` 可见、可选，兼容桌面与 UWP/Immersive 应用）。
//! 2. **原生光标定位 (`ITfContextView::GetTextExt`) 与原生文本上屏 (`ITfInsertAtSelection`)**：
//!    - 每次按键通过只读/读写 `ITfEditSession` 调用 `ITfContextView::GetTextExt` 获取目标应用真实的输入插入符屏幕像素坐标；
//!    - 通过本地回环 IPC (`127.0.0.1:18435`) 请求中央引擎服务 (`RustAI拼音输入法_全局版.exe`) 执行 57 万词库 + AI 重排并渲染候选窗；
//!    - 选词完成时通过 `ITfInsertAtSelection::InsertTextAtSelection` 将文本直接写入目标应用文档，告别 `SendInput` 模拟按键！

use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const TSF_IPC_PORT: u16 = 18435;

/// TSF 客户端与中央守护服务之间的 IPC 请求包
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsfIpcRequest {
    /// `"test_key_down"` | `"key_down"` | `"key_up"` | `"activate"` | `"deactivate"`
    pub op: String,
    pub vk: u32,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub caret_x: i32,
    pub caret_y: i32,
    pub exe_name: String,
    /// 发起请求的宿主进程 ID（用于判断前台应用当前是否正在使用本输入法；旧版 DLL 不带此字段时为 0）
    #[serde(default)]
    pub pid: u32,
}

/// TSF 中央守护服务返回的响应包
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TsfIpcResponse {
    pub eaten: bool,
    pub commit_text: String,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
pub const S_FALSE: HRESULT = 1;
pub const E_NOINTERFACE: HRESULT = -2147467262i32; // 0x80004002
pub const E_POINTER: HRESULT = -2147467261i32;     // 0x80004003
pub const E_FAIL: HRESULT = -2147467259i32;        // 0x80004005
pub const CLASS_E_NOAGGREGATION: HRESULT = -2147221232i32; // 0x80040110
pub const CLASS_E_CLASSNOTAVAILABLE: HRESULT = -2147221231i32; // 0x80040111

/// 输入法 CLSID: `{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}`
pub const CLSID_RUST_AI_IME: GUID = GUID {
    data1: 0x8E7A_9C12,
    data2: 0x4F5B,
    data3: 0x4D8A,
    data4: [0x9E, 0x2C, 0x1B, 0x3D, 0x5F, 0x7A, 0x9E, 0x01],
};

/// 输入法语言配置 Profile GUID: `{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}`
pub const GUID_RUST_AI_PROFILE: GUID = GUID {
    data1: 0x9F8B_0D23,
    data2: 0x5A6C,
    data3: 0x4E9B,
    data4: [0xAF, 0x3D, 0x2C, 0x4E, 0x6F, 0x8B, 0x0A, 0x12],
};

// 标准 COM 与 Windows TSF (`msctf.h`) 接口 IID / CLSID
pub const IID_IUNKNOWN: GUID = GUID {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

pub const IID_ICLASS_FACTORY: GUID = GUID {
    data1: 0x0000_0001,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

pub const IID_ITF_TEXT_INPUT_PROCESSOR: GUID = GUID {
    data1: 0xAA80_E7F7,
    data2: 0x2021,
    data3: 0x11D2,
    data4: [0x93, 0xE0, 0x00, 0x60, 0xB0, 0x67, 0xB8, 0x6E],
};

pub const IID_ITF_TEXT_INPUT_PROCESSOR_EX: GUID = GUID {
    data1: 0x6E4E_2102,
    data2: 0xF9CD,
    data3: 0x433D,
    data4: [0xB4, 0x96, 0x30, 0x3C, 0xE0, 0x3A, 0x65, 0x07],
};

pub const IID_ITF_KEY_EVENT_SINK: GUID = GUID {
    data1: 0xAA80_E7F5,
    data2: 0x2021,
    data3: 0x11D2,
    data4: [0x93, 0xE0, 0x00, 0x60, 0xB0, 0x67, 0xB8, 0x6E],
};

pub const IID_ITF_EDIT_SESSION: GUID = GUID {
    data1: 0xAA80_E803,
    data2: 0x2021,
    data3: 0x11D2,
    data4: [0x93, 0xE0, 0x00, 0x60, 0xB0, 0x67, 0xB8, 0x6E],
};

pub const IID_ITF_KEYSTROKE_MGR: GUID = GUID {
    data1: 0xAA80_E7F0,
    data2: 0x2021,
    data3: 0x11D2,
    data4: [0x93, 0xE0, 0x00, 0x60, 0xB0, 0x67, 0xB8, 0x6E],
};

pub const IID_ITF_INSERT_AT_SELECTION: GUID = GUID {
    data1: 0x55CE_16BA,
    data2: 0x3014,
    data3: 0x41C1,
    data4: [0x9C, 0xEF, 0xFA, 0xDE, 0x14, 0x46, 0xAC, 0x6C],
};

pub const IID_ITF_SOURCE: GUID = GUID {
    data1: 0x4EA4_8A35,
    data2: 0x60AE,
    data3: 0x446F,
    data4: [0x8F, 0xD6, 0xE6, 0xA8, 0xD8, 0x24, 0x59, 0xF7],
};

pub const IID_ITF_THREAD_FOCUS_SINK: GUID = GUID {
    data1: 0xC0F1_DB0C,
    data2: 0x3A20,
    data3: 0x405C,
    data4: [0xA3, 0x03, 0x96, 0xB6, 0x01, 0x0A, 0x88, 0x5F],
};

pub const CLSID_TF_INPUT_PROCESSOR_PROFILES: GUID = GUID {
    data1: 0x33C5_3A50,
    data2: 0xF456,
    data3: 0x4884,
    data4: [0xB0, 0x49, 0x85, 0xFD, 0x64, 0x3E, 0xCF, 0xED],
};

pub const IID_ITF_INPUT_PROCESSOR_PROFILES: GUID = GUID {
    data1: 0x1F02_B6C5,
    data2: 0x7842,
    data3: 0x4EE6,
    data4: [0x8A, 0x0B, 0x9A, 0x24, 0x18, 0x3A, 0x95, 0xCA],
};

pub const IID_ITF_INPUT_PROCESSOR_PROFILE_MGR: GUID = GUID {
    data1: 0x71C6_E74C,
    data2: 0x0F28,
    data3: 0x11D8,
    data4: [0xA8, 0x2A, 0x00, 0x06, 0x5B, 0x84, 0x43, 0x5C],
};

pub const CLSID_TF_CATEGORY_MGR: GUID = GUID {
    data1: 0xA4B5_44A1,
    data2: 0x438D,
    data3: 0x4B41,
    data4: [0x93, 0x25, 0x86, 0x95, 0x23, 0xE2, 0xD6, 0xC7],
};

pub const IID_ITF_CATEGORY_MGR: GUID = GUID {
    data1: 0xC3AC_EFB5,
    data2: 0xF69D,
    data3: 0x4905,
    data4: [0x93, 0x8F, 0xFC, 0xAD, 0xCF, 0x4B, 0xE8, 0x30],
};

pub const GUID_TFCAT_TIP_KEYBOARD: GUID = GUID {
    data1: 0x3474_5C63,
    data2: 0xB2F0,
    data3: 0x4784,
    data4: [0x8B, 0x67, 0x5E, 0x12, 0xC8, 0x70, 0x1A, 0x31],
};

pub const GUID_TFCAT_TIPCAP_SECUREMODE: GUID = GUID {
    data1: 0x49D2_F9CF,
    data2: 0x1F5E,
    data3: 0x11D7,
    data4: [0xA6, 0xD3, 0x00, 0x06, 0x5B, 0x84, 0x43, 0x5C],
};

pub const GUID_TFCAT_TIPCAP_UIELEMENTENABLED: GUID = GUID {
    data1: 0x49D2_F9CE,
    data2: 0x1F5E,
    data3: 0x11D7,
    data4: [0xA6, 0xD3, 0x00, 0x06, 0x5B, 0x84, 0x43, 0x5C],
};

pub const GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT: GUID = GUID {
    data1: 0xCCF0_5DD7,
    data2: 0x4A87,
    data3: 0x11D7,
    data4: [0xA6, 0xE2, 0x00, 0x06, 0x5B, 0x84, 0x43, 0x5C],
};

pub const GUID_TFCAT_TIPCAP_COMLESS: GUID = GUID {
    data1: 0x3642_15D9,
    data2: 0x75BC,
    data3: 0x11D7,
    data4: [0xA6, 0xEF, 0x00, 0x06, 0x5B, 0x84, 0x43, 0x5C],
};

pub const GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT: GUID = GUID {
    data1: 0x13A0_16DF,
    data2: 0x560B,
    data3: 0x46CD,
    data4: [0x94, 0x7A, 0x4C, 0x3A, 0xF1, 0xE0, 0xE3, 0x5D],
};

pub const GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT: GUID = GUID {
    data1: 0x2550_4FB4,
    data2: 0x7BAB,
    data3: 0x4BC1,
    data4: [0x9C, 0x69, 0xCF, 0x81, 0x89, 0x0F, 0x0E, 0xF5],
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
pub struct TF_SELECTION {
    pub range: *mut ITfRange,
    pub style_ase: u32,
    pub style_interim_char: i32,
}

// ============================================================================
// 外部 COM 接口虚表声明 (`ITfThreadMgr`, `ITfKeystrokeMgr`, `ITfContext`, `ITfRange`, `ITfContextView`, `ITfInsertAtSelection`)
// ============================================================================

#[repr(C)]
pub struct IUnknownVtbl {
    pub query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
pub struct ITfKeystrokeMgrVtbl {
    pub base: IUnknownVtbl,
    pub advise_key_event_sink: unsafe extern "system" fn(*mut ITfKeystrokeMgr, u32, *mut c_void, i32) -> HRESULT,
    pub unadvise_key_event_sink: unsafe extern "system" fn(*mut ITfKeystrokeMgr, u32) -> HRESULT,
}

#[repr(C)]
pub struct ITfKeystrokeMgr {
    pub lp_vtbl: *const ITfKeystrokeMgrVtbl,
}

#[repr(C)]
pub struct ITfSourceVtbl {
    pub base: IUnknownVtbl,
    pub advise_sink: unsafe extern "system" fn(*mut ITfSource, *const GUID, *mut c_void, *mut u32) -> HRESULT,
    pub unadvise_sink: unsafe extern "system" fn(*mut ITfSource, u32) -> HRESULT,
}

#[repr(C)]
pub struct ITfSource {
    pub lp_vtbl: *const ITfSourceVtbl,
}

#[repr(C)]
pub struct ITfContextVtbl {
    pub base: IUnknownVtbl,
    pub request_edit_session: unsafe extern "system" fn(*mut ITfContext, u32, *mut c_void, u32, *mut HRESULT) -> HRESULT,
    pub in_write_session: unsafe extern "system" fn(*mut ITfContext, u32, *mut i32) -> HRESULT,
    pub get_selection: unsafe extern "system" fn(*mut ITfContext, u32, u32, u32, *mut TF_SELECTION, *mut u32) -> HRESULT,
    pub set_selection: unsafe extern "system" fn(*mut ITfContext, u32, u32, *const TF_SELECTION) -> HRESULT,
    pub get_start: unsafe extern "system" fn(*mut ITfContext, u32, *mut *mut ITfRange) -> HRESULT,
    pub get_end: unsafe extern "system" fn(*mut ITfContext, u32, *mut *mut ITfRange) -> HRESULT,
    pub get_active_view: unsafe extern "system" fn(*mut ITfContext, *mut *mut ITfContextView) -> HRESULT,
}

#[repr(C)]
pub struct ITfContext {
    pub lp_vtbl: *const ITfContextVtbl,
}

#[repr(C)]
pub struct ITfRangeVtbl {
    pub base: IUnknownVtbl,
    pub get_text: *const c_void,
    pub set_text: unsafe extern "system" fn(*mut ITfRange, u32, u32, *const u16, i32) -> HRESULT,
    pub get_formatted_text: *const c_void,
    pub get_embedded: *const c_void,
    pub insert_embedded: *const c_void,
    pub shift_start: *const c_void,
    pub shift_end: *const c_void,
    pub shift_start_to_range: *const c_void,
    pub shift_end_to_range: *const c_void,
    pub shift_start_region: *const c_void,
    pub shift_end_region: *const c_void,
    pub is_empty: *const c_void,
    pub collapse: unsafe extern "system" fn(*mut ITfRange, u32, u32) -> HRESULT,
}

#[repr(C)]
pub struct ITfRange {
    pub lp_vtbl: *const ITfRangeVtbl,
}

#[repr(C)]
pub struct ITfContextViewVtbl {
    pub base: IUnknownVtbl,
    pub get_range_from_point: *const c_void,
    pub get_text_ext: unsafe extern "system" fn(*mut ITfContextView, u32, *mut ITfRange, *mut RECT, *mut i32) -> HRESULT,
    pub get_screen_ext: unsafe extern "system" fn(*mut ITfContextView, *mut RECT) -> HRESULT,
    pub get_wnd: unsafe extern "system" fn(*mut ITfContextView, *mut isize) -> HRESULT,
}

#[repr(C)]
pub struct ITfContextView {
    pub lp_vtbl: *const ITfContextViewVtbl,
}

#[repr(C)]
pub struct ITfInsertAtSelectionVtbl {
    pub base: IUnknownVtbl,
    pub insert_text_at_selection: unsafe extern "system" fn(*mut ITfInsertAtSelection, u32, u32, *const u16, i32, *mut *mut ITfRange) -> HRESULT,
}

#[repr(C)]
pub struct ITfInsertAtSelection {
    pub lp_vtbl: *const ITfInsertAtSelectionVtbl,
}

#[repr(C)]
pub struct ITfInputProcessorProfilesVtbl {
    pub base: IUnknownVtbl,
    pub register: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID) -> HRESULT,
    pub unregister: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID) -> HRESULT,
    pub add_language_profile: unsafe extern "system" fn(
        *mut ITfInputProcessorProfiles,
        *const GUID,
        u16,
        *const GUID,
        *const u16,
        u32,
        *const u16,
        u32,
        u32,
    ) -> HRESULT,
    pub remove_language_profile: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID, u16, *const GUID) -> HRESULT,
    pub enum_input_processor_info: *const c_void,
    pub get_default_language_profile: *const c_void,
    pub set_default_language_profile: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, u16, *const GUID, *const GUID) -> HRESULT,
    pub activate_language_profile: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID, u16, *const GUID) -> HRESULT,
    pub get_active_language_profile: *const c_void,
    pub get_language_profile_description: *const c_void,
    pub get_current_language: *const c_void,
    pub change_current_language: *const c_void,
    pub get_language_list: *const c_void,
    pub enum_language_profiles: *const c_void,
    pub enable_language_profile: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID, u16, *const GUID, i32) -> HRESULT,
    pub is_enabled_language_profile: *const c_void,
    pub enable_language_profile_by_default: unsafe extern "system" fn(*mut ITfInputProcessorProfiles, *const GUID, u16, *const GUID, i32) -> HRESULT,
}

#[repr(C)]
pub struct ITfInputProcessorProfiles {
    pub lp_vtbl: *const ITfInputProcessorProfilesVtbl,
}

#[repr(C)]
pub struct ITfCategoryMgrVtbl {
    pub base: IUnknownVtbl,
    pub register_category: unsafe extern "system" fn(*mut ITfCategoryMgr, *const GUID, *const GUID, *const GUID) -> HRESULT,
    pub unregister_category: unsafe extern "system" fn(*mut ITfCategoryMgr, *const GUID, *const GUID, *const GUID) -> HRESULT,
}

#[repr(C)]
pub struct ITfCategoryMgr {
    pub lp_vtbl: *const ITfCategoryMgrVtbl,
}

#[allow(dead_code)]
const TF_ES_ASYNCDONTCARE: u32 = 0x0;
const TF_ES_SYNC: u32 = 0x1;
const TF_ES_READ: u32 = 0x2;
#[allow(dead_code)]
const TF_ES_READWRITE: u32 = 0x6;
const TF_DEFAULT_SELECTION: u32 = 0xFFFF_FFFF;
const TF_ANCHOR_END: u32 = 1;
const TF_AE_NONE: u32 = 0;

static DLL_REF_COUNT: AtomicI32 = AtomicI32::new(0);
static ACTIVE_TIP_COUNT: AtomicI32 = AtomicI32::new(0);
static LAST_CARET_RECT: Mutex<(i32, i32)> = Mutex::new((0, 0));
static LOCAL_SHIFT_TAP_ARMED: AtomicBool = AtomicBool::new(false);
static COMMIT_POLLER_STARTED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
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
#[cfg(target_os = "windows")]
const _: () = assert!(std::mem::size_of::<INPUT>() == 40);

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetModuleFileNameW(h_module: isize, lp_filename: *mut u16, n_size: u32) -> u32;
    fn GetModuleHandleExW(dw_flags: u32, lp_module_name: *const u16, ph_module: *mut isize) -> i32;
    fn OpenMutexW(dw_desired_access: u32, b_inherit_handle: i32, lp_name: *const u16) -> isize;
    fn CloseHandle(h_object: isize) -> i32;
    fn LoadLibraryW(lp_lib_file_name: *const u16) -> isize;
    fn GetProcAddress(h_module: isize, lp_proc_name: *const u8) -> *const c_void;
    fn FreeLibrary(h_lib_module: isize) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "user32")]
extern "system" {
    fn GetKeyState(n_virt_key: i32) -> i16;
    fn SendInput(c_inputs: u32, p_inputs: *const INPUT, cb_size: i32) -> u32;
    fn GetForegroundWindow() -> isize;
    fn GetWindowThreadProcessId(h_wnd: isize, lpdw_process_id: *mut u32) -> u32;
}

#[cfg(target_os = "windows")]
#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pv_reserved: *mut c_void, dw_co_init: u32) -> HRESULT;
    fn CoUninitialize();
    fn CoCreateInstance(
        rclsid: *const GUID,
        p_unk_outer: *mut c_void,
        dw_cls_context: u32,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT;
}

#[cfg(target_os = "windows")]
#[link(name = "advapi32")]
extern "system" {
    fn RegCreateKeyExW(
        h_key: isize,
        lp_sub_key: *const u16,
        reserved: u32,
        lp_class: *const u16,
        dw_options: u32,
        sam_desired: u32,
        lp_security_attributes: *const c_void,
        phk_result: *mut isize,
        lpdw_disposition: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        h_key: isize,
        lp_value_name: *const u16,
        reserved: u32,
        dw_type: u32,
        lp_data: *const u8,
        cb_data: u32,
    ) -> i32;
    fn RegCloseKey(h_key: isize) -> i32;
    fn RegDeleteTreeW(h_key: isize, lp_sub_key: *const u16) -> i32;
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn current_exe_basename() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// 判断宿主进程是否为受 Windows AppContainer 沙箱网络隔离及 DWM Immersive 层级限制的系统壳层组件
/// （如 Win11 搜索 `SearchHost.exe`、开始菜单 `StartMenuExperienceHost.exe` 等）。
/// 在这些进程内，本 TSF DLL 保持激活以接管系统输入法焦点（防止系统回退到微软拼音/小狼毫），
/// 同时将按键拦截与上屏交由桌面会话的中央守护服务直接完成，彻底绕开 AppContainer 环回网络限制！
pub fn is_shell_appcontainer_host(exe_name: &str) -> bool {
    let lower = exe_name.trim().to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "searchhost.exe"
            | "startmenuexperiencehost.exe"
            | "shellexperiencehost.exe"
            | "searchapp.exe"
            | "searchui.exe"
            | "textinputhost.exe"
            | "lockapp.exe"
    )
}

/// 为输入法安装目录授予 ALL APPLICATION PACKAGES (`S-1-15-2-1`) 与 ALL RESTRICTED APPLICATION PACKAGES (`S-1-15-2-2`)
/// 的读取与执行权限，确保 `SearchHost.exe` / `StartMenuExperienceHost.exe` 等 AppContainer 进程可正常加载 `ime_tsf_win.dll`
pub fn ensure_appcontainer_dir_acl(dir: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("icacls.exe")
            .arg(dir)
            .args([
                "/grant",
                "*S-1-15-2-1:(OI)(CI)(RX)",
                "*S-1-15-2-2:(OI)(CI)(RX)",
                "*S-1-5-32-545:(OI)(CI)(RX)",
                "/T",
                "/Q",
            ])
            .creation_flags(0x0800_0000)
            .status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dir;
    }
}

/// 向中央守护服务 (`127.0.0.1:18435`) 发送一次 IPC 请求并等待响应
pub fn send_tsf_ipc(req: &TsfIpcRequest) -> Option<TsfIpcResponse> {
    let addr = SocketAddr::from(([127, 0, 0, 1], TSF_IPC_PORT));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(30)).ok()?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(150)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(30)));

    let mut payload = serde_json::to_string(req).ok()?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).ok()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str::<TsfIpcResponse>(line.trim()).ok()
}

// ============================================================================
// 1. ITfEditSession 实现（用于在目标应用程序中查询精确光标位置 & 直接插入上屏文本）
// ============================================================================

#[repr(C)]
struct TsfEditSessionVtbl {
    base: IUnknownVtbl,
    do_edit_session: unsafe extern "system" fn(*mut TsfEditSessionCom, u32) -> HRESULT,
}

#[repr(C)]
struct TsfEditSessionCom {
    lp_vtbl: *const TsfEditSessionVtbl,
    ref_count: AtomicU32,
    context: *mut ITfContext,
    commit_text: String,
    query_caret_only: bool,
}

static EDIT_SESSION_VTBL: TsfEditSessionVtbl = TsfEditSessionVtbl {
    base: IUnknownVtbl {
        query_interface: edit_session_qi,
        add_ref: edit_session_add_ref,
        release: edit_session_release,
    },
    do_edit_session: edit_session_do,
};

impl TsfEditSessionCom {
    unsafe fn new_boxed(context: *mut ITfContext, commit_text: String, query_caret_only: bool) -> *mut Self {
        if !context.is_null() {
            ((*(*context).lp_vtbl).base.add_ref)(context as *mut c_void);
        }
        let b = Box::new(Self {
            lp_vtbl: &EDIT_SESSION_VTBL,
            ref_count: AtomicU32::new(1),
            context,
            commit_text,
            query_caret_only,
        });
        Box::into_raw(b)
    }
}

unsafe extern "system" fn edit_session_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    if *iid == IID_IUNKNOWN || *iid == IID_ITF_EDIT_SESSION {
        *ppv = this;
        edit_session_add_ref(this);
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn edit_session_add_ref(this: *mut c_void) -> u32 {
    let obj = &*(this as *mut TsfEditSessionCom);
    obj.ref_count.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn edit_session_release(this: *mut c_void) -> u32 {
    let obj = &mut *(this as *mut TsfEditSessionCom);
    let prev = obj.ref_count.fetch_sub(1, Ordering::SeqCst);
    if prev == 1 {
        if !obj.context.is_null() {
            ((*(*obj.context).lp_vtbl).base.release)(obj.context as *mut c_void);
        }
        drop(Box::from_raw(this as *mut TsfEditSessionCom));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn edit_session_do(this: *mut TsfEditSessionCom, ec: u32) -> HRESULT {
    let obj = &*this;
    let ctx = obj.context;
    if ctx.is_null() {
        return E_FAIL;
    }

    // 1. 查询当前选区/光标的精确屏幕坐标 (`ITfContextView::GetTextExt`)
    let mut sel = TF_SELECTION {
        range: null_mut(),
        style_ase: 0,
        style_interim_char: 0,
    };
    let mut fetched = 0u32;
    if ((*(*ctx).lp_vtbl).get_selection)(ctx, ec, TF_DEFAULT_SELECTION, 1, &mut sel, &mut fetched) == S_OK
        && !sel.range.is_null()
    {
        let mut view: *mut ITfContextView = null_mut();
        if ((*(*ctx).lp_vtbl).get_active_view)(ctx, &mut view) == S_OK && !view.is_null() {
            let mut rc = RECT::default();
            let mut clipped = 0i32;
            if ((*(*view).lp_vtbl).get_text_ext)(view, ec, sel.range, &mut rc, &mut clipped) == S_OK {
                if rc.left > 0 || rc.bottom > 0 {
                    if let Ok(mut guard) = LAST_CARET_RECT.lock() {
                        *guard = (rc.left, rc.bottom);
                    }
                }
            }
            ((*(*view).lp_vtbl).base.release)(view as *mut c_void);
        }
        ((*(*sel.range).lp_vtbl).base.release)(sel.range as *mut c_void);
    }

    if obj.query_caret_only || obj.commit_text.is_empty() {
        return S_OK;
    }

    // 2. 通过 `ITfInsertAtSelection::InsertTextAtSelection` 将上屏文本直接插入目标应用文档
    let mut insert_sel: *mut ITfInsertAtSelection = null_mut();
    let hr = ((*(*ctx).lp_vtbl).base.query_interface)(
        ctx as *mut c_void,
        &IID_ITF_INSERT_AT_SELECTION,
        &mut insert_sel as *mut *mut _ as *mut *mut c_void,
    );
    if hr == S_OK && !insert_sel.is_null() {
        let utf16: Vec<u16> = obj.commit_text.encode_utf16().collect();
        let mut inserted_range: *mut ITfRange = null_mut();
        let hr_ins = ((*(*insert_sel).lp_vtbl).insert_text_at_selection)(
            insert_sel,
            ec,
            0,
            utf16.as_ptr(),
            utf16.len() as i32,
            &mut inserted_range,
        );
        if hr_ins == S_OK && !inserted_range.is_null() {
            let _ = ((*(*inserted_range).lp_vtbl).collapse)(inserted_range, ec, TF_ANCHOR_END);
            let new_sel = TF_SELECTION {
                range: inserted_range,
                style_ase: TF_AE_NONE,
                style_interim_char: 0,
            };
            let _ = ((*(*ctx).lp_vtbl).set_selection)(ctx, ec, 1, &new_sel);
            ((*(*inserted_range).lp_vtbl).base.release)(inserted_range as *mut c_void);
        }
        ((*(*insert_sel).lp_vtbl).base.release)(insert_sel as *mut c_void);
    }

    S_OK
}

// ============================================================================
// 2. TsfTextService COM 对象（同时实现 `ITfTextInputProcessorEx` 与 `ITfKeyEventSink`）
// ============================================================================

#[repr(C)]
struct ITfTextInputProcessorExVtbl {
    base: IUnknownVtbl,
    activate: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> HRESULT,
    deactivate: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    activate_ex: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32) -> HRESULT,
}

#[repr(C)]
struct ITfKeyEventSinkVtbl {
    base: IUnknownVtbl,
    on_set_focus: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
    on_test_key_down: unsafe extern "system" fn(*mut c_void, *mut ITfContext, usize, isize, *mut i32) -> HRESULT,
    on_test_key_up: unsafe extern "system" fn(*mut c_void, *mut ITfContext, usize, isize, *mut i32) -> HRESULT,
    on_key_down: unsafe extern "system" fn(*mut c_void, *mut ITfContext, usize, isize, *mut i32) -> HRESULT,
    on_key_up: unsafe extern "system" fn(*mut c_void, *mut ITfContext, usize, isize, *mut i32) -> HRESULT,
    on_preserved_key: unsafe extern "system" fn(*mut c_void, *mut ITfContext, *const GUID, *mut i32) -> HRESULT,
}

#[repr(C)]
struct ITfThreadFocusSinkVtbl {
    base: IUnknownVtbl,
    on_set_thread_focus: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    on_kill_thread_focus: unsafe extern "system" fn(*mut c_void) -> HRESULT,
}

#[repr(C)]
pub struct TsfTextServiceCom {
    tip_vtbl: *const ITfTextInputProcessorExVtbl,
    sink_vtbl: *const ITfKeyEventSinkVtbl,
    focus_sink_vtbl: *const ITfThreadFocusSinkVtbl,
    ref_count: AtomicU32,
    thread_mgr: Mutex<*mut c_void>,
    client_id: AtomicU32,
    activated: AtomicBool,
    focus_cookie: AtomicU32,
}

unsafe impl Send for TsfTextServiceCom {}
unsafe impl Sync for TsfTextServiceCom {}

const SINK_OFFSET: usize = std::mem::size_of::<*const ITfTextInputProcessorExVtbl>();
const FOCUS_SINK_OFFSET: usize = SINK_OFFSET + std::mem::size_of::<*const ITfKeyEventSinkVtbl>();

#[inline]
unsafe fn from_sink_ptr(p: *mut c_void) -> *mut TsfTextServiceCom {
    (p as *mut u8).sub(SINK_OFFSET) as *mut TsfTextServiceCom
}

#[inline]
unsafe fn from_focus_sink_ptr(p: *mut c_void) -> *mut TsfTextServiceCom {
    (p as *mut u8).sub(FOCUS_SINK_OFFSET) as *mut TsfTextServiceCom
}

static TIP_VTBL: ITfTextInputProcessorExVtbl = ITfTextInputProcessorExVtbl {
    base: IUnknownVtbl {
        query_interface: tip_qi,
        add_ref: tip_add_ref,
        release: tip_release,
    },
    activate: tip_activate,
    deactivate: tip_deactivate,
    activate_ex: tip_activate_ex,
};

static SINK_VTBL: ITfKeyEventSinkVtbl = ITfKeyEventSinkVtbl {
    base: IUnknownVtbl {
        query_interface: sink_qi,
        add_ref: sink_add_ref,
        release: sink_release,
    },
    on_set_focus: sink_on_set_focus,
    on_test_key_down: sink_on_test_key_down,
    on_test_key_up: sink_on_test_key_up,
    on_key_down: sink_on_key_down,
    on_key_up: sink_on_key_up,
    on_preserved_key: sink_on_preserved_key,
};

static FOCUS_SINK_VTBL: ITfThreadFocusSinkVtbl = ITfThreadFocusSinkVtbl {
    base: IUnknownVtbl {
        query_interface: focus_sink_qi,
        add_ref: focus_sink_add_ref,
        release: focus_sink_release,
    },
    on_set_thread_focus: focus_sink_on_set_thread_focus,
    on_kill_thread_focus: focus_sink_on_kill_thread_focus,
};

impl TsfTextServiceCom {
    pub fn new_boxed() -> *mut Self {
        DLL_REF_COUNT.fetch_add(1, Ordering::SeqCst);
        let b = Box::new(Self {
            tip_vtbl: &TIP_VTBL,
            sink_vtbl: &SINK_VTBL,
            focus_sink_vtbl: &FOCUS_SINK_VTBL,
            ref_count: AtomicU32::new(1),
            thread_mgr: Mutex::new(null_mut()),
            client_id: AtomicU32::new(0),
            activated: AtomicBool::new(false),
            focus_cookie: AtomicU32::new(u32::MAX),
        });
        Box::into_raw(b)
    }
}

unsafe extern "system" fn tip_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    let obj = this as *mut TsfTextServiceCom;
    if *iid == IID_IUNKNOWN
        || *iid == IID_ITF_TEXT_INPUT_PROCESSOR
        || *iid == IID_ITF_TEXT_INPUT_PROCESSOR_EX
    {
        *ppv = obj as *mut c_void;
        tip_add_ref(obj as *mut c_void);
        S_OK
    } else if *iid == IID_ITF_KEY_EVENT_SINK {
        *ppv = (&(*obj).sink_vtbl) as *const _ as *mut c_void;
        tip_add_ref(obj as *mut c_void);
        S_OK
    } else if *iid == IID_ITF_THREAD_FOCUS_SINK {
        *ppv = (&(*obj).focus_sink_vtbl) as *const _ as *mut c_void;
        tip_add_ref(obj as *mut c_void);
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn tip_add_ref(this: *mut c_void) -> u32 {
    let obj = &*(this as *mut TsfTextServiceCom);
    obj.ref_count.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn tip_release(this: *mut c_void) -> u32 {
    let obj = &*(this as *mut TsfTextServiceCom);
    let prev = obj.ref_count.fetch_sub(1, Ordering::SeqCst);
    if prev == 1 {
        if obj.activated.load(Ordering::SeqCst) {
            let _ = tip_deactivate(this);
        }
        drop(Box::from_raw(this as *mut TsfTextServiceCom));
        DLL_REF_COUNT.fetch_sub(1, Ordering::SeqCst);
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn tip_activate(
    this: *mut c_void,
    ptim: *mut c_void,
    tid: u32,
) -> HRESULT {
    tip_activate_ex(this, ptim, tid, 0)
}

unsafe extern "system" fn tip_activate_ex(
    this: *mut c_void,
    ptim: *mut c_void,
    tid: u32,
    _dw_flags: u32,
) -> HRESULT {
    if ptim.is_null() {
        return E_POINTER;
    }
    let obj = &*(this as *mut TsfTextServiceCom);
    let was_active = obj.activated.swap(true, Ordering::SeqCst);
    if !was_active {
        ACTIVE_TIP_COUNT.fetch_add(1, Ordering::SeqCst);
    }
    obj.client_id.store(tid, Ordering::SeqCst);

    let unk_vtbl = *(ptim as *mut *const IUnknownVtbl);
    ((*unk_vtbl).add_ref)(ptim);
    if let Ok(mut guard) = obj.thread_mgr.lock() {
        if !(*guard).is_null() {
            ((*unk_vtbl).release)(*guard);
        }
        *guard = ptim;
    }

    // 1. 挂载 ITfKeyEventSink（先尝试卸载同 tid 上可能残留的旧 Sink）
    let mut ks_mgr: *mut ITfKeystrokeMgr = null_mut();
    if ((*unk_vtbl).query_interface)(
        ptim,
        &IID_ITF_KEYSTROKE_MGR,
        &mut ks_mgr as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !ks_mgr.is_null()
    {
        let _ = ((*(*ks_mgr).lp_vtbl).unadvise_key_event_sink)(ks_mgr, tid);
        let sink_ptr = (&obj.sink_vtbl) as *const _ as *mut c_void;
        let _ = ((*(*ks_mgr).lp_vtbl).advise_key_event_sink)(ks_mgr, tid, sink_ptr, 1);
        ((*(*ks_mgr).lp_vtbl).base.release)(ks_mgr as *mut c_void);
    }

    // 2. 挂载 ITfThreadFocusSink，使窗口切焦（例如在 Edge 浏览器不同标签页或新建标签页点击时）也能可靠上报 activate
    let mut tf_src: *mut ITfSource = null_mut();
    if ((*unk_vtbl).query_interface)(
        ptim,
        &IID_ITF_SOURCE,
        &mut tf_src as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !tf_src.is_null()
    {
        let prev_cookie = obj.focus_cookie.swap(u32::MAX, Ordering::SeqCst);
        if prev_cookie != u32::MAX {
            let _ = ((*(*tf_src).lp_vtbl).unadvise_sink)(tf_src, prev_cookie);
        }
        let mut cookie: u32 = u32::MAX;
        let focus_sink_ptr = (&obj.focus_sink_vtbl) as *const _ as *mut c_void;
        if ((*(*tf_src).lp_vtbl).advise_sink)(
            tf_src,
            &IID_ITF_THREAD_FOCUS_SINK,
            focus_sink_ptr,
            &mut cookie,
        ) == S_OK
            && cookie != u32::MAX
        {
            obj.focus_cookie.store(cookie, Ordering::SeqCst);
        }
        ((*(*tf_src).lp_vtbl).base.release)(tf_src as *mut c_void);
    }

    // 在受限 AppContainer 壳层组件（SearchHost.exe / StartMenuExperienceHost.exe）中，
    // 保持本 TSF TIP 激活以阻止系统切回微软拼音，同时跳过环回 TCP 通信交由中央守护服务直接拦截按键
    let exe_name = current_exe_basename();
    if is_shell_appcontainer_host(&exe_name) {
        return S_OK;
    }

    // 启动单实例进程内异步上屏轮询线程（支持鼠标点击候选框在管理员窗口如 Taskmgr.exe 中正常上屏）
    if !COMMIT_POLLER_STARTED.swap(true, Ordering::SeqCst) {
        std::thread::spawn(|| {
            let my_pid = std::process::id();
            loop {
                std::thread::sleep(Duration::from_millis(55));
                if ACTIVE_TIP_COUNT.load(Ordering::SeqCst) <= 0 {
                    continue;
                }
                #[cfg(target_os = "windows")]
                unsafe {
                    let fg = GetForegroundWindow();
                    if fg == 0 {
                        continue;
                    }
                    let mut fg_pid: u32 = 0;
                    GetWindowThreadProcessId(fg, &mut fg_pid);
                    if fg_pid != my_pid {
                        continue;
                    }
                    if let Some(resp) = send_tsf_ipc(&TsfIpcRequest {
                        op: "poll_commit".to_string(),
                        vk: 0,
                        shift: false,
                        ctrl: false,
                        alt: false,
                        caret_x: 0,
                        caret_y: 0,
                        exe_name: String::new(),
                        pid: my_pid,
                    }) {
                        if !resp.commit_text.is_empty() {
                            let _ = send_unicode_from_tsf(&resp.commit_text);
                        }
                    }
                }
            }
        });
    }

    // 通知中央守护服务：TSF 正式输入法已激活
    if send_tsf_ipc(&TsfIpcRequest {
        op: "activate".to_string(),
        vk: 0,
        shift: false,
        ctrl: false,
        alt: false,
        caret_x: 0,
        caret_y: 0,
        exe_name,
        pid: std::process::id(),
    })
    .is_none()
    {
        #[cfg(target_os = "windows")]
        {
            // 先检查全局单实例命名互斥量，若守护服务已在启动加载词库中则绝不重复拉起新进程
            let mutex_name = to_wide("Local\\RustAiPinyinImeGlobalServerSingletonMutex_v1");
            let h_mtx = OpenMutexW(0x0010_0000 /* SYNCHRONIZE */, 0, mutex_name.as_ptr());
            if h_mtx != 0 {
                CloseHandle(h_mtx);
            } else if let Ok(dll_p) = get_this_dll_path() {
                if let Some(parent) = std::path::Path::new(&dll_p).parent() {
                    let server_exe = parent.join("RustAI拼音输入法_全局版.exe");
                    if server_exe.exists() {
                        use std::os::windows::process::CommandExt;
                        let _ = std::process::Command::new(server_exe)
                            .creation_flags(0x0800_0000)
                            .spawn();
                    }
                }
            }
        }
    }

    S_OK
}

unsafe extern "system" fn tip_deactivate(this: *mut c_void) -> HRESULT {
    let obj = &*(this as *mut TsfTextServiceCom);
    // 仅当实例确实处于已激活状态时才执行注销与 IPC 通知，防止 msctf 探测接口后释放未激活实例导致误发 deactivate！
    if !obj.activated.swap(false, Ordering::SeqCst) {
        return S_OK;
    }
    let tid = obj.client_id.swap(0, Ordering::SeqCst);
    let focus_cookie = obj.focus_cookie.swap(u32::MAX, Ordering::SeqCst);
    let ptim = if let Ok(mut guard) = obj.thread_mgr.lock() {
        let p = *guard;
        *guard = null_mut();
        p
    } else {
        null_mut()
    };

    if !ptim.is_null() {
        let unk_vtbl = *(ptim as *mut *const IUnknownVtbl);
        if focus_cookie != u32::MAX {
            let mut tf_src: *mut ITfSource = null_mut();
            if ((*unk_vtbl).query_interface)(
                ptim,
                &IID_ITF_SOURCE,
                &mut tf_src as *mut *mut _ as *mut *mut c_void,
            ) == S_OK
                && !tf_src.is_null()
            {
                let _ = ((*(*tf_src).lp_vtbl).unadvise_sink)(tf_src, focus_cookie);
                ((*(*tf_src).lp_vtbl).base.release)(tf_src as *mut c_void);
            }
        }
        let mut ks_mgr: *mut ITfKeystrokeMgr = null_mut();
        if ((*unk_vtbl).query_interface)(
            ptim,
            &IID_ITF_KEYSTROKE_MGR,
            &mut ks_mgr as *mut *mut _ as *mut *mut c_void,
        ) == S_OK
            && !ks_mgr.is_null()
        {
            let _ = ((*(*ks_mgr).lp_vtbl).unadvise_key_event_sink)(ks_mgr, tid);
            ((*(*ks_mgr).lp_vtbl).base.release)(ks_mgr as *mut c_void);
        }
        ((*unk_vtbl).release)(ptim);
    }

    let prev_active = ACTIVE_TIP_COUNT.fetch_sub(1, Ordering::SeqCst);
    if prev_active <= 1 {
        let exe_name = current_exe_basename();
        if !is_shell_appcontainer_host(&exe_name) {
            let _ = send_tsf_ipc(&TsfIpcRequest {
                op: "deactivate".to_string(),
                vk: 0,
                shift: false,
                ctrl: false,
                alt: false,
                caret_x: 0,
                caret_y: 0,
                exe_name,
                pid: std::process::id(),
            });
        }
    }

    S_OK
}

unsafe extern "system" fn sink_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    tip_qi(from_sink_ptr(this) as *mut c_void, riid, ppv)
}

unsafe extern "system" fn sink_add_ref(this: *mut c_void) -> u32 {
    tip_add_ref(from_sink_ptr(this) as *mut c_void)
}

unsafe extern "system" fn sink_release(this: *mut c_void) -> u32 {
    tip_release(from_sink_ptr(this) as *mut c_void)
}

unsafe extern "system" fn focus_sink_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    tip_qi(from_focus_sink_ptr(this) as *mut c_void, riid, ppv)
}

unsafe extern "system" fn focus_sink_add_ref(this: *mut c_void) -> u32 {
    tip_add_ref(from_focus_sink_ptr(this) as *mut c_void)
}

unsafe extern "system" fn focus_sink_release(this: *mut c_void) -> u32 {
    tip_release(from_focus_sink_ptr(this) as *mut c_void)
}

unsafe extern "system" fn focus_sink_on_set_thread_focus(this: *mut c_void) -> HRESULT {
    LOCAL_SHIFT_TAP_ARMED.store(false, Ordering::SeqCst);
    let obj = &*from_focus_sink_ptr(this);
    if obj.activated.load(Ordering::SeqCst) {
        let exe_name = current_exe_basename();
        if !is_shell_appcontainer_host(&exe_name) {
            let _ = send_tsf_ipc(&TsfIpcRequest {
                op: "activate".to_string(),
                vk: 0,
                shift: false,
                ctrl: false,
                alt: false,
                caret_x: 0,
                caret_y: 0,
                exe_name,
                pid: std::process::id(),
            });
        }
    }
    S_OK
}

unsafe extern "system" fn focus_sink_on_kill_thread_focus(_this: *mut c_void) -> HRESULT {
    LOCAL_SHIFT_TAP_ARMED.store(false, Ordering::SeqCst);
    S_OK
}

unsafe extern "system" fn sink_on_set_focus(this: *mut c_void, f_foreground: i32) -> HRESULT {
    LOCAL_SHIFT_TAP_ARMED.store(false, Ordering::SeqCst);
    let obj = &*from_sink_ptr(this);
    if f_foreground != 0 && obj.activated.load(Ordering::SeqCst) {
        let exe_name = current_exe_basename();
        if !is_shell_appcontainer_host(&exe_name) {
            let _ = send_tsf_ipc(&TsfIpcRequest {
                op: "activate".to_string(),
                vk: 0,
                shift: false,
                ctrl: false,
                alt: false,
                caret_x: 0,
                caret_y: 0,
                exe_name,
                pid: std::process::id(),
            });
        }
    }
    S_OK
}

unsafe fn query_key_modifiers() -> (bool, bool, bool) {
    #[cfg(target_os = "windows")]
    {
        let shift = (GetKeyState(0x10) as u16 & 0x8000) != 0;
        let ctrl = (GetKeyState(0x11) as u16 & 0x8000) != 0;
        let alt = (GetKeyState(0x12) as u16 & 0x8000) != 0;
        (shift, ctrl, alt)
    }
    #[cfg(not(target_os = "windows"))]
    {
        (false, false, false)
    }
}

/// 在目标应用进程内直接通过 `SendInput(KEYEVENTF_UNICODE)` 注入上屏文本（兼容 Chromium/Electron/微信/VS Code/Notepad/管理员窗口等 100% 场景）
#[cfg(target_os = "windows")]
unsafe fn send_unicode_from_tsf(text: &str) -> u32 {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    if utf16.is_empty() {
        return 0;
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
    SendInput(
        inputs.len() as u32,
        inputs.as_ptr(),
        std::mem::size_of::<INPUT>() as i32,
    )
}

#[cfg(not(target_os = "windows"))]
unsafe fn send_unicode_from_tsf(_text: &str) -> u32 {
    0
}

unsafe fn update_caret_from_context(obj: &TsfTextServiceCom, pic: *mut ITfContext) {
    if pic.is_null() {
        return;
    }
    if let Ok(mut guard) = LAST_CARET_RECT.lock() {
        *guard = (0, 0);
    }
    let tid = obj.client_id.load(Ordering::SeqCst);
    let es = TsfEditSessionCom::new_boxed(pic, String::new(), true);
    let mut hr_session: HRESULT = S_OK;
    let _ = ((*(*pic).lp_vtbl).request_edit_session)(
        pic,
        tid,
        es as *mut c_void,
        TF_ES_SYNC | TF_ES_READ,
        &mut hr_session,
    );
    edit_session_release(es as *mut c_void);
}

unsafe fn commit_text_to_context(obj: &TsfTextServiceCom, pic: *mut ITfContext, text: String) {
    if text.is_empty() {
        return;
    }
    // 优先在目标进程内延迟 6ms 使用 Unicode SendInput 上屏（等待 OnKeyDown 返回 *pf_eaten=1 后再以目标进程自身权限注入，
    // 100% 兼容 Taskmgr.exe 等管理员窗口以及 Chrome/Electron/WinUI3 XAML Island 控件）
    let text_clone = text.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(6));
        unsafe {
            let _ = send_unicode_from_tsf(&text_clone);
        }
    });
    if pic.is_null() {
        return;
    }
    let _ = obj;
}

unsafe extern "system" fn sink_on_test_key_down(
    _this: *mut c_void,
    _pic: *mut ITfContext,
    w_param: usize,
    _l_param: isize,
    pf_eaten: *mut i32,
) -> HRESULT {
    if pf_eaten.is_null() {
        return E_POINTER;
    }
    let vk = (w_param & 0xFF) as u32;
    // 立即放行由 SendInput(KEYEVENTF_UNICODE) 注入的 VK_PACKET (0xE7) 字符事件
    if vk == 0xE7 || vk == 0 {
        *pf_eaten = 0;
        return S_OK;
    }
    let exe_name = current_exe_basename();
    if is_shell_appcontainer_host(&exe_name) {
        *pf_eaten = 0;
        return S_OK;
    }

    let (shift, ctrl, alt) = query_key_modifiers();
    // 单按 Shift 键在本地标记候选切换状态，不吃掉按键以便 Shift 组合键正常工作
    if matches!(vk, 0x10 | 0xA0 | 0xA1) {
        LOCAL_SHIFT_TAP_ARMED.store(!ctrl && !alt, Ordering::SeqCst);
        *pf_eaten = 0;
        return S_OK;
    }
    LOCAL_SHIFT_TAP_ARMED.store(false, Ordering::SeqCst);

    // 放行带有 Alt 或 Ctrl 的快捷键（除 Ctrl+Space 0x20 与 Ctrl+Left/Right 0x25/0x27 外）
    if alt || (ctrl && !matches!(vk, 0x20 | 0x25 | 0x27)) {
        *pf_eaten = 0;
        return S_OK;
    }

    if let Some(resp) = send_tsf_ipc(&TsfIpcRequest {
        op: "test_key_down".to_string(),
        vk,
        shift,
        ctrl,
        alt,
        caret_x: 0,
        caret_y: 0,
        exe_name,
        pid: std::process::id(),
    }) {
        *pf_eaten = if resp.eaten { 1 } else { 0 };
    } else {
        *pf_eaten = 0;
    }
    S_OK
}

unsafe extern "system" fn sink_on_test_key_up(
    this: *mut c_void,
    pic: *mut ITfContext,
    w_param: usize,
    _l_param: isize,
    pf_eaten: *mut i32,
) -> HRESULT {
    if pf_eaten.is_null() {
        return E_POINTER;
    }
    // 绝不吞掉 WM_KEYUP，避免目标应用误判 Shift 等修饰键卡住
    *pf_eaten = 0;
    let vk = (w_param & 0xFF) as u32;
    if matches!(vk, 0x10 | 0xA0 | 0xA1) && LOCAL_SHIFT_TAP_ARMED.swap(false, Ordering::SeqCst) {
        let exe_name = current_exe_basename();
        if is_shell_appcontainer_host(&exe_name) {
            return S_OK;
        }
        let obj = &*from_sink_ptr(this);
        let (shift, ctrl, alt) = query_key_modifiers();
        if let Some(resp) = send_tsf_ipc(&TsfIpcRequest {
            op: "key_up".to_string(),
            vk: 0x10,
            shift,
            ctrl,
            alt,
            caret_x: 0,
            caret_y: 0,
            exe_name,
            pid: std::process::id(),
        }) {
            if !resp.commit_text.is_empty() {
                commit_text_to_context(obj, pic, resp.commit_text);
            }
        }
    }
    S_OK
}

unsafe extern "system" fn sink_on_key_down(
    this: *mut c_void,
    pic: *mut ITfContext,
    w_param: usize,
    _l_param: isize,
    pf_eaten: *mut i32,
) -> HRESULT {
    if pf_eaten.is_null() {
        return E_POINTER;
    }
    let vk = (w_param & 0xFF) as u32;
    if vk == 0xE7 || vk == 0 {
        *pf_eaten = 0;
        return S_OK;
    }
    let exe_name = current_exe_basename();
    if is_shell_appcontainer_host(&exe_name) {
        *pf_eaten = 0;
        return S_OK;
    }
    LOCAL_SHIFT_TAP_ARMED.store(false, Ordering::SeqCst);

    let obj = &*from_sink_ptr(this);
    update_caret_from_context(obj, pic);
    let (caret_x, caret_y) = LAST_CARET_RECT.lock().map(|g| *g).unwrap_or((0, 0));
    let (shift, ctrl, alt) = query_key_modifiers();

    if let Some(resp) = send_tsf_ipc(&TsfIpcRequest {
        op: "key_down".to_string(),
        vk,
        shift,
        ctrl,
        alt,
        caret_x,
        caret_y,
        exe_name,
        pid: std::process::id(),
    }) {
        *pf_eaten = if resp.eaten { 1 } else { 0 };
        if !resp.commit_text.is_empty() {
            commit_text_to_context(obj, pic, resp.commit_text);
        }
    } else {
        *pf_eaten = 0;
    }
    S_OK
}

unsafe extern "system" fn sink_on_key_up(
    _this: *mut c_void,
    _pic: *mut ITfContext,
    _w_param: usize,
    _l_param: isize,
    pf_eaten: *mut i32,
) -> HRESULT {
    if !pf_eaten.is_null() {
        *pf_eaten = 0;
    }
    S_OK
}

unsafe extern "system" fn sink_on_preserved_key(
    _this: *mut c_void,
    _pic: *mut ITfContext,
    _rguid: *const GUID,
    pf_eaten: *mut i32,
) -> HRESULT {
    if !pf_eaten.is_null() {
        *pf_eaten = 0;
    }
    S_OK
}

// ============================================================================
// 3. IClassFactory 实现
// ============================================================================

#[repr(C)]
struct IClassFactoryVtbl {
    base: IUnknownVtbl,
    create_instance: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    lock_server: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
}

#[repr(C)]
struct TsfClassFactory {
    lp_vtbl: *const IClassFactoryVtbl,
}

unsafe impl Sync for TsfClassFactory {}

static CLASS_FACTORY_VTBL: IClassFactoryVtbl = IClassFactoryVtbl {
    base: IUnknownVtbl {
        query_interface: cf_qi,
        add_ref: cf_add_ref,
        release: cf_release,
    },
    create_instance: cf_create_instance,
    lock_server: cf_lock_server,
};

static STATIC_CLASS_FACTORY: TsfClassFactory = TsfClassFactory {
    lp_vtbl: &CLASS_FACTORY_VTBL,
};

unsafe extern "system" fn cf_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    if *iid == IID_IUNKNOWN || *iid == IID_ICLASS_FACTORY {
        *ppv = this;
        S_OK
    } else {
        *ppv = null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn cf_add_ref(_this: *mut c_void) -> u32 {
    2
}

unsafe extern "system" fn cf_release(_this: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn cf_create_instance(
    _this: *mut c_void,
    p_unk_outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    if !p_unk_outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    let instance = TsfTextServiceCom::new_boxed();
    let hr = tip_qi(instance as *mut c_void, riid, ppv);
    tip_release(instance as *mut c_void);
    hr
}

unsafe extern "system" fn cf_lock_server(_this: *mut c_void, f_lock: i32) -> HRESULT {
    if f_lock != 0 {
        DLL_REF_COUNT.fetch_add(1, Ordering::SeqCst);
    } else {
        DLL_REF_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
    S_OK
}

// ============================================================================
// 4. 标准 COM DLL 导出入口实现 (`DllGetClassObject`, `DllCanUnloadNow`, `DllRegisterServer`, `DllUnregisterServer`)
// ============================================================================

pub unsafe fn com_dll_get_class_object(
    rclsid: *const c_void,
    riid: *const c_void,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() || rclsid.is_null() || riid.is_null() {
        return E_POINTER;
    }
    *ppv = null_mut();
    let clsid = &*(rclsid as *const GUID);
    if *clsid != CLSID_RUST_AI_IME {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    cf_qi(
        (&STATIC_CLASS_FACTORY) as *const _ as *mut c_void,
        riid as *const GUID,
        ppv,
    )
}

pub fn com_dll_can_unload_now() -> HRESULT {
    if DLL_REF_COUNT.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[cfg(target_os = "windows")]
unsafe fn get_this_dll_path() -> Result<String, HRESULT> {
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x0000_0002;
    let mut h_mod: isize = 0;
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        com_dll_get_class_object as *const u16,
        &mut h_mod,
    );
    if ok == 0 || h_mod == 0 {
        return Err(E_FAIL);
    }
    let mut buf = [0u16; 512];
    let len = GetModuleFileNameW(h_mod, buf.as_mut_ptr(), buf.len() as u32);
    if len == 0 {
        return Err(E_FAIL);
    }
    Ok(String::from_utf16_lossy(&buf[..len as usize]))
}

#[cfg(target_os = "windows")]
pub unsafe fn com_dll_register_server() -> HRESULT {
    let dll_path = match get_this_dll_path() {
        Ok(p) => p,
        Err(hr) => return hr,
    };
    if let Some(parent_dir) = std::path::Path::new(&dll_path).parent() {
        ensure_appcontainer_dir_acl(parent_dir);
    }

    // 1. 写入 HKLM\SOFTWARE\Classes\CLSID\{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}\InprocServer32
    const HKEY_LOCAL_MACHINE: isize = -2147483646isize; // 0x80000002
    const KEY_WRITE: u32 = 0x20006;
    const REG_SZ: u32 = 1;

    let clsid_key = to_wide("SOFTWARE\\Classes\\CLSID\\{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}");
    let inproc_key = to_wide(
        "SOFTWARE\\Classes\\CLSID\\{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}\\InprocServer32",
    );
    let desc_w = to_wide("Rust AI 拼音输入法 (TSF)");
    let dll_w = to_wide(&dll_path);
    let model_name_w = to_wide("ThreadingModel");
    let apartment_w = to_wide("Apartment");

    let mut h_key: isize = 0;
    let mut disp: u32 = 0;
    if RegCreateKeyExW(
        HKEY_LOCAL_MACHINE,
        clsid_key.as_ptr(),
        0,
        null(),
        0,
        KEY_WRITE,
        null(),
        &mut h_key,
        &mut disp,
    ) != 0
    {
        return E_FAIL;
    }
    let _ = RegSetValueExW(
        h_key,
        null(),
        0,
        REG_SZ,
        desc_w.as_ptr() as *const u8,
        (desc_w.len() * 2) as u32,
    );
    RegCloseKey(h_key);

    if RegCreateKeyExW(
        HKEY_LOCAL_MACHINE,
        inproc_key.as_ptr(),
        0,
        null(),
        0,
        KEY_WRITE,
        null(),
        &mut h_key,
        &mut disp,
    ) != 0
    {
        return E_FAIL;
    }
    let _ = RegSetValueExW(
        h_key,
        null(),
        0,
        REG_SZ,
        dll_w.as_ptr() as *const u8,
        (dll_w.len() * 2) as u32,
    );
    let _ = RegSetValueExW(
        h_key,
        model_name_w.as_ptr(),
        0,
        REG_SZ,
        apartment_w.as_ptr() as *const u8,
        (apartment_w.len() * 2) as u32,
    );
    RegCloseKey(h_key);

    // 2. 调用 TSF ITfInputProcessorProfiles 注册输入法与简体中文 (0x0804) Profile
    let _ = CoInitializeEx(null_mut(), 0x2 /* COINIT_APARTMENTTHREADED */);
    let mut profiles: *mut ITfInputProcessorProfiles = null_mut();
    if CoCreateInstance(
        &CLSID_TF_INPUT_PROCESSOR_PROFILES,
        null_mut(),
        1 /* CLSCTX_INPROC_SERVER */,
        &IID_ITF_INPUT_PROCESSOR_PROFILES,
        &mut profiles as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !profiles.is_null()
    {
        let _ = ((*(*profiles).lp_vtbl).register)(profiles, &CLSID_RUST_AI_IME);
        let name_str = "Rust AI 拼音输入法";
        let name_utf16 = to_wide(name_str);
        let icon_file_str = std::path::Path::new(&dll_path)
            .parent()
            .map(|p| p.join("rust_ai_ime.ico"))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| dll_path.clone());
        let icon_utf16 = to_wide(&icon_file_str);
        let _ = ((*(*profiles).lp_vtbl).add_language_profile)(
            profiles,
            &CLSID_RUST_AI_IME,
            0x0804, // zh-CN
            &GUID_RUST_AI_PROFILE,
            name_utf16.as_ptr(),
            (name_utf16.len() - 1) as u32,
            icon_utf16.as_ptr(),
            (icon_utf16.len() - 1) as u32,
            0,
        );
        let _ = ((*(*profiles).lp_vtbl).enable_language_profile)(
            profiles,
            &CLSID_RUST_AI_IME,
            0x0804,
            &GUID_RUST_AI_PROFILE,
            1,
        );
        let _ = ((*(*profiles).lp_vtbl).enable_language_profile_by_default)(
            profiles,
            &CLSID_RUST_AI_IME,
            0x0804,
            &GUID_RUST_AI_PROFILE,
            1,
        );
        let _ = ((*(*profiles).lp_vtbl).set_default_language_profile)(
            profiles,
            0x0804,
            &CLSID_RUST_AI_IME,
            &GUID_RUST_AI_PROFILE,
        );
        ((*(*profiles).lp_vtbl).base.release)(profiles as *mut c_void);
    }

    // 3. 调用 ITfCategoryMgr 注册键盘输入法类别与 UWP/系统托盘支持类别
    let mut cat_mgr: *mut ITfCategoryMgr = null_mut();
    if CoCreateInstance(
        &CLSID_TF_CATEGORY_MGR,
        null_mut(),
        1,
        &IID_ITF_CATEGORY_MGR,
        &mut cat_mgr as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !cat_mgr.is_null()
    {
        for cat in [
            &GUID_TFCAT_TIP_KEYBOARD,
            &GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
            &GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
            &GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
            &GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
            &GUID_TFCAT_TIPCAP_COMLESS,
            &GUID_TFCAT_TIPCAP_SECUREMODE,
        ] {
            let _ = ((*(*cat_mgr).lp_vtbl).register_category)(
                cat_mgr,
                &CLSID_RUST_AI_IME,
                cat,
                &CLSID_RUST_AI_IME,
            );
        }
        ((*(*cat_mgr).lp_vtbl).base.release)(cat_mgr as *mut c_void);
    }

    install_layout_or_tip(true);

    CoUninitialize();
    S_OK
}

/// 调用 Windows `input.dll!InstallLayoutOrTip` 将本输入法正式写入当前用户 `Control Panel\International\User Profile\zh-Hans-CN`
/// 与 `CTF\SortOrder\AssemblyItem`（对齐小狼毫 Weasel 与微软拼音在 Win10/Win11 下的用户态输入法列表注册）
#[cfg(target_os = "windows")]
unsafe fn install_layout_or_tip(install: bool) {
    let dll_w = to_wide("input.dll");
    let h_mod = LoadLibraryW(dll_w.as_ptr());
    if h_mod != 0 {
        let p = GetProcAddress(h_mod, b"InstallLayoutOrTip\0".as_ptr());
        if !p.is_null() {
            type InstallLayoutOrTipFn = unsafe extern "system" fn(*const u16, u32) -> i32;
            let func: InstallLayoutOrTipFn = std::mem::transmute(p);
            let tip_id = to_wide(
                "0804:{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}",
            );
            // flags: 0 = 安装并启用, 1 = ILOT_UNINSTALL (卸载)
            let _ = func(tip_id.as_ptr(), if install { 0 } else { 1 });
        }
        FreeLibrary(h_mod);
    }
}

#[cfg(target_os = "windows")]
pub unsafe fn com_dll_unregister_server() -> HRESULT {
    let _ = CoInitializeEx(null_mut(), 0x2);

    install_layout_or_tip(false);

    let mut cat_mgr: *mut ITfCategoryMgr = null_mut();
    if CoCreateInstance(
        &CLSID_TF_CATEGORY_MGR,
        null_mut(),
        1,
        &IID_ITF_CATEGORY_MGR,
        &mut cat_mgr as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !cat_mgr.is_null()
    {
        for cat in [
            &GUID_TFCAT_TIP_KEYBOARD,
            &GUID_TFCAT_TIPCAP_IMMERSIVESUPPORT,
            &GUID_TFCAT_TIPCAP_SYSTRAYSUPPORT,
            &GUID_TFCAT_TIPCAP_UIELEMENTENABLED,
            &GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT,
            &GUID_TFCAT_TIPCAP_COMLESS,
            &GUID_TFCAT_TIPCAP_SECUREMODE,
        ] {
            let _ = ((*(*cat_mgr).lp_vtbl).unregister_category)(
                cat_mgr,
                &CLSID_RUST_AI_IME,
                cat,
                &CLSID_RUST_AI_IME,
            );
        }
        ((*(*cat_mgr).lp_vtbl).base.release)(cat_mgr as *mut c_void);
    }

    let mut profiles: *mut ITfInputProcessorProfiles = null_mut();
    if CoCreateInstance(
        &CLSID_TF_INPUT_PROCESSOR_PROFILES,
        null_mut(),
        1,
        &IID_ITF_INPUT_PROCESSOR_PROFILES,
        &mut profiles as *mut *mut _ as *mut *mut c_void,
    ) == S_OK
        && !profiles.is_null()
    {
        let _ = ((*(*profiles).lp_vtbl).remove_language_profile)(
            profiles,
            &CLSID_RUST_AI_IME,
            0x0804,
            &GUID_RUST_AI_PROFILE,
        );
        let _ = ((*(*profiles).lp_vtbl).unregister)(profiles, &CLSID_RUST_AI_IME);
        ((*(*profiles).lp_vtbl).base.release)(profiles as *mut c_void);
    }

    const HKEY_LOCAL_MACHINE: isize = -2147483646isize;
    let clsid_key = to_wide("SOFTWARE\\Classes\\CLSID\\{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}");
    let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, clsid_key.as_ptr());

    CoUninitialize();
    S_OK
}

/// 检查当前系统中是否已成功注册本 TSF COM 输入法
pub fn is_tsf_com_registered() -> bool {
    #[cfg(target_os = "windows")]
    unsafe {
        const HKEY_LOCAL_MACHINE: isize = -2147483646isize;
        const KEY_READ: u32 = 0x20019;
        #[link(name = "advapi32")]
        extern "system" {
            fn RegOpenKeyExW(
                h_key: isize,
                lp_sub_key: *const u16,
                ul_options: u32,
                sam_desired: u32,
                phk_result: *mut isize,
            ) -> i32;
        }
        let sub = to_wide(
            "SOFTWARE\\Classes\\CLSID\\{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}\\InprocServer32",
        );
        let mut hk: isize = 0;
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hk) == 0 && hk != 0 {
            RegCloseKey(hk);
            return true;
        }
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

#[repr(C)]
struct ITfInputProcessorProfileMgrVtbl {
    base: IUnknownVtbl,
    activate_profile: unsafe extern "system" fn(
        *mut ITfInputProcessorProfileMgr,
        u32,
        u16,
        *const GUID,
        *const GUID,
        isize,
        u32,
    ) -> HRESULT,
}

#[repr(C)]
struct ITfInputProcessorProfileMgr {
    lp_vtbl: *const ITfInputProcessorProfileMgrVtbl,
}

/// 在当前用户会话中启用并激活本 TSF 拼音输入法（使 Win+Space 语言栏立即生效）
pub fn activate_tsf_profile_for_current_user() -> bool {
    #[cfg(target_os = "windows")]
    unsafe {
        let _ = CoInitializeEx(null_mut(), 0x2);
        install_layout_or_tip(true);
        let mut ok = false;

        let mut profiles: *mut ITfInputProcessorProfiles = null_mut();
        if CoCreateInstance(
            &CLSID_TF_INPUT_PROCESSOR_PROFILES,
            null_mut(),
            1,
            &IID_ITF_INPUT_PROCESSOR_PROFILES,
            &mut profiles as *mut *mut _ as *mut *mut c_void,
        ) == S_OK
            && !profiles.is_null()
        {
            let _ = ((*(*profiles).lp_vtbl).enable_language_profile)(
                profiles,
                &CLSID_RUST_AI_IME,
                0x0804,
                &GUID_RUST_AI_PROFILE,
                1,
            );
            let _ = ((*(*profiles).lp_vtbl).enable_language_profile_by_default)(
                profiles,
                &CLSID_RUST_AI_IME,
                0x0804,
                &GUID_RUST_AI_PROFILE,
                1,
            );
            let _ = ((*(*profiles).lp_vtbl).set_default_language_profile)(
                profiles,
                0x0804,
                &CLSID_RUST_AI_IME,
                &GUID_RUST_AI_PROFILE,
            );
            let _ = ((*(*profiles).lp_vtbl).activate_language_profile)(
                profiles,
                &CLSID_RUST_AI_IME,
                0x0804,
                &GUID_RUST_AI_PROFILE,
            );

            let mut mgr: *mut ITfInputProcessorProfileMgr = null_mut();
            if ((*(*profiles).lp_vtbl).base.query_interface)(
                profiles as *mut c_void,
                &IID_ITF_INPUT_PROCESSOR_PROFILE_MGR,
                &mut mgr as *mut *mut _ as *mut *mut c_void,
            ) == S_OK
                && !mgr.is_null()
            {
                // TF_PROFILETYPE_INPUTPROCESSOR = 1
                // msctf.h 标准常量：
                // TF_IPPMF_FORPROCESS = 0x1000_0000
                // TF_IPPMF_FORSESSION = 0x2000_0000
                // TF_IPPMF_ENABLEPROFILE = 0x0000_0001
                // TF_IPPMF_DONTCARECURRENTINPUTLANGUAGE = 0x0000_0004
                let hr = ((*(*mgr).lp_vtbl).activate_profile)(
                    mgr,
                    1,
                    0x0804,
                    &CLSID_RUST_AI_IME,
                    &GUID_RUST_AI_PROFILE,
                    0,
                    0x2000_0000 | 0x1000_0000 | 0x0000_0001 | 0x0000_0004,
                );
                if hr == S_OK {
                    ok = true;
                }
                ((*(*mgr).lp_vtbl).base.release)(mgr as *mut c_void);
            }
            ((*(*profiles).lp_vtbl).base.release)(profiles as *mut c_void);
        }
        CoUninitialize();
        ok
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}
