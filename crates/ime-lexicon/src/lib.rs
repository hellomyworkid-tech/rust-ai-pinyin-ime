//! `ime-lexicon` (v3): 多源开源词库融合编译器、零拷贝二进制格式 (`AIMELX03`) 与 `mmap` 只读视图检索存储层
//!
//! 升级特性：
//! 1. 自定义 Little-Endian 分区式二进制格式 (`AIMELX03`)：定长记录 + 字符串池 + 并列数组，
//!    加载后 [`CompiledLexicon`] 只是映射字节上的一层视图，不为任何词条或 Trie 节点分配堆内存
//!    （只读文件映射页不计入 iOS 键盘扩展的内存上限），加载耗时为微秒级
//! 2. 支持直接融合百万级完整版 `rime-ice` (`8105` + `base` + `ext`)、`rime-frost`、万象 Bigram 与 `melt_eng`
//! 3. 智能简拼剪枝（仅对高频 2~5 字词建立首字母简拼索引），保持微秒级检索性能
//!
//! 结构分工：[`MultiSourceBuilder`] 在堆上融合词条并构建 [`FlatTrie`]（[`LexiconParts`]），
//! 再由 `format` 模块编码为 `AIMELX03` 字节；运行期只通过 [`CompiledLexicon`] 的访问器读取。

mod format;
mod legacy;

pub use format::{
    CommonPrefixSearch, CompiledLexicon, EntryRef, IdIter, IdList, Syllables, TrieChildren,
    TrieNodeRef, TrieView, LEGACY_MAGIC_V2, LEXICON_MAGIC,
};

use memmap2::MmapOptions;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LexiconError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid or outdated lexicon binary header magic")]
    InvalidMagic,
    #[error("Corrupted binary lexicon data at offset {0}")]
    CorruptedBinary(usize),
    #[error("Outdated lexicon binary format {0} (this build reads AIMELX03); migrate it with `lexicon-builder --migrate-v2 <file>` or rebuild it with lexicon-builder")]
    OutdatedFormat(String),
    #[error("Lexicon binary is truncated or has trailing data: header says {expected} bytes, found {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("Lexicon cannot be encoded: {0}")]
    Unencodable(&'static str),
    #[error("Lexicon entry #{index} cannot be encoded: {reason}")]
    UnencodableEntry { index: usize, reason: &'static str },
    #[error("Lexicon file {} cannot be replaced, most likely because a running IME process (service, workbench or playground) still has it memory-mapped; close it and retry: {source}", .path.display())]
    TargetInUse {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 词条所属的垂直领域标签，用于配合 L3 应用焦点场景感知动态调权
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum DomainTag {
    General = 0,
    Tech = 1,
    Academic = 2,
    Chat = 3,
}

impl DomainTag {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Tech,
            2 => Self::Academic,
            3 => Self::Chat,
            _ => Self::General,
        }
    }

    pub fn from_str_tag(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "tech" | "it" | "code" => Self::Tech,
            "academic" | "math" | "science" => Self::Academic,
            "chat" | "daily" => Self::Chat,
            _ => Self::General,
        }
    }
}

/// 编译并归一化后的统一词条结构（构建期的堆上表示；运行期请通过 [`EntryRef`] 访问映射中的词条）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexiconEntry {
    /// 最终上屏文本（如 `"不在意了"`, `"傅里叶级数"`, `"TypeScript"`, `"GitHub"`）
    pub text: String,
    /// 标准无调拼音音节列表（中文词条如 `["bu", "zai", "yi", "le"]`，纯英文词条为空）
    pub pinyin_syllables: Vec<String>,
    /// 音节分隔检索键（如 `"bu'zai'yi'le"`，英文词为小写 `"typescript"`）
    pub syllable_key: String,
    /// 连续无分隔检索键（如 `"buzaiyile"`）
    pub flat_key: String,
    /// 首字母简拼检索键（如 `"bzyl"`，仅对多字中文词有效）
    pub abbrev_key: String,
    /// 归一化后的负对数概率代价 $-\ln P(w)$，数值越小代表词频越高、优先级越高
    pub cost: f32,
    /// 原始融合频次权重
    pub raw_weight: f64,
    /// 是否为英文/混输词条
    pub is_english: bool,
    /// 垂直领域分类
    pub domain: DomainTag,
    /// 可选的万象声调信息（1=阴平, 2=阳平, 3=上声, 4=去声, 5=轻声）
    pub tones: Option<Vec<u8>>,
}

/// 扁平化高速前缀树节点（连续数组存储，CPU 缓存友好）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlatTrieNode {
    /// 子节点转移列表：(byte, child_node_index) —— 保持按 byte 有序以便二分查找
    pub transitions: Vec<(u8, u32)>,
    /// 精确命中当前节点的 `LexiconEntry` 下标列表（已按 `cost` 升序排列）
    pub exact_entries: Vec<u32>,
    /// 以当前节点为前缀的 Top-8 高优 `LexiconEntry` 下标缓存（用于毫秒级前缀自动补全）
    pub top_prefix_entries: Vec<u32>,
}

impl FlatTrieNode {
    fn new() -> Self {
        Self {
            transitions: Vec::new(),
            exact_entries: Vec::new(),
            top_prefix_entries: Vec::new(),
        }
    }

    #[inline]
    pub fn find_child(&self, b: u8) -> Option<u32> {
        self.transitions
            .binary_search_by_key(&b, |&(ch, _)| ch)
            .ok()
            .map(|idx| self.transitions[idx].1)
    }
}

/// 高性能扁平化前缀树索引（构建期的堆上表示；运行期请通过 [`TrieView`] 访问映射中的前缀树）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlatTrie {
    pub nodes: Vec<FlatTrieNode>,
}

impl Default for FlatTrie {
    fn default() -> Self {
        Self::new()
    }
}

impl FlatTrie {
    pub fn new() -> Self {
        Self {
            nodes: vec![FlatTrieNode::new()],
        }
    }

    /// 插入一个键与对应的词条 ID（假设插入顺序已按 cost 升序排列）
    pub fn insert(&mut self, key: &str, entry_id: u32, record_prefix_top: bool, max_prefix_top: usize) {
        self.insert_with_limit(key, entry_id, record_prefix_top, max_prefix_top, 255);
    }

    /// 插入一个键与对应的词条 ID，并指定当前节点精确命中列表的最大容量（上限 255，与二进制格式 `u8` 对齐）
    pub fn insert_with_limit(
        &mut self,
        key: &str,
        entry_id: u32,
        record_prefix_top: bool,
        max_prefix_top: usize,
        max_exact: usize,
    ) {
        let mut curr = 0u32;
        for &b in key.as_bytes() {
            let next = match self.nodes[curr as usize].find_child(b) {
                Some(child_idx) => child_idx,
                None => {
                    let new_idx = self.nodes.len() as u32;
                    self.nodes.push(FlatTrieNode::new());
                    let trans = &mut self.nodes[curr as usize].transitions;
                    let pos = trans.binary_search_by_key(&b, |&(ch, _)| ch).unwrap_err();
                    trans.insert(pos, (b, new_idx));
                    new_idx
                }
            };
            curr = next;
            if record_prefix_top {
                let top = &mut self.nodes[curr as usize].top_prefix_entries;
                if top.len() < max_prefix_top && !top.contains(&entry_id) {
                    top.push(entry_id);
                }
            }
        }
        let exact = &mut self.nodes[curr as usize].exact_entries;
        let cap = max_exact.min(255);
        if exact.len() < cap && !exact.contains(&entry_id) {
            exact.push(entry_id);
        }
    }

    /// 从指定节点 `start_node` 沿 `segment` 字节序列前进一步，若路径不存在则立即返回 `None`（用于晶格图 DFS 零拷贝剪枝）
    #[inline]
    pub fn advance_node(&self, start_node: u32, segment: &str) -> Option<u32> {
        let mut curr = start_node;
        for &b in segment.as_bytes() {
            curr = self.nodes[curr as usize].find_child(b)?;
        }
        Some(curr)
    }

    /// 精确查找键对应的词条 ID 切片
    #[inline]
    pub fn exact_lookup(&self, key: &str) -> &[u32] {
        let mut curr = 0u32;
        for &b in key.as_bytes() {
            match self.nodes[curr as usize].find_child(b) {
                Some(next) => curr = next,
                None => return &[],
            }
        }
        &self.nodes[curr as usize].exact_entries
    }

    /// 前缀联想查找（用于英文自动补全如输入 `typesc` -> `TypeScript`）
    #[inline]
    pub fn prefix_lookup(&self, prefix: &str) -> &[u32] {
        let mut curr = 0u32;
        for &b in prefix.as_bytes() {
            match self.nodes[curr as usize].find_child(b) {
                Some(next) => curr = next,
                None => return &[],
            }
        }
        &self.nodes[curr as usize].top_prefix_entries
    }

    /// 单趟扫描返回 `input` 所有命中的前缀长度及词条下标列表：$O(|input|)$
    #[inline]
    pub fn common_prefix_search<'a>(&'a self, input: &str) -> Vec<(usize, &'a [u32])> {
        let mut results = Vec::new();
        let mut curr = 0u32;
        for (idx, &b) in input.as_bytes().iter().enumerate() {
            match self.nodes[curr as usize].find_child(b) {
                Some(next) => {
                    curr = next;
                    let entries = &self.nodes[curr as usize].exact_entries;
                    if !entries.is_empty() {
                        results.push((idx + 1, entries.as_slice()));
                    }
                }
                None => break,
            }
        }
        results
    }
}

/// 构建期的词库堆上表示：全部词条、三棵前缀树与二元语法表。
///
/// 由 [`MultiSourceBuilder::build_parts`] 产出，经 [`CompiledLexicon::from_parts`] 编码为 `AIMELX03`；
/// 运行期不再持有该结构（全量词库下它要占用四百多 MB 堆内存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexiconParts {
    /// 全部词条池（已按 cost 升序排列；[`MultiSourceBuilder::build_parts`] 同分时先放不含 ASCII 大写字母的，再按 text、syllable_key 排）
    pub entries: Vec<LexiconEntry>,
    /// 精确音节序列索引（键格式：`"bu'zai'yi'le"` 或 `"xi'an"`）
    pub syllable_trie: FlatTrie,
    /// 首字母简拼索引（键格式：`"bzyl"` -> `"不在意了"`, `"jsj"` -> `"计算机"`）
    pub abbrev_trie: FlatTrie,
    /// 英文与混输大小写规范前缀树（键格式：小写 `"typescript"` -> 词条 `"TypeScript"`）
    pub english_trie: FlatTrie,
    /// 二元语法转移加成表（键：`prev_word \t next_word`，值：正数 bonus 代表减少的 cost）
    pub bigrams: HashMap<String, f32>,
}

impl LexiconParts {
    /// 读取旧版 `AIMELX02` 镜像并还原为堆上结构，仅用于一次性迁移（`lexicon-builder --migrate-v2`）。
    /// 迁移会原样保留词条顺序与前缀树内容，因此解码结果与旧镜像逐字节一致。
    pub fn load_legacy_v2(path: impl AsRef<Path>) -> Result<Self, LexiconError> {
        legacy::read_v2(&std::fs::read(path)?)
    }
}

/// 临时文件名序号（同一进程内并发写盘时避免互相覆盖）
static SAVE_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl CompiledLexicon {
    /// 把 `AIMELX03` 字节镜像写入磁盘。
    ///
    /// 先写同目录下的临时文件（落盘后）再原子改名：目标文件可能正被其他进程映射着，
    /// 就地截断重写会让对方读到半截文件（Unix 下甚至触发 SIGBUS）。
    ///
    /// Windows 上被映射着的文件不允许被替换：只要还有进程加载着目标词库，改名就会失败，
    /// 此时返回 [`LexiconError::TargetInUse`]，需要先退出占用它的输入法进程再重试。
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<(), LexiconError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_name = path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        tmp_name.push(format!(
            ".tmp-{}-{}",
            std::process::id(),
            SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let tmp_path = path.with_file_name(tmp_name);
        // 改名前先 fsync：否则断电后可能留下一份改了名却没写完的目标文件
        let written = File::create(&tmp_path).and_then(|mut f| {
            f.write_all(self.as_bytes())?;
            f.sync_all()
        });
        if let Err(err) = written {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err.into());
        }
        if let Err(err) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(replace_error(err, path, cfg!(windows) && path.exists()));
        }
        Ok(())
    }

    /// 以只读 `mmap` 方式打开 `AIMELX03` 二进制镜像：只校验文件头与分区表，不读取正文、不分配堆内存。
    /// 旧版 `AIMELX02` 文件返回 [`LexiconError::OutdatedFormat`]，被截断的文件返回 [`LexiconError::SizeMismatch`]。
    ///
    /// 映射会一直保持到返回值及其全部 `clone()` 都被释放为止（通常就是整个进程的生命周期），因此：
    /// - 词库文件只允许“写新文件再改名覆盖”（[`CompiledLexicon::save_to_file`] 即如此），绝不能就地改写或截断：
    ///   映射期间文件内容被改动时，读取方在 Unix 下可能收到 SIGBUS，或读到中途变化的字节；
    ///   iOS 上宿主 App 更新 App Group 共享目录里的词库时同样必须遵守；
    /// - Windows 上被映射的文件无法被覆盖、截断或删除（可以先改名挪开），
    ///   安装器更新词库前要先结束加载着它的进程，或把旧文件改名挪开后再写入新文件。
    ///
    /// 加载不检查正文：词库文件刚安装/更新之后应调用一次 [`CompiledLexicon::verify`]，校验失败就不要使用该文件。
    pub fn load_mmap(path: impl AsRef<Path>) -> Result<Self, LexiconError> {
        let file = File::open(path)?;
        if file.metadata()?.len() == 0 {
            return Err(LexiconError::InvalidMagic);
        }
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        Self::from_mmap(mmap)
    }

    /// 直接从内置/种子三源词库目录编译构建（如果磁盘已有 `AIMELX03` 编译镜像则优先 mmap 加载；
    /// 旧版 `AIMELX02` 镜像或损坏的文件一律视同不存在，回退到种子词库现场编译，并向 stderr 说明原因）
    pub fn load_or_build_default(workspace_root: impl AsRef<Path>) -> Result<Self, LexiconError> {
        let root = workspace_root.as_ref();
        let mut candidate_roots = vec![root.to_path_buf()];
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                candidate_roots.push(exe_dir.to_path_buf());
                if let Some(parent_dir) = exe_dir.parent() {
                    candidate_roots.push(parent_dir.to_path_buf());
                }
            }
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidate_roots.push(cwd);
        }
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            candidate_roots.push(PathBuf::from(local_app_data).join("RustAiPinyinIME"));
        }
        Self::load_or_build_from(&candidate_roots, root)
    }

    /// [`CompiledLexicon::load_or_build_default`] 的实际逻辑：只在给定的 `candidate_roots` 中依次查找编译镜像与种子词库，
    /// 找不到可用镜像时把现场编译的结果回写到 `write_root`（查找范围由调用方显式给出，便于测试不受本机环境影响）。
    fn load_or_build_from(candidate_roots: &[PathBuf], write_root: &Path) -> Result<Self, LexiconError> {
        let compiled_path = write_root.join("data/compiled/merged_lexicon.bin");
        // 回写位置上若躺着一份旧版格式的镜像，回退编译后不去覆盖它：
        // 那多半是尚未迁移的全量词库，用种子词库顶掉它会让用户丢失数据（可用 `lexicon-builder --migrate-v2` 迁移）
        let mut keep_outdated_image = false;
        let mut rejected_images = 0usize;
        for cand_root in candidate_roots {
            for rel in ["data/compiled/merged_lexicon.bin", "merged_lexicon.bin"] {
                let bin_path = cand_root.join(rel);
                if bin_path.exists() {
                    match Self::load_mmap(&bin_path) {
                        Ok(lex) => return Ok(lex),
                        Err(err) => {
                            // 静默回退会让 57 万词条的词库悄悄变成几百条的种子词库，这里至少留下一行原因
                            rejected_images += 1;
                            log_stderr(format_args!(
                                "[ime-lexicon] 跳过无法加载的词库镜像 {}: {err}",
                                bin_path.display()
                            ));
                            if matches!(err, LexiconError::OutdatedFormat(_)) && bin_path == compiled_path {
                                keep_outdated_image = true;
                            }
                        }
                    }
                }
            }
        }

        let mut builder = MultiSourceBuilder::new();
        let seed_dir = candidate_roots
            .iter()
            .map(|r| r.join("data/seed"))
            .find(|p| p.exists())
            .unwrap_or_else(|| write_root.join("data/seed"));
        let base_path = seed_dir.join("rime_ice.base.dict.yaml");
        let eng_path = seed_dir.join("melt_eng.dict.yaml");
        let bigram_path = seed_dir.join("wanxiang.bigram.tsv");

        if base_path.exists() {
            builder.load_rime_dict_file(&base_path, 1.0, DomainTag::General)?;
        }
        if eng_path.exists() {
            builder.load_english_dict_file(&eng_path, 1.0)?;
        }
        if bigram_path.exists() {
            builder.load_bigram_file(&bigram_path)?;
        }
        let compiled = builder.build();
        if rejected_images > 0 {
            log_stderr(format_args!(
                "[ime-lexicon] 没有可用的编译词库，已回退为种子词库现场编译（仅 {} 个词条）",
                compiled.entry_count()
            ));
        }
        if !keep_outdated_image {
            let _ = compiled.save_to_file(&compiled_path);
        }
        Ok(compiled)
    }
}

/// 向 stderr 写一行诊断信息；写失败（例如 stderr 已关闭）时直接忽略，绝不因此 panic
fn log_stderr(line: std::fmt::Arguments<'_>) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// 把“改名覆盖目标词库失败”的 IO 错误转换成更易懂的错误。
/// `target_is_mapped_candidate` 为真（Windows 且目标文件已存在）并且错误属于“拒绝访问 / 共享冲突 / 文件被映射”时，
/// 几乎可以肯定是目标词库正被某个输入法进程映射着，返回 [`LexiconError::TargetInUse`]。
fn replace_error(err: std::io::Error, path: &Path, target_is_mapped_candidate: bool) -> LexiconError {
    // ERROR_ACCESS_DENIED = 5, ERROR_SHARING_VIOLATION = 32, ERROR_USER_MAPPED_FILE = 1224
    let looks_in_use = err.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(err.raw_os_error(), Some(5 | 32 | 1224));
    if target_is_mapped_candidate && looks_in_use {
        LexiconError::TargetInUse {
            path: path.to_path_buf(),
            source: err,
        }
    } else {
        LexiconError::Io(err)
    }
}

/// 词条文本与检索键的字节长度上限（`AIMELX03` 词条记录中以 `u16` 保存长度）
const MAX_FIELD_BYTES: usize = u16::MAX as usize;

/// 多源开源词库（雾凇 + 白霜 + 万象 + melt_eng + THUOCL）融合构建器
pub struct MultiSourceBuilder {
    /// 聚合键 `(text, syllable_key)` -> 词条
    merged_entries: HashMap<(String, String), LexiconEntry>,
    bigrams: HashMap<String, f32>,
}

impl Default for MultiSourceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiSourceBuilder {
    pub fn new() -> Self {
        Self {
            merged_entries: HashMap::new(),
            bigrams: HashMap::new(),
        }
    }

    /// 解析带声调或无声调的拼音串，例如 `"zhong1 guo2"` -> `(["zhong", "guo"], Some([1, 2]))`
    pub fn normalize_pinyin_code(raw_code: &str) -> (Vec<String>, Option<Vec<u8>>) {
        let mut syllables = Vec::new();
        let mut tones = Vec::new();
        let mut has_tone = false;

        for token in raw_code.split(|c: char| c.is_whitespace() || c == '\'') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let last_char = token.chars().last().unwrap();
            if ('1'..='5').contains(&last_char) && token.len() > 1 {
                has_tone = true;
                tones.push((last_char as u8) - b'0');
                let clean = token[..token.len() - 1]
                    .to_ascii_lowercase()
                    .replace('ü', "v")
                    .replace("u:", "v");
                syllables.push(clean);
            } else {
                tones.push(5);
                let clean = token
                    .to_ascii_lowercase()
                    .replace('ü', "v")
                    .replace("u:", "v");
                syllables.push(clean);
            }
        }
        (syllables, if has_tone { Some(tones) } else { None })
    }

    /// 从 Rime `.dict.yaml` 格式字符串导入中文词库（兼容雾凇、白霜、万象格式）
    pub fn load_rime_dict_str(&mut self, content: &str, source_scale: f64, default_domain: DomainTag) {
        let mut in_header = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed == "---" {
                in_header = true;
                continue;
            }
            if trimmed == "..." {
                in_header = false;
                continue;
            }
            if in_header {
                continue;
            }

            let parts: Vec<&str> = trimmed.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let text = parts[0].trim();
            let raw_code = parts[1].trim();
            let weight: f64 = parts
                .get(2)
                .and_then(|w| w.trim().trim_end_matches('%').parse::<f64>().ok())
                .unwrap_or(100.0)
                * source_scale;
            let domain = parts
                .get(3)
                .map(|d| DomainTag::from_str_tag(d))
                .unwrap_or(default_domain);

            let (syllables, tones) = Self::normalize_pinyin_code(raw_code);
            if syllables.is_empty() || syllables.len() > 12 {
                continue;
            }

            let syllable_key = syllables.join("'");
            // 二进制格式用 u16 记录文本与检索键的字节长度：超长的（必然是坏数据的）行直接跳过，保证 `build()` 一定能编码
            if text.len() > MAX_FIELD_BYTES || syllable_key.len() > MAX_FIELD_BYTES {
                continue;
            }
            let flat_key = syllables.join("");
            let abbrev_key: String = syllables
                .iter()
                .filter_map(|s| s.chars().next())
                .collect();

            let inferred_domain = if domain != DomainTag::General {
                domain
            } else if is_tech_keyword(text) {
                DomainTag::Tech
            } else if is_academic_keyword(text) {
                DomainTag::Academic
            } else {
                DomainTag::General
            };

            let key = (text.to_string(), syllable_key.clone());
            self.merged_entries
                .entry(key)
                .and_modify(|existing| {
                    existing.raw_weight += weight;
                    if existing.tones.is_none() && tones.is_some() {
                        existing.tones = tones.clone();
                    }
                })
                .or_insert_with(|| LexiconEntry {
                    text: text.to_string(),
                    pinyin_syllables: syllables,
                    syllable_key,
                    flat_key,
                    abbrev_key,
                    cost: 0.0,
                    raw_weight: weight.max(1.0),
                    is_english: false,
                    domain: inferred_domain,
                    tones,
                });
        }
    }

    pub fn load_rime_dict_file(
        &mut self,
        path: impl AsRef<Path>,
        source_scale: f64,
        default_domain: DomainTag,
    ) -> Result<(), LexiconError> {
        let content = std::fs::read_to_string(path)?;
        self.load_rime_dict_str(&content, source_scale, default_domain);
        Ok(())
    }

    /// 导入 `melt_eng` / 程序员大小写规范英文词表
    pub fn load_english_dict_str(&mut self, content: &str, source_scale: f64) {
        let mut in_header = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed == "---" {
                in_header = true;
                continue;
            }
            if trimmed == "..." {
                in_header = false;
                continue;
            }
            if in_header {
                continue;
            }

            let parts: Vec<&str> = trimmed.split('\t').collect();
            if parts.is_empty() {
                continue;
            }
            let canonical_text = parts[0].trim();
            if canonical_text.is_empty() || canonical_text.len() > 32 {
                continue;
            }
            let lower_key = parts
                .get(1)
                .map(|s| s.trim().to_ascii_lowercase())
                .unwrap_or_else(|| canonical_text.to_ascii_lowercase());
            if !lower_key.bytes().all(|b| b.is_ascii_alphanumeric()) || lower_key.len() > MAX_FIELD_BYTES {
                continue;
            }
            let weight: f64 = parts
                .get(2)
                .and_then(|w| w.trim().parse::<f64>().ok())
                .unwrap_or(2_500.0)
                * source_scale;
            let domain = parts
                .get(3)
                .map(|d| DomainTag::from_str_tag(d))
                .unwrap_or(DomainTag::Tech);

            let key = (canonical_text.to_string(), lower_key.clone());
            self.merged_entries
                .entry(key)
                .and_modify(|e| e.raw_weight = e.raw_weight.max(weight) + weight * 0.25)
                .or_insert_with(|| LexiconEntry {
                    text: canonical_text.to_string(),
                    pinyin_syllables: Vec::new(),
                    syllable_key: lower_key.clone(),
                    flat_key: lower_key,
                    abbrev_key: String::new(),
                    cost: 0.0,
                    raw_weight: weight.max(1.0),
                    is_english: true,
                    domain,
                    tones: None,
                });
        }
    }

    pub fn load_english_dict_file(
        &mut self,
        path: impl AsRef<Path>,
        source_scale: f64,
    ) -> Result<(), LexiconError> {
        let content = std::fs::read_to_string(path)?;
        self.load_english_dict_str(&content, source_scale);
        Ok(())
    }

    /// 导入万象 / 双语 Bigram 转移概率文件
    pub fn load_bigram_str(&mut self, content: &str) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = trimmed.split('\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let prev = parts[0].trim();
            let next = parts[1].trim();
            if let Ok(bonus) = parts[2].trim().parse::<f32>() {
                self.bigrams.insert(format!("{prev}\t{next}"), bonus);
            }
        }
    }

    pub fn load_bigram_file(&mut self, path: impl AsRef<Path>) -> Result<(), LexiconError> {
        let content = std::fs::read_to_string(path)?;
        self.load_bigram_str(&content);
        Ok(())
    }

    /// 完成词频对数平滑归一化并构建全部前缀树索引，直接得到 `AIMELX03` 视图（自有内存，不落盘）
    pub fn build(self) -> CompiledLexicon {
        let parts = self.build_parts();
        // 不会失败：两个 `load_*_dict_str` 已跳过超长的文本/检索键，音节数不超过 12，
        // 派生键与前缀树都由本构建器自己生成，必然自洽
        CompiledLexicon::from_parts(&parts)
            .expect("MultiSourceBuilder 产出的词条与前缀树必然可以编码为 AIMELX03")
    }

    /// 完成词频对数平滑归一化并构建全部前缀树索引，返回构建期的堆上结构。
    ///
    /// 结果与 `HashMap` 的遍历顺序无关：同样的词表按同样的顺序加载，每次都编译出逐字节相同的词库
    /// （发布的词库要能由源码包重新编译、按 SHA-256 核对）。
    pub fn build_parts(self) -> LexiconParts {
        let mut entries: Vec<LexiconEntry> = self.merged_entries.into_values().collect();
        // `HashMap` 每次运行的遍历顺序都不同，先按聚合键 `(text, syllable_key)` 排好：
        // 下面的权重求和（浮点加法不满足结合律）从此与它无关
        entries.sort_unstable_by(|a, b| {
            a.text
                .cmp(&b.text)
                .then_with(|| a.syllable_key.cmp(&b.syllable_key))
        });
        let total_weight: f64 = entries.iter().map(|e| e.raw_weight).sum::<f64>().max(1.0);
        let vocab_size = entries.len() as f64;

        for entry in &mut entries {
            let prob = (entry.raw_weight + 1.0) / (total_weight + vocab_size);
            let mut base_cost = (-prob.ln()) as f32;

            if !entry.is_english && entry.pinyin_syllables.len() >= 2 {
                let len_bonus = (entry.pinyin_syllables.len() as f32 - 1.0) * 1.15;
                base_cost = (base_cost - len_bonus).max(0.5);
            }
            entry.cost = base_cost;
        }

        // 按 cost 升序；同分时不含 ASCII 大写字母的在前（见 [`has_ascii_uppercase`]），
        // 再按 text、syllable_key（二者合起来就是聚合键，所以这是全序）。
        // 同分的词条很多（光 rime_ice_base 里权重为 1 的就有约 6 万条），只按 cost 排时它们的先后、
        // 词条编号，以及下面各前缀树的每键上限截掉哪些词条，都会随遍历顺序变化
        entries.sort_unstable_by(|a, b| {
            a.cost
                .total_cmp(&b.cost)
                .then_with(|| has_ascii_uppercase(&a.text).cmp(&has_ascii_uppercase(&b.text)))
                .then_with(|| a.text.cmp(&b.text))
                .then_with(|| a.syllable_key.cmp(&b.syllable_key))
        });

        let mut syllable_trie = FlatTrie::new();
        let mut abbrev_trie = FlatTrie::new();
        let mut english_trie = FlatTrie::new();

        for (idx, entry) in entries.iter().enumerate() {
            let id = idx as u32;
            if entry.is_english {
                english_trie.insert_with_limit(&entry.flat_key, id, true, 6, 24);
            } else {
                // 单音节汉字保留全部同音字（8105 字表中单音节最多 141 字 <= 255，彻底杜绝如 `ji` 漏掉「吉」等常用字）
                let syl_cap = if entry.pinyin_syllables.len() == 1 { 255 } else { 64 };
                syllable_trie.insert_with_limit(&entry.syllable_key, id, false, 0, syl_cap);
                if entry.pinyin_syllables.len() == 1
                    && entry.text.chars().count() == 1
                    && !entry.abbrev_key.is_empty()
                {
                    // 单字首字母简拼索引（如 `w` -> 我/为/无/问/外/王，`z` -> 在/这/中/正，`j` -> 就/家/吉/佳）
                    abbrev_trie.insert_with_limit(&entry.abbrev_key, id, false, 0, 120);
                } else if (2..=6).contains(&entry.pinyin_syllables.len())
                    && !entry.abbrev_key.is_empty()
                    && entry.raw_weight >= 100.0
                {
                    // 对 2~6 字常用词建立首字母简拼索引（如 `wz` -> 我在/文章/位置/完整，`wzj` -> 为自己/我自己/我这就）
                    abbrev_trie.insert_with_limit(&entry.abbrev_key, id, false, 0, 48);
                }
            }
        }

        LexiconParts {
            entries,
            syllable_trie,
            abbrev_trie,
            english_trie,
            bigrams: self.bigrams,
        }
    }
}

/// 同分排序用：文本里有没有 ASCII 大写字母。
///
/// 英文词表（melt_eng_full）没有词频，两万多个词全是同一个默认权重；直接按字节序比较时大写字母排在小写前面，
/// `Polish` 会排在 `polish` 前面，前缀补全也偏向人名、地名（`water` 补出 Waterford、Waterloo）。
/// 同分时先放不含大写字母的词条，再按文本排
fn has_ascii_uppercase(text: &str) -> bool {
    text.bytes().any(|b| b.is_ascii_uppercase())
}

fn is_tech_keyword(text: &str) -> bool {
    const TECH_WORDS: &[&str] = &[
        "技术", "代码", "编程", "接口", "数据", "模型", "算法", "架构", "性能", "优化",
        "测试", "部署", "修复", "调用", "返回", "组件", "模块", "函数", "变量", "类型",
        "结构体", "编译器", "输入法", "词库", "计算机", "动态规划", "前缀树", "双数组",
        "有向无环图", "束搜索", "内存安全", "零拷贝", "低延迟", "开源", "服务器", "客户端",
        "终端", "编辑器", "大模型", "人工智能",
    ];
    TECH_WORDS.contains(&text)
}

fn is_academic_keyword(text: &str) -> bool {
    const ACADEMIC_WORDS: &[&str] = &[
        "级数", "奇数", "偶数", "微积分", "傅里叶", "傅里叶级数", "泰勒级数", "无穷级数",
        "收敛", "方程", "矩阵", "向量", "概率", "基数", "计数",
    ];
    ACADEMIC_WORDS.contains(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_pinyin_with_tones() {
        let (syl, tones) = MultiSourceBuilder::normalize_pinyin_code("zhong1 guo2");
        assert_eq!(syl, vec!["zhong", "guo"]);
        assert_eq!(tones, Some(vec![1, 2]));

        let (syl2, tones2) = MultiSourceBuilder::normalize_pinyin_code("nv3 er2");
        assert_eq!(syl2, vec!["nv", "er"]);
        assert_eq!(tones2, Some(vec![3, 2]));
    }

    #[test]
    fn test_multi_source_build_and_binary_roundtrip() {
        let mut builder = MultiSourceBuilder::new();
        builder.load_rime_dict_str(
            "---\nname: test\n...\n技术\tji shu\t180000\n级数\tji shu\t60000\n计算机\tji suan ji\t120000\n",
            1.0,
            DomainTag::General,
        );
        builder.load_english_dict_str("TypeScript\ttypescript\t150000\ttech\n", 1.0);
        builder.load_bigram_str("傅里叶\t级数\t5.5\n");

        let lex = builder.build();
        let hits = lex.syllable_trie().exact_lookup("ji'shu");
        assert_eq!(hits.len(), 2);
        assert_eq!(lex.entry(hits.get(0).unwrap()).text(), "技术");
        assert_eq!(lex.entry(hits.get(1).unwrap()).text(), "级数");

        let abbrev_hits = lex.abbrev_trie().exact_lookup("jsj");
        assert_eq!(abbrev_hits.len(), 1);
        assert_eq!(lex.entry(abbrev_hits.first().unwrap()).text(), "计算机");

        let eng_prefix = lex.english_trie().prefix_lookup("typesc");
        assert_eq!(eng_prefix.len(), 1);
        assert_eq!(lex.entry(eng_prefix.first().unwrap()).text(), "TypeScript");

        assert!((lex.bigram_bonus("傅里叶", "级数") - 5.5).abs() < 1e-5);

        // 经由字节镜像再走一遍，结果不变
        let reloaded = CompiledLexicon::from_bytes(lex.as_bytes().to_vec()).unwrap();
        assert_eq!(reloaded.entry_count(), 4);
        assert_eq!(reloaded.syllable_trie().exact_lookup("ji'shu").to_vec(), hits.to_vec());
        assert!((reloaded.bigram_bonus("傅里叶", "级数") - 5.5).abs() < 1e-5);
    }

    fn texts_of(lex: &CompiledLexicon, ids: IdList<'_>) -> Vec<String> {
        ids.iter().map(|id| lex.entry(id).text().to_string()).collect()
    }

    /// 按同分规则（先放不含 ASCII 大写字母的，再按字节序）排好后取前 `n` 个
    fn first_by_tie_rule(mut texts: Vec<String>, n: usize) -> Vec<String> {
        texts.sort_by(|a, b| has_ascii_uppercase(a).cmp(&has_ascii_uppercase(b)).then_with(|| a.cmp(b)));
        texts.truncate(n);
        texts
    }

    /// 每个 `HashMap` 的哈希种子都不同，同一进程里编译两次，遍历词条的顺序也不同。
    /// 大量同分词条时编译结果仍要逐字节相同；同分规则（先放不含 ASCII 大写字母的，再按文本）
    /// 决定先后，也决定每键上限截掉哪些词条
    #[test]
    fn test_build_is_byte_identical_with_cost_ties() {
        let han = |i: u32| char::from_u32(0x4E00 + i).unwrap();
        let mut rime = String::from("---\nname: ties\n...\n");
        // 130 个同音、同权重的单字：音节树 `zi` 全部保留（上限 255），单字简拼 `z` 只留 120 个
        for i in 0..130 {
            rime.push_str(&format!("{}\tzi\t1\n", han(i)));
        }
        // 100 个同音、同权重的双音节词，外加 10 个带大写拉丁字母的（A子…J子）：音节树 `zi'zi` 只留 64 个。
        // 只按字节序的话 A子…J子 会排在最前面；按同分规则它们排在最后，全被截掉
        for i in 0..100 {
            rime.push_str(&format!("{}子\tzi zi\t1\n", han(i)));
        }
        for c in 'A'..='J' {
            rime.push_str(&format!("{c}子\tzi zi\t1\n"));
        }
        // 60 个同音、同权重的常用双音节词：简拼 `dm` 只留 48 个
        for i in 0..60 {
            rime.push_str(&format!("{}马\tda ma\t500\n", han(i)));
        }
        // 15 个大写开头、15 个全小写的英文词，检索键都是 foo、权重相同：英文树精确命中只留 24 个
        // （15 个小写的在前，再 9 个大写开头的），前缀 Top 缓存只留 6 个（全是小写的）
        let mut eng = String::new();
        for i in 0..15 {
            eng.push_str(&format!("Foo{i}\tfoo\t2500\nfoo{i}\tfoo\t2500\n"));
        }

        let build = |english_first: bool| {
            let mut builder = MultiSourceBuilder::new();
            if english_first {
                builder.load_english_dict_str(&eng, 1.0);
            }
            builder.load_rime_dict_str(&rime, 1.0, DomainTag::General);
            if !english_first {
                builder.load_english_dict_str(&eng, 1.0);
            }
            builder.build()
        };
        let lex = build(false);
        for round in 0..4 {
            // 两份词表没有相同的词条，谁先加载也不影响结果
            assert_eq!(build(round % 2 == 0).as_bytes(), lex.as_bytes(), "第 {round} 次重新编译");
        }

        let singles: Vec<String> = (0..130).map(|i| han(i).to_string()).collect();
        let zizi: Vec<String> = (0..100)
            .map(|i| format!("{}子", han(i)))
            .chain(('A'..='J').map(|c| format!("{c}子")))
            .collect();
        let dama: Vec<String> = (0..60).map(|i| format!("{}马", han(i))).collect();
        let foos: Vec<String> = (0..15).flat_map(|i| [format!("Foo{i}"), format!("foo{i}")]).collect();
        let syllable = lex.syllable_trie();
        let abbrev = lex.abbrev_trie();
        let english = lex.english_trie();
        assert_eq!(texts_of(&lex, syllable.exact_lookup("zi")), first_by_tie_rule(singles.clone(), 255));
        assert_eq!(texts_of(&lex, abbrev.exact_lookup("z")), first_by_tie_rule(singles, 120));
        let kept_zizi = texts_of(&lex, syllable.exact_lookup("zi'zi"));
        assert_eq!(kept_zizi, first_by_tie_rule(zizi, 64));
        assert!(kept_zizi.iter().all(|t| !has_ascii_uppercase(t)), "带大写字母的同分词条应在上限处被截掉");
        assert_eq!(texts_of(&lex, syllable.exact_lookup("da'ma")), first_by_tie_rule(dama.clone(), 64));
        assert_eq!(texts_of(&lex, abbrev.exact_lookup("dm")), first_by_tie_rule(dama, 48));
        let kept_foo = texts_of(&lex, english.exact_lookup("foo"));
        assert_eq!(kept_foo, first_by_tie_rule(foos.clone(), 24));
        assert!(kept_foo[..15].iter().all(|t| t.starts_with("foo")), "15 个小写的应全部留下并排在前面");
        assert!(kept_foo[15..].iter().all(|t| t.starts_with("Foo")));
        let top_prefix = texts_of(&lex, english.prefix_lookup("fo"));
        assert_eq!(top_prefix, first_by_tie_rule(foos, 6));
        assert!(top_prefix.iter().all(|t| t.starts_with("foo")));
    }

    /// 同分时不含大写字母的排前面：英文词表没有词频，直接按字节序比较会让 Polish 排在 polish 前面，
    /// 前缀补全也偏向人名、地名
    #[test]
    fn test_ties_put_entries_without_uppercase_first() {
        let mut builder = MultiSourceBuilder::new();
        builder.load_english_dict_str(
            "Polish\tpolish\t2500\npolish\tpolish\t2500\nPolk\tpolk\t2500\npolka\tpolka\t2500\n\
             polishing\tpolishing\t2500\npolished\tpolished\t2500\n",
            1.0,
        );
        builder.load_rime_dict_str("---\nname: t\n...\nT恤\tti xu\t300\n体恤\tti xu\t300\n", 1.0, DomainTag::General);
        let lex = builder.build();
        assert_eq!(texts_of(&lex, lex.english_trie().exact_lookup("polish")), ["polish", "Polish"]);
        assert_eq!(
            texts_of(&lex, lex.english_trie().prefix_lookup("pol")),
            ["polish", "polished", "polishing", "polka", "Polish", "Polk"]
        );
        assert_eq!(texts_of(&lex, lex.syllable_trie().exact_lookup("ti'xu")), ["体恤", "T恤"]);
    }
}
