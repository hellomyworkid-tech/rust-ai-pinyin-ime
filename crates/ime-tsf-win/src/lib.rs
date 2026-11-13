//! `ime-tsf-win`: Windows TSF (Text Services Framework) 输入法外壳与 COM 导出接口
//!
//! 功能：
//! 1. **TSF 按键事件处理器 (`TsfKeySession`)**：
//!    状态机本体已抽到平台无关的 `ime-session` crate（`KeySession`），这里以类型别名与 re-export 保持原有导出不变。
//! 2. **焦点应用程序场景感知 (`detect_scene_from_process`)**：同上，实现位于 `ime-session`。
//! 3. **候选框 UI 视图模型 (`CandidateWindowViewModel`)**：同上，实现位于 `ime-session`。
//! 4. **Windows COM DLL 标准导出函数**：
//!    提供 `DllGetClassObject`, `DllCanUnloadNow`, `DllRegisterServer`, `DllUnregisterServer` 导出入口。

pub mod tsf_com;
pub use tsf_com::{
    activate_tsf_profile_for_current_user, ensure_appcontainer_dir_acl,
    invalidate_tsf_active_cache, is_our_tip_active_in_tsf, is_shell_appcontainer_host,
    is_tsf_com_registered, send_tsf_ipc, TsfIpcRequest, TsfIpcResponse, TSF_IPC_PORT,
    WM_APP_TSF_TIP_STATE,
};

pub use ime_session::{
    detect_scene_from_process, CandidateWindowViewModel, ImePersistentConfig, KeyAction,
    KeySession, VirtualKey,
};

/// 历史名称：Windows 侧沿用 `TsfKeySession`，与 `ime_session::KeySession` 完全相同
pub type TsfKeySession = KeySession;

use std::ffi::c_void;

/// 输入法唯一 CLSID: `{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}`
pub const IME_CLSID_STR: &str = "{8E7A9C12-4F5B-4D8A-9E2C-1B3D5F7A9E01}";
/// 输入法 Profile GUID: `{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}`
pub const IME_PROFILE_GUID_STR: &str = "{9F8B0D23-5A6C-4E9B-AF3D-2C4E6F8B0A12}";

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
