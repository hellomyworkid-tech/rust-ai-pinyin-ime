# Rust AI 拼音输入法

[English](#english) · 简体中文

一个用 Rust 编写的 Windows 拼音输入法：大词库整句解码、中英文混输、拼音原位改错，可选接入本地或云端大模型来帮助选词。

> [!NOTE]
> 这是一个个人实验性项目，目前处于早期阶段（v0.1），欢迎试用、反馈与贡献。

## ✨ 特性

- **整句解码**：基于词图（Lattice）+ Beam Search 的整句解码，融合雾凇拼音词库（约 57 万词条）与二元语法；
  单字按常用字频排序，长句按「整句 → 前缀词 → 单字」逐级切分备选。
- **中英文免切换混输**：`wozaiyongrustxieapi` → `我在用 Rust 写 API`，并自动规范英文大小写（GitHub、NVIDIA、API…）。
- **拼音原位编辑**：`←/→` 按音节跳动光标、点击音节定位，在句中直接改错；逐段选词时已选部分会锁定并以绿色标出。
- **模糊音与纠错**：平翘舌、前后鼻音、n/l、f/h 以及常见按键顺序错误；精确拼写的结果始终优先。
- **个人词频记忆**：记住你的选词习惯与常用搭配，数据只保存在本机 `%LOCALAPPDATA%\RustAiPinyinIME`。
- **可选的大模型选词**（默认关闭）：
  - 内置本地引擎：一键下载 llama.cpp + Qwen2.5 0.5B / 1.5B GGUF 模型，完全离线运行；
  - 也可以接入 Ollama、LM Studio、llama.cpp Server，或任何 OpenAI 兼容接口（ChatGPT / Claude / Gemini / 自定义）。
- **两种接入方式**：
  - **TSF 系统输入法**（推荐）：注册为 Windows 正式输入法，可通过 `Win + Space` 切换；
  - **全局键盘钩子模式**：无需管理员权限，即装即用。
- **简洁的界面**：Win11 风格的横排候选条与可拖动的悬浮状态栏，贴边后会自动收成小方块。

## 📦 安装（普通用户）

1. 在 [Releases](../../releases) 页面下载最新的 `RustAI-Pinyin-IME-vX.Y.Z-windows-x64.zip` 并解压；
2. 运行 `RustAI拼音输入法_安装程序.exe`，点击「一键安装」；
3. （可选）在状态栏「⚙ 设置 → 输入法模式」中注册为 TSF 系统输入法（需要管理员权限确认一次）。

常用快捷键：

| 按键 | 功能 |
| --- | --- |
| `Shift`（单击） / `Ctrl + Space` | 切换中文 / 英文 |
| `空格` / `1`–`5` | 上屏高亮候选 / 选择对应候选 |
| `↑` `↓` | 移动候选高亮（在首项继续按 `↑` 可直接选中上方「整句」预览，按空格整句上屏） |
| `←` `→` | 按音节移动拼音光标，原位改错 |
| `-` `=` / `PageUp` `PageDown` | 翻页 |
| `Enter` | 直接上屏原始字母 |
| `Esc` | 取消当前输入 |
| `，。？！；：` | 整句上屏并输入中文标点 |

> [!TIP]
> 若安装了 ESET 等安全软件并开启了「银行业务和浏览保护 → 保护所有浏览器」，安全沙箱会拦截未加盖商业 EV 代码签名的第三方 TSF 输入法 DLL 注入浏览器。日常浏览网页时建议关闭「保护所有浏览器」选项。

## 🛠️ 从源码构建（开发者）

环境要求：Windows 10/11、Rust stable（项目使用 `x86_64-pc-windows-gnu` 工具链开发）。

```powershell
# 1. 下载开源词库并编译二进制词库 data\compiled\merged_lexicon.bin
powershell -ExecutionPolicy Bypass -File scripts\sync_open_lexicons.ps1

# 2. 编译与测试
cargo build --release -p ime-tsf-win
cargo test --workspace

# 3.（可选）打包发布用的免安装压缩包
powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1 -Version 0.1.0
```

> [!TIP]
> 如果仓库路径包含中文，MinGW 链接器可能因为路径编码出错。可以参考 `.cargo/config.toml.example`，把构建目录设置到纯英文路径。

### 项目结构

| Crate | 说明 |
| --- | --- |
| `crates/ime-lexicon` | 多源词库融合、紧凑二进制格式（mmap 加载）、音节 / 简拼 / 英文 Trie；`lexicon-builder` 编译工具 |
| `crates/ime-core` | 拼音切分、模糊音纠错、词图构建与整句解码、个人词频记忆 |
| `crates/ime-ai` | 大模型重排（OpenAI 兼容客户端、本地 llama.cpp 托管、拼音约束解码） |
| `crates/ime-tsf-win` | Windows TSF COM 组件（`ime_tsf_win.dll`）、全局服务 / 候选窗 / 状态栏 / 设置窗口、安装程序、桌面体验台 |
| `crates/ime-playground` | 命令行试验工具 |

## 🔒 隐私

- 个人词频、配置与 API Key 只保存在本机 `%LOCALAPPDATA%\RustAiPinyinIME`，不会上传；
- 大模型选词默认关闭；开启云端接口后，候选所需的上下文会发送给你配置的服务商；
- 在 Win32 密码输入框中，输入法会自动放行，不拦截、也不记忆。

## 📚 致谢与第三方数据

- [雾凇拼音 rime-ice](https://github.com/iDvel/rime-ice)（GPL-3.0）：中文词库与英文词库，由 `scripts/sync_open_lexicons.ps1` 下载；
  `data/seed` 中的小型种子词表也参考了它的格式与词频；
- [万象拼音 rime-wanxiang](https://github.com/amzxyz/rime_wanxiang)：二元语法种子数据的思路参考；
- [Qwen2.5](https://github.com/QwenLM/Qwen2.5) 与 [llama.cpp](https://github.com/ggml-org/llama.cpp)：可选的本地大模型推理（运行时按需下载，不包含在本仓库中）。

## 📄 许可证

本项目以 [GNU General Public License v3.0 or later](LICENSE) 发布。

---

## English

**Rust AI Pinyin IME** is a Windows Chinese Pinyin input method written in Rust. It provides:

- whole-sentence decoding (lattice + beam search) over a ~570k-entry lexicon derived from [rime-ice](https://github.com/iDvel/rime-ice);
- seamless mixed Chinese/English input without switching modes;
- in-place pinyin editing with syllable-level cursor movement, plus fuzzy-pinyin and typo correction;
- on-device personal frequency memory;
- optional LLM-assisted candidate re-ranking (bundled local llama.cpp + Qwen2.5, or any OpenAI-compatible API);
- a native TSF text service and a no-admin global-hook mode.

Download a prebuilt package from [Releases](../../releases), or build from source with the steps above.
Licensed under GPL-3.0-or-later.
