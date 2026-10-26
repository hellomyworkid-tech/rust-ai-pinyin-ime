//! `managed_llm`: 免安装 Ollama 的输入法内置大模型下载器与后台推理进程管理器
//!
//! 架构设计（业界最优解）：
//! 1. **为什么不直接塞进安装包，也不捆绑 800MB 的 Ollama 安装器？**
//!    - 捆绑 Ollama 会额外安装系统托盘服务与数百 MB 冗余运行时；
//!    - 将模型直接打入安装器会导致每次更新小程序都要重拷 1GB 文件。
//! 2. **最优解：「轻量安装包 + 设置内一键高速下载 GGUF 模型与 Vulkan GPU 引擎 + 随输入法自动启停」**
//!    - **推理引擎**：采用官方 `llama-server.exe` (Vulkan GPU 加速版，仅 ~35MB)，直接调用 Windows 自带的
//!      NVIDIA RTX 4070 / AMD / Intel Arc 显卡驱动 (`vulkan-1.dll`) 进行 `-ngl 99` 全层 GPU 卸载，无需安装龐大的 CUDA Toolkit！
//!    - **模型源**：直接对接国内阿里云魔搭社区（ModelScope）高速 CDN 直链，满速下载 `Qwen2.5-0.5B-Instruct` (398MB)
//!      或 `Qwen2.5-1.5B-Instruct` (986MB)，下载完成后由输入法静默托管在 `127.0.0.1:18434` 端口，真正开箱即用！

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 内置托管推理服务的专用本地端口（避免与用户自装的 11434 Ollama 或 8080 端口冲突）
pub const MANAGED_LLM_PORT: u16 = 18434;
pub const MANAGED_LLM_BASE_URL: &str = "http://127.0.0.1:18434/v1";

/// 内置一键下载模型档位
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinModelTier {
    /// 极速轻量版：Qwen2.5-0.5B-Instruct-Q4_K_M (~398 MB)，适合快速下载、核显/CPU/独显通用
    Qwen05B,
    /// 旗舰高准版：Qwen2.5-1.5B-Instruct-Q4_K_M (~986 MB)，适合 RTX 4070 等独显，中英代码词汇量最佳
    Qwen15B,
}

impl BuiltinModelTier {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Qwen05B => "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            Self::Qwen15B => "qwen2.5-1.5b-instruct-q4_k_m.gguf",
        }
    }

    pub fn model_id(self) -> &'static str {
        match self {
            Self::Qwen05B => "qwen2.5-0.5b-builtin",
            Self::Qwen15B => "qwen2.5-1.5b-builtin",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Qwen05B => "Qwen2.5-0.5B 极速内置版 (约 398MB)",
            Self::Qwen15B => "Qwen2.5-1.5B 旗舰内置版 (约 986MB · RTX4070 推荐)",
        }
    }

    /// 用于状态栏等窄区域的短名称
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Qwen05B => "Qwen2.5-0.5B",
            Self::Qwen15B => "Qwen2.5-1.5B",
        }
    }

    pub const ALL: [BuiltinModelTier; 2] = [BuiltinModelTier::Qwen05B, BuiltinModelTier::Qwen15B];

    /// 根据模型名称（如 `qwen2.5-1.5b-builtin`）反查内置档位
    pub fn from_model_id(model: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.model_id() == model.trim())
    }

    pub fn expected_bytes(self) -> u64 {
        match self {
            Self::Qwen05B => 397_800_000,
            Self::Qwen15B => 986_000_000,
        }
    }

    pub fn download_urls(self) -> &'static [&'static str] {
        match self {
            Self::Qwen05B => &[
                "https://modelscope.cn/models/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/master/qwen2.5-0.5b-instruct-q4_k_m.gguf",
                "https://hf-mirror.com/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf",
            ],
            Self::Qwen15B => &[
                "https://modelscope.cn/models/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/master/qwen2.5-1.5b-instruct-q4_k_m.gguf",
                "https://hf-mirror.com/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf",
            ],
        }
    }
}

const LLAMA_SERVER_ZIP_URLS: &[&str] = &[
    "https://ghfast.top/https://github.com/ggml-org/llama.cpp/releases/download/b11803/llama-b11803-bin-win-vulkan-x64.zip",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11803/llama-b11803-bin-win-vulkan-x64.zip",
    "https://github.com/ggml-org/llama.cpp/releases/download/b11803/llama-b11803-bin-win-cpu-x64.zip",
];

pub struct ManagedLocalLlm;

impl ManagedLocalLlm {
    /// 获取内置本地模型与引擎的存储根目录 (`%LOCALAPPDATA%\RustAiPinyinIME\local_llm`)
    pub fn root_dir() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("RustAiPinyinIME").join("local_llm")
    }

    pub fn engine_exe_path() -> PathBuf {
        Self::root_dir().join("llama-server.exe")
    }

    pub fn model_path(tier: BuiltinModelTier) -> PathBuf {
        Self::root_dir().join("models").join(tier.filename())
    }

    /// 检查某档位内置模型与引擎是否均已下载就绪
    pub fn is_tier_downloaded(tier: BuiltinModelTier) -> bool {
        let exe_ok = Self::engine_exe_path().exists();
        let model_ok = fs::metadata(Self::model_path(tier))
            .map(|m| m.len() > 100 * 1024 * 1024)
            .unwrap_or(false);
        exe_ok && model_ok
    }

    /// 检查内置推理服务端口 (`127.0.0.1:18434`) 当前是否在线
    pub fn is_server_running() -> bool {
        let addr = SocketAddr::from(([127, 0, 0, 1], MANAGED_LLM_PORT));
        TcpStream::connect_timeout(&addr, Duration::from_millis(60)).is_ok()
    }

    /// 停止由输入法托管的 `llama-server.exe` 后台进程
    pub fn stop_server() {
        #[cfg(target_os = "windows")]
        {
            let mut cmd = Command::new("taskkill");
            cmd.args(["/F", "/IM", "llama-server.exe"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW);
            let _ = cmd.status();
        }
    }

    /// 若用户之前已下载并启用了内置模型，在输入法启动时静默拉起 `llama-server.exe`
    pub fn auto_start_if_configured(api_base: &str, model_name: &str, enabled: bool) {
        if !enabled || !api_base.contains("18434") {
            return;
        }
        if Self::is_server_running() {
            return;
        }
        let tier = if model_name.contains("1.5b") {
            BuiltinModelTier::Qwen15B
        } else {
            BuiltinModelTier::Qwen05B
        };
        if Self::is_tier_downloaded(tier) {
            std::thread::spawn(move || {
                let _ = Self::start_server_for_tier(tier);
            });
        }
    }

    /// 一键下载（若尚未下载）并启动指定档位的内置 GGUF 模型与 Vulkan GPU 推理引擎
    pub fn download_and_start<F>(tier: BuiltinModelTier, on_progress: F) -> Result<(), String>
    where
        F: Fn(&str),
    {
        let root = Self::root_dir();
        let models_dir = root.join("models");
        fs::create_dir_all(&models_dir).map_err(|e| format!("创建目录失败: {e}"))?;

        // 1. 确保 llama-server.exe (Vulkan GPU 版) 已就绪
        if !Self::engine_exe_path().exists() {
            on_progress("正在下载推理引擎 llama.cpp（约 35 MB）…");
            let zip_path = root.join("llama-engine.zip");
            let mut engine_ok = false;
            for &url in LLAMA_SERVER_ZIP_URLS {
                if download_file_with_progress(
                    url,
                    &zip_path,
                    36_865_376,
                    "下载推理引擎",
                    &on_progress,
                )
                .is_ok()
                {
                    on_progress("正在解压推理引擎…");
                    if extract_zip_archive(&zip_path, &root).is_ok()
                        && Self::engine_exe_path().exists()
                    {
                        let _ = fs::remove_file(&zip_path);
                        engine_ok = true;
                        break;
                    }
                }
            }
            if !engine_ok {
                return Err("下载或解压内置推理引擎 (llama-server.exe) 失败，请检查网络连接".to_string());
            }
        }

        // 2. 确保对应的 Qwen2.5 GGUF 模型文件已下载
        let target_model = Self::model_path(tier);
        let model_exists = fs::metadata(&target_model)
            .map(|m| m.len() > 100 * 1024 * 1024)
            .unwrap_or(false);

        if !model_exists {
            let mut model_ok = false;
            for &url in tier.download_urls() {
                if download_file_with_progress(
                    url,
                    &target_model,
                    tier.expected_bytes(),
                    &format!("下载 {}", tier.short_name()),
                    &on_progress,
                )
                .is_ok()
                {
                    model_ok = true;
                    break;
                }
            }
            if !model_ok {
                return Err(format!("下载 {} 失败，请检查网络连接", tier.short_name()));
            }
        }

        // 3. 启动内置 llama-server 进程
        on_progress(&format!("正在启动 {}（GPU 加速）…", tier.short_name()));
        Self::start_server_for_tier(tier)?;
        on_progress(&format!(
            "已启用 {}，本地 AI 选词已生效。",
            tier.short_name()
        ));
        Ok(())
    }

    pub fn start_server_for_tier(tier: BuiltinModelTier) -> Result<(), String> {
        let exe = Self::engine_exe_path();
        let model = Self::model_path(tier);
        if !exe.exists() {
            return Err("未找到内置推理引擎 llama-server.exe".to_string());
        }
        if !model.exists() {
            return Err(format!("未找到模型文件: {}", model.display()));
        }

        // 先停止旧实例，确保切换 0.5B / 1.5B 模型时加载最新权重
        Self::stop_server();
        std::thread::sleep(Duration::from_millis(200));

        let mut cmd = Command::new(&exe);
        cmd.current_dir(Self::root_dir())
            .args([
                "-m",
                &model.to_string_lossy(),
                "--host",
                "127.0.0.1",
                "--port",
                &MANAGED_LLM_PORT.to_string(),
                "-ngl",
                "99",
                "-c",
                "1024",
                "-np",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(target_os = "windows")]
        {
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        cmd.spawn()
            .map_err(|e| format!("启动 llama-server.exe 失败: {e}"))?;

        // 等待服务端口就绪（最长 10 秒）
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if Self::is_server_running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(180));
        }

        Err("内置推理引擎启动超时（端口 18434 未响应）".to_string())
    }
}

fn download_file_with_progress<F>(
    url: &str,
    dest: &PathBuf,
    expected_bytes: u64,
    stage_label: &str,
    on_progress: &F,
) -> Result<(), String>
where
    F: Fn(&str),
{
    let part_path = dest.with_extension("part");
    let mut cmd = Command::new("curl.exe");
    cmd.args([
        "-L",
        "--fail",
        "--retry",
        "2",
        "--connect-timeout",
        "10",
        "-o",
        &part_path.to_string_lossy(),
        url,
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法调用系统下载器 curl.exe: {e}"))?;

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    let final_len = fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
                    if final_len < 1024 * 1024 {
                        let _ = fs::remove_file(&part_path);
                        return Err("下载的文件体积异常偏小".to_string());
                    }
                    let _ = fs::remove_file(dest);
                    fs::rename(&part_path, dest)
                        .map_err(|e| format!("重命名下载文件失败: {e}"))?;
                    return Ok(());
                } else {
                    let _ = fs::remove_file(&part_path);
                    return Err(format!("下载返回非零状态码: {status}"));
                }
            }
            Ok(None) => {
                if let Ok(meta) = fs::metadata(&part_path) {
                    let cur_bytes = meta.len();
                    let cur_mb = cur_bytes as f64 / (1024.0 * 1024.0);
                    let total_mb = expected_bytes.max(1) as f64 / (1024.0 * 1024.0);
                    let pct = ((cur_bytes as f64 / expected_bytes.max(1) as f64) * 100.0).min(99.9);
                    let secs = start.elapsed().as_secs_f64().max(0.5);
                    let speed_mb = cur_mb / secs;
                    on_progress(&format!(
                        "{stage_label}  {pct:.0}%  ·  {cur_mb:.0}/{total_mb:.0} MB  ·  {speed_mb:.1} MB/s"
                    ));
                }
                std::thread::sleep(Duration::from_millis(350));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待下载进程失败: {e}"));
            }
        }
    }
}

fn extract_zip_archive(zip_path: &PathBuf, dest_dir: &PathBuf) -> Result<(), String> {
    // 1. 优先使用 Windows 10/11 内置极速 `tar.exe -xf` 解压
    let mut cmd = Command::new("tar.exe");
    cmd.args([
        "-xf",
        &zip_path.to_string_lossy(),
        "-C",
        &dest_dir.to_string_lossy(),
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    if let Ok(status) = cmd.status() {
        if status.success() {
            flatten_nested_llama_dir(dest_dir);
            if dest_dir.join("llama-server.exe").exists() {
                return Ok(());
            }
        }
    }

    // 2. 兜底使用 PowerShell Expand-Archive
    let ps_cmd = format!(
        "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
        zip_path.display(),
        dest_dir.display()
    );
    let mut ps = Command::new("powershell");
    ps.args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps_cmd])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        ps.creation_flags(CREATE_NO_WINDOW);
    }

    let status = ps.status().map_err(|e| format!("解压失败: {e}"))?;
    if !status.success() {
        return Err("解压压缩包失败".to_string());
    }
    flatten_nested_llama_dir(dest_dir);
    Ok(())
}

/// 若压缩包内含有一层子目录（如 `build/bin/llama-server.exe`），将其中的 `.exe` 与 `.dll` 提升至 `dest_dir` 根目录
fn flatten_nested_llama_dir(dest_dir: &PathBuf) {
    if dest_dir.join("llama-server.exe").exists() {
        return;
    }
    fn walk_and_move(curr: &PathBuf, root: &PathBuf) {
        let Ok(entries) = fs::read_dir(curr) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_and_move(&path, root);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let lower = ext.to_ascii_lowercase();
                if matches!(lower.as_str(), "exe" | "dll") {
                    if let Some(name) = path.file_name() {
                        let _ = fs::rename(&path, root.join(name));
                    }
                }
            }
        }
    }
    walk_and_move(dest_dir, dest_dir);
}
