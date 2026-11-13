//! `llama` 特性开启时编译 C++ 垫片（shim/ime_llm_shim.cpp）并输出 llama.cpp 静态库的链接指令；
//! 特性关闭时什么都不做，整个 crate 在任何平台（含 Windows）都是纯 Rust。
//!
//! 环境变量（仅在 `llama` 特性开启时读取）：
//! - `IME_LLM_LLAMA_INCLUDE_DIR`：包含 `llama.h` 与 `ggml*.h` 的目录。
//!   默认 `<仓库>/third_party/llama/include`（scripts/build_llama.sh 的输出）
//! - `IME_LLM_LLAMA_LIB_DIR`：llama.cpp 静态库所在目录。
//!   默认 `<仓库>/third_party/llama/<平台>/lib`，平台为 `macos-arm64` / `ios-arm64` / `ios-arm64-simulator`
//! - `IME_LLM_LLAMA_LIBS`：要链接的静态库名，逗号分隔，默认 `ime_llama`（build_llama.sh 合并出的单个库）。
//!   直接使用 llama.cpp 自己的构建产物时可设为 `llama,ggml,ggml-cpu,ggml-base`
//! - `IME_LLM_NO_LINK`：设为 `1` 时不输出 llama.cpp 库的链接指令，只编译并打包垫片本身。
//!   用于 iOS：Rust 产出 staticlib，由 Xcode 在最终链接时加入 `ImeLlama.xcframework`、
//!   `libc++` 与 `Accelerate.framework`
//!
//! 重新构建的触发条件（`llama` 特性开启时）：垫片源码、上面这些环境变量、头文件目录里的任何文件
//! （`llama.h` 与 `ggml*.h` / `gguf.h`：垫片按值传递 llama.cpp 的参数结构体，头文件变了必须重新编译），
//! 以及每个被链接的静态库文件（静态库会被打包进本 crate 的 rlib，再进入 `libime_ffi.a`；
//! 不跟踪的话 scripts/build_llama.sh 重新构建之后 cargo 仍然使用旧的那份）。
//!
//! 为 iOS 交叉编译时还需设置 `IPHONEOS_DEPLOYMENT_TARGET`（不低于构建 llama.cpp 时的最低版本，默认 16.4），
//! 否则 rustc 按它自己的默认最低版本链接，会出现 `___chkstk_darwin` 等符号找不到的错误。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "llama")]
    llama::build();
}

#[cfg(feature = "llama")]
mod llama {
    use std::env;
    use std::path::PathBuf;

    fn env_path(name: &str) -> Option<PathBuf> {
        println!("cargo:rerun-if-env-changed={name}");
        env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    }

    fn env_flag(name: &str) -> bool {
        println!("cargo:rerun-if-env-changed={name}");
        matches!(env::var(name).as_deref(), Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes"))
    }

    /// scripts/build_llama.sh 输出目录中与当前目标对应的子目录名
    fn platform_dir() -> Option<&'static str> {
        let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let target = env::var("TARGET").unwrap_or_default();
        match (os.as_str(), arch.as_str()) {
            ("macos", "aarch64") => Some("macos-arm64"),
            ("ios", "aarch64") if target.ends_with("-sim") => Some("ios-arm64-simulator"),
            ("ios", "aarch64") => Some("ios-arm64"),
            _ => None,
        }
    }

    pub fn build() {
        println!("cargo:rerun-if-changed=shim/ime_llm_shim.cpp");
        println!("cargo:rerun-if-changed=shim/ime_llm_shim.h");

        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        let third_party = manifest_dir.join("../../third_party/llama");

        let include_dir = env_path("IME_LLM_LLAMA_INCLUDE_DIR").unwrap_or_else(|| third_party.join("include"));
        if !include_dir.join("llama.h").is_file() {
            panic!(
                "ime-llm: 在 {} 找不到 llama.h。请先运行 scripts/build_llama.sh，或设置 IME_LLM_LLAMA_INCLUDE_DIR",
                include_dir.display()
            );
        }
        // 头文件目录整体作为输入（cargo 会扫描目录里每个文件的修改时间）：llama.h 以及它包含的
        // ggml.h / ggml-cpu.h / ggml-backend.h / ggml-opt.h / gguf.h 任何一个变了，垫片都要重新编译
        println!("cargo:rerun-if-changed={}", include_dir.display());

        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let apple = matches!(target_os.as_str(), "macos" | "ios" | "tvos" | "watchos" | "visionos");

        let mut build = cc::Build::new();
        build
            .cpp(true)
            .std("c++17")
            .opt_level(2)
            .warnings(true)
            .include(&include_dir)
            .include(manifest_dir.join("shim"))
            .file("shim/ime_llm_shim.cpp");
        build.compile("ime_llm_shim");

        if apple {
            // 垫片用 vDSP / vForce 做整词表 log-softmax
            println!("cargo:rustc-link-lib=framework=Accelerate");
        }

        let no_link = env_flag("IME_LLM_NO_LINK");
        let lib_dir_env = env_path("IME_LLM_LLAMA_LIB_DIR");
        println!("cargo:rerun-if-env-changed=IME_LLM_LLAMA_LIBS");
        if no_link {
            return;
        }
        let lib_dir = lib_dir_env.or_else(|| platform_dir().map(|p| third_party.join(p).join("lib")));
        let Some(lib_dir) = lib_dir else {
            panic!("ime-llm: 当前目标没有默认的 llama.cpp 库目录，请设置 IME_LLM_LLAMA_LIB_DIR（或 IME_LLM_NO_LINK=1）");
        };
        if !lib_dir.is_dir() {
            panic!(
                "ime-llm: llama.cpp 库目录 {} 不存在。请先运行 scripts/build_llama.sh，或设置 IME_LLM_LLAMA_LIB_DIR",
                lib_dir.display()
            );
        }
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        let libs = env::var("IME_LLM_LLAMA_LIBS").unwrap_or_else(|_| "ime_llama".to_string());
        for lib in libs.split(',').map(str::trim).filter(|l| !l.is_empty()) {
            println!("cargo:rustc-link-lib=static={lib}");
            // 静态库被打包进 rlib：库文件换了就得重新运行本脚本并重新链接。
            // 只跟踪确实在 lib_dir 里的文件（不存在的路径会让 cargo 每次都重新构建）
            let lib_file = lib_dir.join(format!("lib{lib}.a"));
            if lib_file.is_file() {
                println!("cargo:rerun-if-changed={}", lib_file.display());
            }
        }
    }
}
