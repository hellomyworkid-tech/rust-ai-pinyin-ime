//! Native Windows GUI Installer (`RustAI拼音输入法_安装程序.exe`) for the Rust AI Pinyin IME.
//!
//! Installs the Global System-Wide IME Service (`RustAI拼音输入法_全局版.exe`),
//! the 571,673-entry compiled lexicon (`data/compiled/merged_lexicon.bin`),
//! the Interactive Desktop Workbench (`AI拼音输入法桌面体验台.exe`),
//! Desktop shortcuts, and an uninstaller script into `%LOCALAPPDATA%\RustAiPinyinIME`.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("This installer requires Windows.");
}

#[cfg(target_os = "windows")]
fn main() {
    win32_installer::run();
}

#[cfg(target_os = "windows")]
mod win32_installer {
    use std::cell::RefCell;
    use std::env;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::ptr::null;

    type HWND = isize;
    type HDC = isize;
    type HFONT = isize;
    type HBRUSH = isize;
    type HPEN = isize;
    type HGDIOBJ = isize;
    type WPARAM = usize;
    type LPARAM = isize;
    type LRESULT = isize;

    const WS_OVERLAPPED: u32 = 0x00000000;
    const WS_CAPTION: u32 = 0x00C00000;
    const WS_SYSMENU: u32 = 0x00080000;
    const WS_MINIMIZEBOX: u32 = 0x00020000;
    const WS_VISIBLE: u32 = 0x10000000;
    const WS_CLIPCHILDREN: u32 = 0x02000000;
    const WS_EX_APPWINDOW: u32 = 0x00040000;
    const CS_HREDRAW: u32 = 0x0002;
    const CS_VREDRAW: u32 = 0x0001;

    const WM_DESTROY: u32 = 0x0002;
    const WM_PAINT: u32 = 0x000F;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_LBUTTONDOWN: u32 = 0x0201;

    const TRANSPARENT: i32 = 1;
    const DT_LEFT: u32 = 0x0000;
    const DT_CENTER: u32 = 0x0001;
    const DT_VCENTER: u32 = 0x0004;
    const DT_SINGLELINE: u32 = 0x0020;
    const DT_WORDBREAK: u32 = 0x0010;
    const DT_NOPREFIX: u32 = 0x0800;

    const SW_SHOW: i32 = 5;
    const PS_SOLID: i32 = 0;
    const NULL_BRUSH: i32 = 5;
    const IDC_ARROW: *const u16 = 32512 as *const u16;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct PAINTSTRUCT {
        hdc: HDC,
        f_erase: i32,
        rc_paint: RECT,
        f_restore: i32,
        f_inc_update: i32,
        rgb_reserved: [u8; 32],
    }

    #[repr(C)]
    struct MSG {
        hwnd: HWND,
        message: u32,
        w_param: WPARAM,
        l_param: LPARAM,
        time: u32,
        pt: POINT,
    }

    #[repr(C)]
    struct WNDCLASSEXW {
        cb_size: u32,
        style: u32,
        lpfn_wnd_proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: isize,
        h_icon: isize,
        h_cursor: isize,
        hbr_background: HBRUSH,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
        h_icon_sm: isize,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetProcessDPIAware() -> i32;
        fn OpenDesktopW(lpsz_desktop: *const u16, dw_flags: u32, f_inherit: i32, dw_desired_access: u32) -> isize;
        fn SetThreadDesktop(h_desktop: isize) -> i32;
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
            h_wnd_parent: HWND,
            h_menu: isize,
            h_instance: isize,
            lp_param: *const u8,
        ) -> HWND;
        fn DefWindowProcW(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;
        fn ShowWindow(hwnd: HWND, n_cmd_show: i32) -> i32;
        fn UpdateWindow(hwnd: HWND) -> i32;
        fn SetForegroundWindow(hwnd: HWND) -> i32;
        fn GetMessageW(lp_msg: *mut MSG, hwnd: HWND, w_msg_filter_min: u32, w_msg_filter_max: u32) -> i32;
        fn TranslateMessage(lp_msg: *const MSG) -> i32;
        fn DispatchMessageW(lp_msg: *const MSG) -> LRESULT;
        fn PostQuitMessage(n_exit_code: i32);
        fn InvalidateRect(hwnd: HWND, lp_rect: *const RECT, b_erase: i32) -> i32;
        fn BeginPaint(hwnd: HWND, lp_paint: *mut PAINTSTRUCT) -> HDC;
        fn EndPaint(hwnd: HWND, lp_paint: *const PAINTSTRUCT) -> i32;
        fn GetClientRect(hwnd: HWND, lp_rect: *mut RECT) -> i32;
        fn FillRect(hdc: HDC, lprc: *const RECT, hbr: HBRUSH) -> i32;
        fn DrawTextW(hdc: HDC, lpch_text: *const u16, cch_text: i32, lprc: *mut RECT, format: u32) -> i32;
        fn LoadCursorW(h_instance: isize, lp_cursor_name: *const u16) -> isize;
        fn GetSystemMetrics(n_index: i32) -> i32;
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn CreateCompatibleDC(hdc: HDC) -> HDC;
        fn CreateCompatibleBitmap(hdc: HDC, cx: i32, cy: i32) -> isize;
        fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
        fn DeleteObject(ho: HGDIOBJ) -> i32;
        fn DeleteDC(hdc: HDC) -> i32;
        fn BitBlt(hdc: HDC, x: i32, y: i32, cx: i32, cy: i32, hdc_src: HDC, x1: i32, y1: i32, rop: u32) -> i32;
        fn CreateSolidBrush(color: u32) -> HBRUSH;
        fn CreatePen(i_style: i32, c_width: i32, color: u32) -> HPEN;
        fn RoundRect(hdc: HDC, left: i32, top: i32, right: i32, bottom: i32, width: i32, height: i32) -> i32;
        fn SetBkMode(hdc: HDC, mode: i32) -> i32;
        fn SetTextColor(hdc: HDC, color: u32) -> u32;
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
        ) -> HFONT;
        fn GetStockObject(i: i32) -> HGDIOBJ;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
    }

    const fn rgb(r: u8, g: u8, b: u8) -> u32 {
        (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
    }

    fn to_wstring(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    #[derive(Clone, Copy)]
    struct Fonts {
        title: HFONT,
        subtitle: HFONT,
        body: HFONT,
        bold: HFONT,
        mono: HFONT,
    }

    impl Fonts {
        unsafe fn new() -> Self {
            let ui_face = to_wstring("Microsoft YaHei UI");
            let mono_face = to_wstring("Consolas");
            unsafe {
                Self {
                    title: CreateFontW(-24, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, ui_face.as_ptr()),
                    subtitle: CreateFontW(-14, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, ui_face.as_ptr()),
                    body: CreateFontW(-15, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, ui_face.as_ptr()),
                    bold: CreateFontW(-16, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 5, 0, ui_face.as_ptr()),
                    mono: CreateFontW(-13, 0, 0, 0, 600, 0, 0, 0, 1, 0, 0, 5, 0, mono_face.as_ptr()),
                }
            }
        }
    }

    struct InstallerState {
        install_dir: PathBuf,
        source_dir: PathBuf,
        status_title: String,
        status_logs: Vec<String>,
        installed_ok: bool,
        btn_install: RECT,
        btn_run_direct: RECT,
        btn_open_dir: RECT,
        btn_uninstall: RECT,
        fonts: Option<Fonts>,
    }

    impl InstallerState {
        fn new() -> Self {
            let install_dir = env::var_os("LOCALAPPDATA")
                .map(|p| PathBuf::from(p).join("RustAiPinyinIME"))
                .unwrap_or_else(|| PathBuf::from("C:\\RustAiPinyinIME"));

            let source_dir = env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            let already_installed = install_dir.join("RustAI拼音输入法_全局版.exe").exists();

            Self {
                install_dir,
                source_dir,
                status_title: if already_installed {
                    "✅ 检测到已安装版本，可点击【一键更新安装并启动】或直接运行".to_string()
                } else {
                    "✨ 准备就绪：点击下方【一键安装并立即启用全局输入法】即可在任意软件中打字".to_string()
                },
                status_logs: vec![
                    "• 包含组件 1: 全局输入法服务 (支持记事本 / 微信 / 浏览器 / VS Code / Word 等所有窗口)".to_string(),
                    "• 包含组件 2: 571,673 词条融合词库 (雾凇拼音 + 白霜词库 + 万象 Bigram + 英文词典)".to_string(),
                    "• 包含组件 3: 拼音音节光标移动与原位改错 + 光标跟随选词 + 免切换中英混输 + AI 选词".to_string(),
                    "• 包含组件 4: 桌面快捷方式 + 桌面体验台 + 一键卸载程序".to_string(),
                ],
                installed_ok: already_installed,
                btn_install: RECT::default(),
                btn_run_direct: RECT::default(),
                btn_open_dir: RECT::default(),
                btn_uninstall: RECT::default(),
                fonts: None,
            }
        }

        fn find_source_file(&self, candidates: &[&str]) -> Option<PathBuf> {
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let mut search_roots = vec![
                self.source_dir.clone(),
                self.source_dir.join("dist"),
                cwd.clone(),
                cwd.join("dist"),
            ];
            if let Some(parent) = self.source_dir.parent() {
                search_roots.push(parent.to_path_buf());
            }
            for root in &search_roots {
                for rel in candidates {
                    let p = root.join(rel);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
            None
        }

        fn perform_install(&mut self) -> Result<(), String> {
            self.status_logs.clear();
            let target_dir = &self.install_dir;
            let data_dir = target_dir.join("data").join("compiled");
            fs::create_dir_all(&data_dir).map_err(|e| format!("创建安装目录失败: {e}"))?;
            ime_tsf_win::ensure_appcontainer_dir_acl(target_dir);
            self.status_logs.push(format!("✔ 已创建安装目录: {}", target_dir.display()));

            // Stop existing running instance before overwriting
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "RustAI拼音输入法_全局版.exe"])
                .output();
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "ime-global-service.exe"])
                .output();
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "llama-server.exe"])
                .output();

            // 1. Copy global service executable
            let global_src = self
                .find_source_file(&[
                    "RustAI拼音输入法_全局版.exe",
                    "ime-global-service.exe",
                    "dist\\RustAI拼音输入法_全局版.exe",
                ])
                .ok_or_else(|| "未找到全局输入法程序 (RustAI拼音输入法_全局版.exe)".to_string())?;
            let global_dst = target_dir.join("RustAI拼音输入法_全局版.exe");
            fs::copy(&global_src, &global_dst)
                .map_err(|e| format!("复制全局输入法服务失败: {e}"))?;
            self.status_logs
                .push("✔ 已安装全局输入法守护服务: RustAI拼音输入法_全局版.exe".to_string());

            // 2. Copy TSF COM InprocServer32 DLL (`ime_tsf_win.dll`) if available
            if let Some(dll_src) = self.find_source_file(&[
                "ime_tsf_win.dll",
                "dist\\ime_tsf_win.dll",
                "target\\release\\ime_tsf_win.dll",
            ]) {
                let dll_dst = target_dir.join("ime_tsf_win.dll");
                if fs::copy(&dll_src, &dll_dst).is_err() {
                    let old_bak = target_dir.join(format!("ime_tsf_win.{}.old", std::process::id()));
                    let _ = fs::remove_file(&old_bak);
                    let _ = fs::rename(&dll_dst, &old_bak);
                    let _ = fs::copy(&dll_src, &dll_dst);
                }
                self.status_logs.push(
                    "✔ 已安装 Windows TSF 正式系统输入法 COM 组件: ime_tsf_win.dll".to_string(),
                );
            }
            // 2b. 32 位 TSF 组件（供 WPS Office 等 32 位程序加载）
            if let Some(dll32_src) = self.find_source_file(&[
                "ime_tsf_win_x86.dll",
                "dist\\ime_tsf_win_x86.dll",
            ]) {
                let dll32_dst = target_dir.join("ime_tsf_win_x86.dll");
                if fs::copy(&dll32_src, &dll32_dst).is_err() {
                    let old_bak = target_dir.join(format!("ime_tsf_win_x86.{}.old", std::process::id()));
                    let _ = fs::remove_file(&old_bak);
                    let _ = fs::rename(&dll32_dst, &old_bak);
                    let _ = fs::copy(&dll32_src, &dll32_dst);
                }
                self.status_logs.push(
                    "✔ 已安装 32 位 TSF 组件（WPS 等 32 位程序）: ime_tsf_win_x86.dll".to_string(),
                );
            }

            // 3. Copy desktop workbench if available
            if let Some(ui_src) = self.find_source_file(&[
                "AI拼音输入法桌面体验台.exe",
                "ime-desktop-ui.exe",
                "dist\\AI拼音输入法桌面体验台.exe",
            ]) {
                let ui_dst = target_dir.join("AI拼音输入法桌面体验台.exe");
                let _ = fs::copy(&ui_src, &ui_dst);
                self.status_logs
                    .push("✔ 已安装独立桌面体验台: AI拼音输入法桌面体验台.exe".to_string());
            }

            // 3. Copy compiled lexicon
            if let Some(lex_src) = self.find_source_file(&[
                "data\\compiled\\merged_lexicon.bin",
                "merged_lexicon.bin",
            ]) {
                let lex_dst = data_dir.join("merged_lexicon.bin");
                fs::copy(&lex_src, &lex_dst)
                    .map_err(|e| format!("复制 57.1 万词库失败: {e}"))?;
                self.status_logs
                    .push("✔ 已安装 571,673 词条融合词库: data\\compiled\\merged_lexicon.bin".to_string());
            }

            // 4. Write Uninstaller script
            let uninstall_cmd = target_dir.join("卸载_RustAI拼音输入法.cmd");
            let uninstall_script = format!(
                "@echo off\r\nchcp 65001 >nul\r\necho 正在停止 Rust AI 拼音输入法服务...\r\ntaskkill /F /IM \"RustAI拼音输入法_全局版.exe\" >nul 2>nul\r\ntaskkill /F /IM \"AI拼音输入法桌面体验台.exe\" >nul 2>nul\r\ndel /F /Q \"%USERPROFILE%\\Desktop\\Rust AI 拼音输入法 (全局版).lnk\" >nul 2>nul\r\ndel /F /Q \"%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\Startup\\Rust AI 拼音输入法.lnk\" >nul 2>nul\r\necho 已清理桌面快捷方式、开机自启并停止服务。\r\necho 安装目录: {}\r\npause\r\n",
                target_dir.display()
            );
            let _ = fs::write(&uninstall_cmd, uninstall_script);

            // 5. Create Desktop shortcut via PowerShell WScript.Shell
            if let Some(userprofile) = env::var_os("USERPROFILE") {
                let desktop = PathBuf::from(userprofile).join("Desktop");
                if desktop.exists() {
                    let lnk_path = desktop.join("Rust AI 拼音输入法 (全局版).lnk");
                    let ico_path = target_dir.join("rust_ai_ime.ico");
                    let ps_cmd = format!(
                        "$ws = New-Object -ComObject WScript.Shell; $s = $ws.CreateShortcut('{}'); $s.TargetPath = '{}'; $s.WorkingDirectory = '{}'; $s.IconLocation = '{},0'; $s.Description = 'Rust AI 拼音输入法全局服务'; $s.Save()",
                        lnk_path.display(),
                        global_dst.display(),
                        target_dir.display(),
                        ico_path.display()
                    );
                    let _ = Command::new("powershell")
                        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
                        .output();
                    self.status_logs
                        .push("✔ 已创建桌面快捷方式: Rust AI 拼音输入法 (全局版).lnk".to_string());
                }
            }

            // 6. 开机自启：在当前用户「启动」文件夹创建快捷方式（无需管理员权限）
            if let Some(startup) = startup_shortcut_path() {
                let ps_cmd = format!(
                    "$ws = New-Object -ComObject WScript.Shell; $s = $ws.CreateShortcut('{}'); $s.TargetPath = '{}'; $s.WorkingDirectory = '{}'; $s.Description = 'Rust AI 拼音输入法开机自启'; $s.Save()",
                    startup.display(),
                    global_dst.display(),
                    target_dir.display()
                );
                let _ = Command::new("powershell")
                    .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
                    .output();
                if startup.exists() {
                    self.status_logs
                        .push("✔ 已设置开机自动启动（启动文件夹快捷方式）".to_string());
                }
            }

            // 7. Launch the global IME service immediately!
            Command::new(&global_dst)
                .current_dir(target_dir)
                .spawn()
                .map_err(|e| format!("启动全局输入法服务失败: {e}"))?;

            self.installed_ok = true;
            self.status_title =
                "🎉 安装完成且【全局输入法】已启动！现在请打开记事本/微信/浏览器直接打字体验！"
                    .to_string();
            Ok(())
        }

        fn launch_direct(&mut self) {
            if let Some(global_src) = self.find_source_file(&[
                "RustAI拼音输入法_全局版.exe",
                "ime-global-service.exe",
            ]) {
                let work_dir = global_src
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| self.source_dir.clone());
                let _ = Command::new("taskkill")
                    .args(["/F", "/IM", "RustAI拼音输入法_全局版.exe"])
                    .output();
                if Command::new(&global_src).current_dir(&work_dir).spawn().is_ok() {
                    self.status_title =
                        "⚡ 已直接启动全局输入法（屏幕右下角已出现状态栏），现在可在任意软件中打字！"
                            .to_string();
                }
            }
        }

        fn perform_uninstall(&mut self) {
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "RustAI拼音输入法_全局版.exe"])
                .output();
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "ime-global-service.exe"])
                .output();
            let _ = Command::new("taskkill")
                .args(["/F", "/IM", "llama-server.exe"])
                .output();
            if let Some(userprofile) = env::var_os("USERPROFILE") {
                let lnk = PathBuf::from(userprofile)
                    .join("Desktop")
                    .join("Rust AI 拼音输入法 (全局版).lnk");
                let _ = fs::remove_file(lnk);
            }
            if let Some(startup) = startup_shortcut_path() {
                let _ = fs::remove_file(startup);
            }
            // 仅移除程序本体与静态词库，保留用户的 user_dict.json（个人输入记忆）与 ime_config.json（AI/模糊音配置）
            for file_name in [
                "RustAI拼音输入法_全局版.exe",
                "AI拼音输入法桌面体验台.exe",
                "卸载_RustAI拼音输入法.cmd",
            ] {
                let _ = fs::remove_file(self.install_dir.join(file_name));
            }
            let _ = fs::remove_dir_all(self.install_dir.join("data"));
            self.installed_ok = false;
            self.status_title = "🗑️ 已停止全局输入法服务并卸载程序（已为您保留个人记忆词库与 AI 配置）。".to_string();
            self.status_logs.clear();
            self.status_logs
                .push("• 全局服务进程已终止，快捷方式与程序组件已移除（user_dict.json 与 ime_config.json 已保留）。".to_string());
        }
    }

    thread_local! {
        static STATE: RefCell<InstallerState> = RefCell::new(InstallerState::new());
    }

    /// 当前用户「启动」文件夹中的自启快捷方式路径
    fn startup_shortcut_path() -> Option<PathBuf> {
        env::var_os("APPDATA").map(|p| {
            PathBuf::from(p)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("Startup")
                .join("Rust AI 拼音输入法.lnk")
        })
    }

    unsafe fn draw_round_box(
        hdc: HDC,
        rc: &RECT,
        radius: i32,
        fill_color: u32,
        border_color: u32,
        border_width: i32,
    ) {
        unsafe {
            let brush = CreateSolidBrush(fill_color);
            let pen = CreatePen(PS_SOLID, border_width, border_color);
            let old_b = SelectObject(hdc, brush);
            let old_p = SelectObject(hdc, pen);
            RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
            SelectObject(hdc, old_b);
            SelectObject(hdc, old_p);
            DeleteObject(brush);
            DeleteObject(pen);
        }
    }

    unsafe fn draw_text_line(hdc: HDC, text: &str, rc: &RECT, font: HFONT, color: u32, flags: u32) {
        unsafe {
            let old_f = SelectObject(hdc, font);
            SetTextColor(hdc, color);
            let w = to_wstring(text);
            let mut r = *rc;
            DrawTextW(hdc, w.as_ptr(), (w.len() - 1) as i32, &mut r, flags | DT_NOPREFIX);
            SelectObject(hdc, old_f);
        }
    }

    unsafe fn render(hwnd: HWND, hdc_win: HDC) {
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(hwnd, &mut rc);
            let width = rc.right - rc.left;
            let height = rc.bottom - rc.top;

            let hdc = CreateCompatibleDC(hdc_win);
            let bmp = CreateCompatibleBitmap(hdc_win, width, height);
            let old_bmp = SelectObject(hdc, bmp);
            SetBkMode(hdc, TRANSPARENT);

            let bg_brush = CreateSolidBrush(rgb(13, 19, 33));
            FillRect(hdc, &rc, bg_brush);
            DeleteObject(bg_brush);

            STATE.with(|cell| {
                let mut st = cell.borrow_mut();
                if st.fonts.is_none() {
                    st.fonts = Some(Fonts::new());
                }
                let fonts = *st.fonts.as_ref().unwrap();

                // Header banner
                let header_rc = RECT {
                    left: 24,
                    top: 20,
                    right: width - 24,
                    bottom: 108,
                };
                draw_round_box(hdc, &header_rc, 14, rgb(22, 33, 56), rgb(59, 130, 246), 1);

                draw_text_line(
                    hdc,
                    "🚀 Rust AI 拼音输入法 · Windows 全局安装程序",
                    &RECT {
                        left: 42,
                        top: 32,
                        right: width - 42,
                        bottom: 66,
                    },
                    fonts.title,
                    rgb(248, 250, 252),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                draw_text_line(
                    hdc,
                    "57.1万词条融合词库 · 拼音音节光标改错与联动选词 · 免切换中英混输 · 全局光标跟随悬浮窗",
                    &RECT {
                        left: 42,
                        top: 68,
                        right: width - 42,
                        bottom: 96,
                    },
                    fonts.subtitle,
                    rgb(147, 197, 253),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );

                // Install path box
                let path_box = RECT {
                    left: 24,
                    top: 122,
                    right: width - 24,
                    bottom: 174,
                };
                draw_round_box(hdc, &path_box, 10, rgb(19, 28, 46), rgb(51, 65, 85), 1);
                draw_text_line(
                    hdc,
                    &format!("📂 默认安装位置: {}", st.install_dir.display()),
                    &RECT {
                        left: 40,
                        top: 122,
                        right: width - 170,
                        bottom: 174,
                    },
                    fonts.mono,
                    rgb(226, 232, 240),
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );

                let btn_open = RECT {
                    left: width - 160,
                    top: 132,
                    right: width - 38,
                    bottom: 164,
                };
                draw_round_box(hdc, &btn_open, 8, rgb(30, 41, 59), rgb(71, 85, 105), 1);
                draw_text_line(
                    hdc,
                    "打开目录",
                    &btn_open,
                    fonts.subtitle,
                    rgb(148, 163, 184),
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                st.btn_open_dir = btn_open;

                // Component & status logs box
                let log_box = RECT {
                    left: 24,
                    top: 188,
                    right: width - 24,
                    bottom: 368,
                };
                draw_round_box(hdc, &log_box, 12, rgb(17, 24, 39), rgb(51, 65, 85), 1);

                let status_color = if st.installed_ok {
                    rgb(52, 211, 153)
                } else {
                    rgb(250, 204, 21)
                };
                draw_text_line(
                    hdc,
                    &st.status_title,
                    &RECT {
                        left: 40,
                        top: 200,
                        right: width - 40,
                        bottom: 230,
                    },
                    fonts.bold,
                    status_color,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );

                for (i, line) in st.status_logs.iter().enumerate() {
                    let y = 238 + (i as i32) * 28;
                    draw_text_line(
                        hdc,
                        line,
                        &RECT {
                            left: 44,
                            top: y,
                            right: width - 40,
                            bottom: y + 26,
                        },
                        fonts.body,
                        rgb(203, 213, 225),
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                    );
                }

                // Usage guide box
                let guide_box = RECT {
                    left: 24,
                    top: 380,
                    right: width - 24,
                    bottom: 432,
                };
                draw_round_box(hdc, &guide_box, 10, rgb(23, 37, 84), rgb(59, 130, 246), 1);
                draw_text_line(
                    hdc,
                    "💡 全局使用说明：启动后屏幕右下角会出现状态条。打开记事本/微信/浏览器直接敲字母即可弹出候选窗；按 Shift 切换中/英；按 ←/→ 移动拼音音节光标改错与对应选词。",
                    &RECT {
                        left: 38,
                        top: 386,
                        right: width - 38,
                        bottom: 428,
                    },
                    fonts.subtitle,
                    rgb(219, 234, 254),
                    DT_LEFT | DT_WORDBREAK,
                );

                // Action buttons at bottom
                let btn_install = RECT {
                    left: 24,
                    top: 446,
                    right: 390,
                    bottom: 500,
                };
                draw_round_box(hdc, &btn_install, 10, rgb(37, 99, 235), rgb(96, 165, 250), 2);
                draw_text_line(
                    hdc,
                    "🚀 一键安装并立即启用全局输入法",
                    &btn_install,
                    fonts.bold,
                    rgb(255, 255, 255),
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                st.btn_install = btn_install;

                let btn_direct = RECT {
                    left: 404,
                    top: 446,
                    right: 620,
                    bottom: 500,
                };
                draw_round_box(hdc, &btn_direct, 10, rgb(15, 118, 110), rgb(45, 212, 191), 1);
                draw_text_line(
                    hdc,
                    "⚡ 免安装直接启动",
                    &btn_direct,
                    fonts.bold,
                    rgb(240, 253, 250),
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                st.btn_run_direct = btn_direct;

                let btn_uninst = RECT {
                    left: 634,
                    top: 446,
                    right: width - 24,
                    bottom: 500,
                };
                draw_round_box(hdc, &btn_uninst, 10, rgb(69, 10, 10), rgb(248, 113, 113), 1);
                draw_text_line(
                    hdc,
                    "🗑️ 卸载",
                    &btn_uninst,
                    fonts.bold,
                    rgb(254, 202, 202),
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                st.btn_uninstall = btn_uninst;
            });

            BitBlt(hdc_win, 0, 0, width, height, hdc, 0, 0, 0x00CC0020);
            SelectObject(hdc, old_bmp);
            DeleteObject(bmp);
            DeleteDC(hdc);
        }
    }

    fn pt_in_rect(rc: &RECT, x: i32, y: i32) -> bool {
        x >= rc.left && x <= rc.right && y >= rc.top && y <= rc.bottom
    }

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_ERASEBKGND => 1,
                WM_PAINT => {
                    let mut ps = std::mem::zeroed::<PAINTSTRUCT>();
                    let hdc = BeginPaint(hwnd, &mut ps);
                    render(hwnd, hdc);
                    EndPaint(hwnd, &ps);
                    0
                }
                WM_LBUTTONDOWN => {
                    let x = (lparam & 0xFFFF) as i16 as i32;
                    let y = ((lparam >> 16) & 0xFFFF) as i16 as i32;

                    STATE.with(|cell| {
                        let mut st = cell.borrow_mut();
                        if pt_in_rect(&st.btn_install, x, y) {
                            if let Err(e) = st.perform_install() {
                                st.status_title = format!("❌ 安装出错: {e}");
                            }
                        } else if pt_in_rect(&st.btn_run_direct, x, y) {
                            st.launch_direct();
                        } else if pt_in_rect(&st.btn_open_dir, x, y) {
                            let _ = fs::create_dir_all(&st.install_dir);
                            let _ = Command::new("explorer.exe").arg(&st.install_dir).spawn();
                        } else if pt_in_rect(&st.btn_uninstall, x, y) {
                            st.perform_uninstall();
                        }
                    });
                    InvalidateRect(hwnd, null(), 0);
                    0
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    pub fn run() {
        let args: Vec<String> = env::args().collect();
        if args.iter().any(|a| a == "--self-test") {
            let st = InstallerState::new();
            println!("INSTALLER_SELF_TEST_OK install_dir={}", st.install_dir.display());
            return;
        }
        if args.iter().any(|a| a == "--install-now") {
            let mut st = InstallerState::new();
            match st.perform_install() {
                Ok(()) => {
                    println!("INSTALL_NOW_OK dir={}", st.install_dir.display());
                    return;
                }
                Err(e) => {
                    eprintln!("INSTALL_NOW_ERR: {e}");
                    std::process::exit(1);
                }
            }
        }

        unsafe {
            let default_desk = to_wstring("Default");
            let h_desk = OpenDesktopW(default_desk.as_ptr(), 0, 0, 0x01FF);
            if h_desk != 0 {
                SetThreadDesktop(h_desk);
            }
            SetProcessDPIAware();

            let h_instance = GetModuleHandleW(null());
            let class_name = to_wstring("RustAiImeInstallerWndClass");
            let wcx = WNDCLASSEXW {
                cb_size: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfn_wnd_proc: wnd_proc,
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance,
                h_icon: 0,
                h_cursor: LoadCursorW(0, IDC_ARROW),
                hbr_background: GetStockObject(NULL_BRUSH) as HBRUSH,
                lpsz_menu_name: null(),
                lpsz_class_name: class_name.as_ptr(),
                h_icon_sm: 0,
            };
            RegisterClassExW(&wcx);

            let win_w = 820;
            let win_h = 560;
            let scr_w = GetSystemMetrics(0);
            let scr_h = GetSystemMetrics(1);
            let win_x = ((scr_w - win_w) / 2).max(40);
            let win_y = ((scr_h - win_h) / 2).max(40);

            let title = to_wstring("Rust AI 拼音输入法 - Windows 安装向导 (Setup)");
            let hwnd = CreateWindowExW(
                WS_EX_APPWINDOW,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE | WS_CLIPCHILDREN,
                win_x,
                win_y,
                win_w,
                win_h,
                0,
                0,
                h_instance,
                null(),
            );

            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetForegroundWindow(hwnd);

            let mut msg = std::mem::zeroed::<MSG>();
            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
