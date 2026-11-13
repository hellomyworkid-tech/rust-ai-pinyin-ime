//! `format`: `AIMELX03` 零拷贝二进制格式的编码器与只读视图
//!
//! 设计目标：加载后词库就是映射字节上的一层视图，加载阶段不为任何词条或 Trie 节点分配堆内存
//! （iOS 键盘扩展的 `phys_footprint` 上限约 177 MB，而只读文件映射页不计入该上限）。
//!
//! 文件布局（全部 Little-Endian，各分区起始偏移按 8 字节对齐）：
//!
//! ```text
//! [0..64)    定长文件头
//!     0   [u8; 8]  魔数 "AIMELX03"
//!     8   u32      header_len（定长头 + 分区表的总字节数）
//!     12  u32      section_count（当前恒为 23）
//!     16  u64      file_len（整个文件的字节数，加载时与实际长度比对以发现截断）
//!     24  u32      entry_count
//!     28  u32      bigram_count
//!     32  u32      音节 Trie 节点数
//!     36  u32      简拼 Trie 节点数
//!     40  u32      英文 Trie 节点数
//!     44  u32      Bigram 哈希槽数量（0 或 2 的幂）
//!     48  u64      内容校验和（覆盖 header_len 之后的全部字节，仅 `verify()` 时校验）
//!     56  u32      flags（保留，恒为 0）
//!     60  u32      文件头校验和（FNV-1a 32，覆盖 [0..60) 与分区表，加载时校验）
//! [64..header_len)  分区表：section_count × { u64 offset, u64 byte_len }
//!
//! 分区 0   词条热区      entry_count × 20 字节：
//!              u32 text_off, u32 key_off, f32 cost, u16 text_len, u16 key_len,
//!              u8 syllable_count, u8 flags(bit0=英文, bit1..2=领域), u16 保留
//! 分区 1   词条冷区      entry_count × 12 字节：f64 raw_weight, u32 tones_off（u32::MAX = 无声调）
//! 分区 2   字符串池      去重后的 UTF-8 字节（词条文本、音节键、Bigram 键）与声调块 [u8 len][tones]
//! 分区 3..21  三棵 Trie（音节 / 简拼 / 英文），每棵 6 个分区，节点按 BFS 重新编号，
//!             因此同一父节点的子节点下标连续：
//!              child_start   u32 × (节点数 + 1)：节点 i 的子节点为 [child_start[i], child_start[i+1])
//!              labels        u8  × 节点数：进入该节点的边上的字节（根节点为 0；同一父节点下严格升序）
//!              exact_start   u32 × (节点数 + 1)：节点 i 的精确命中词条为 exact_ids[exact_start[i]..exact_start[i+1]]
//!              exact_ids     u32 × k
//!              prefix_start  u32 × (节点数 + 1)，若整棵树都没有前缀 Top 缓存则为空分区
//!              prefix_ids    u32 × k
//! 分区 21  Bigram 哈希槽  槽数 × 8 字节：u32 指纹（哈希高 32 位）, u32 记录下标 + 1（0 = 空槽），线性探测
//! 分区 22  Bigram 记录    bigram_count × 12 字节：u32 key_off, u32 key_len, f32 bonus（键为 "prev\tnext"，按键字节序排列）
//! ```
//!
//! 安全性：全部读取都走带边界检查的安全切片代码。加载时只校验文件头与分区表（O(1)）；
//! 字符串在每次访问时做 UTF-8 校验，正文损坏时访问器返回空结果或触发安全 panic。
//! 只要文件在被映射期间不被改动，就不会出现未定义行为；映射会保持到词库被释放为止，
//! 所以词库文件只能通过“写新文件再改名覆盖”来替换，绝不能就地改写或截断（详见 [`CompiledLexicon::load_mmap`]）。
//! 需要完整确认文件完好时调用 [`CompiledLexicon::verify`]：加载不检查正文，
//! 而损坏的正文可能让解码器 panic（例如前缀树里出现越界的词条下标），
//! 因此宿主应在词库安装/更新之后校验一次，失败就拒用该文件。

use crate::{DomainTag, FlatTrie, LexiconEntry, LexiconError, LexiconParts};
use memmap2::Mmap;
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// 当前二进制格式魔数
pub const LEXICON_MAGIC: &[u8; 8] = b"AIMELX03";
/// 上一代（逐条反序列化到堆上的）格式魔数，仅用于给出明确的“格式过旧”错误与一次性迁移
pub const LEGACY_MAGIC_V2: &[u8; 8] = b"AIMELX02";

const HEADER_FIXED_LEN: usize = 64;
const SECTION_ENTRY_LEN: usize = 16;
const SECTION_COUNT: usize = 23;
const HEADER_LEN: usize = HEADER_FIXED_LEN + SECTION_ENTRY_LEN * SECTION_COUNT;

const ENTRY_HOT_LEN: usize = 20;
const ENTRY_COLD_LEN: usize = 12;
const BIGRAM_SLOT_LEN: usize = 8;
const BIGRAM_RECORD_LEN: usize = 12;
const NO_TONES: u32 = u32::MAX;
const FLAG_ENGLISH: u8 = 0b0000_0001;
/// 单个节点的子节点上限（边标签是互不相同的字节，合法文件不可能超过 256）
const MAX_CHILDREN: usize = 256;

const SEC_ENTRY_HOT: usize = 0;
const SEC_ENTRY_COLD: usize = 1;
const SEC_POOL: usize = 2;
const SEC_TRIE_BASE: usize = 3;
const SEC_PER_TRIE: usize = 6;
const SEC_BIGRAM_SLOTS: usize = 21;
const SEC_BIGRAM_RECORDS: usize = 22;

// === 基础读写辅助 ===

#[inline]
fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[inline]
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// 读取 u32 数组分区中相邻的两个元素 `(arr[idx], arr[idx + 1])`，越界时返回 `None`
#[inline]
fn u32_pair(arr: &[u8], idx: u32) -> Option<(u32, u32)> {
    let i = (idx as usize).checked_mul(4)?;
    let pair = arr.get(i..i.checked_add(8)?)?;
    Some((le_u32(&pair[..4]), le_u32(&pair[4..])))
}

/// 取字符串池中的 `[off, off + len)`，越界（或 32 位平台上溢出）时返回 `None`
#[inline]
fn pool_slice(pool: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    pool.get(off..off.checked_add(len)?)
}

const FNV64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

#[inline]
fn fnv1a64_update(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV64_PRIME);
    }
    h
}

/// Bigram 键 `prev \t next` 的哈希（按拼接后的字节流计算，查询时无需真的拼出字符串）
#[inline]
fn bigram_hash(prev: &[u8], next: &[u8]) -> u64 {
    let h = fnv1a64_update(FNV64_OFFSET, prev);
    let h = fnv1a64_update(h, b"\t");
    fnv1a64_update(h, next)
}

fn fnv1a32(chunks: &[&[u8]]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for chunk in chunks {
        for &b in *chunk {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

/// 正文校验和：按 8 字节一组做 FNV 风格混合（比逐字节快一个量级），尾部不足 8 字节的部分逐字节处理
fn content_checksum(bytes: &[u8]) -> u64 {
    let mut h = FNV64_OFFSET ^ (bytes.len() as u64);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ le_u64(c)).wrapping_mul(FNV64_PRIME);
        h ^= h >> 32;
    }
    fnv1a64_update(h, chunks.remainder())
}

// === 编码器：LexiconParts -> AIMELX03 字节 ===

/// 构建期字符串池（整串去重）
#[derive(Default)]
struct PoolBuilder<'a> {
    bytes: Vec<u8>,
    strs: HashMap<&'a [u8], u32>,
    blobs: HashMap<Vec<u8>, u32>,
}

impl<'a> PoolBuilder<'a> {
    fn intern(&mut self, s: &'a [u8]) -> Result<u32, &'static str> {
        if let Some(&off) = self.strs.get(s) {
            return Ok(off);
        }
        let off = u32::try_from(self.bytes.len()).map_err(|_| "string pool exceeds 4 GiB")?;
        if self.bytes.len() + s.len() > u32::MAX as usize {
            return Err("string pool exceeds 4 GiB");
        }
        self.bytes.extend_from_slice(s);
        self.strs.insert(s, off);
        Ok(off)
    }

    fn intern_blob(&mut self, blob: Vec<u8>) -> Result<u32, &'static str> {
        if let Some(&off) = self.blobs.get(&blob) {
            return Ok(off);
        }
        let off = u32::try_from(self.bytes.len()).map_err(|_| "string pool exceeds 4 GiB")?;
        // NO_TONES 被用作“无声调”哨兵，池内偏移不允许取到该值
        if self.bytes.len() + blob.len() >= u32::MAX as usize {
            return Err("string pool exceeds 4 GiB");
        }
        self.bytes.extend_from_slice(&blob);
        self.blobs.insert(blob, off);
        Ok(off)
    }
}

/// 检查词条的派生字段与音节列表是否自洽（`flat_key` / `abbrev_key` / 音节列表在新格式中不落盘，由 `syllable_key` 推导）
fn check_entry_derivable(e: &LexiconEntry) -> Result<(), &'static str> {
    if e.text.len() > u16::MAX as usize {
        return Err("text longer than 65535 bytes");
    }
    if e.syllable_key.len() > u16::MAX as usize {
        return Err("syllable_key longer than 65535 bytes");
    }
    if e.pinyin_syllables.len() > u8::MAX as usize {
        return Err("more than 255 syllables");
    }
    if e.pinyin_syllables.is_empty() {
        if e.flat_key != e.syllable_key {
            return Err("flat_key differs from syllable_key on an entry without syllables");
        }
        if !e.abbrev_key.is_empty() {
            return Err("abbrev_key is not empty on an entry without syllables");
        }
        return Ok(());
    }
    if e.pinyin_syllables.iter().any(|s| s.is_empty() || s.contains('\'')) {
        return Err("empty syllable or syllable containing the separator");
    }
    if !e.syllable_key.split('\'').eq(e.pinyin_syllables.iter().map(|s| s.as_str())) {
        return Err("syllable_key is not the syllables joined by the separator");
    }
    if !e.flat_key.bytes().eq(e.syllable_key.bytes().filter(|&b| b != b'\'')) {
        return Err("flat_key is not the syllables concatenated");
    }
    if !e
        .abbrev_key
        .chars()
        .eq(e.pinyin_syllables.iter().filter_map(|s| s.chars().next()))
    {
        return Err("abbrev_key is not the first letters of the syllables");
    }
    Ok(())
}

struct TrieSections {
    node_count: u32,
    child_start: Vec<u8>,
    labels: Vec<u8>,
    exact_start: Vec<u8>,
    exact_ids: Vec<u8>,
    prefix_start: Vec<u8>,
    prefix_ids: Vec<u8>,
}

/// 把堆上的 `FlatTrie` 按 BFS 顺序重新编号并写成并列数组（子节点下标连续，边只需 1 字节标签）
fn encode_trie(trie: &FlatTrie, entry_count: u32) -> Result<TrieSections, LexiconError> {
    let bad = |msg: &'static str| LexiconError::Unencodable(msg);
    let n = trie.nodes.len();
    if n == 0 {
        return Err(bad("trie has no root node"));
    }
    if n > u32::MAX as usize - 1 {
        return Err(bad("trie has too many nodes"));
    }
    let has_prefix = trie.nodes.iter().any(|nd| !nd.top_prefix_entries.is_empty());

    // order[new_id] = old_id
    let mut order: Vec<u32> = Vec::with_capacity(n);
    order.push(0);
    let mut labels: Vec<u8> = Vec::with_capacity(n);
    labels.push(0);
    let mut child_start = Vec::with_capacity((n + 1) * 4);
    let mut exact_start = Vec::with_capacity((n + 1) * 4);
    let mut exact_ids = Vec::new();
    let mut prefix_start = Vec::with_capacity(if has_prefix { (n + 1) * 4 } else { 0 });
    let mut prefix_ids = Vec::new();

    let mut i = 0usize;
    while i < order.len() {
        let node = trie
            .nodes
            .get(order[i] as usize)
            .ok_or_else(|| bad("trie transition points outside the node array"))?;
        push_u32(&mut child_start, order.len() as u32);
        let mut prev_label: Option<u8> = None;
        for &(b, child) in &node.transitions {
            if prev_label.is_some_and(|p| p >= b) {
                return Err(bad("trie transitions are not strictly ascending"));
            }
            prev_label = Some(b);
            if order.len() >= n {
                return Err(bad("trie is not a tree (a node is reachable twice)"));
            }
            labels.push(b);
            order.push(child);
        }
        push_u32(&mut exact_start, (exact_ids.len() / 4) as u32);
        for &id in &node.exact_entries {
            if id >= entry_count {
                return Err(bad("trie references an entry id out of range"));
            }
            push_u32(&mut exact_ids, id);
        }
        if has_prefix {
            push_u32(&mut prefix_start, (prefix_ids.len() / 4) as u32);
            for &id in &node.top_prefix_entries {
                if id >= entry_count {
                    return Err(bad("trie references an entry id out of range"));
                }
                push_u32(&mut prefix_ids, id);
            }
        }
        i += 1;
    }
    if exact_ids.len() / 4 > u32::MAX as usize || prefix_ids.len() / 4 > u32::MAX as usize {
        return Err(bad("trie has too many entry references"));
    }
    push_u32(&mut child_start, order.len() as u32);
    push_u32(&mut exact_start, (exact_ids.len() / 4) as u32);
    if has_prefix {
        push_u32(&mut prefix_start, (prefix_ids.len() / 4) as u32);
    }

    Ok(TrieSections {
        node_count: order.len() as u32,
        child_start,
        labels,
        exact_start,
        exact_ids,
        prefix_start,
        prefix_ids,
    })
}

/// 把构建期的堆结构编码为 `AIMELX03` 字节镜像
pub(crate) fn encode(parts: &LexiconParts) -> Result<Vec<u8>, LexiconError> {
    let entry_count = u32::try_from(parts.entries.len())
        .map_err(|_| LexiconError::Unencodable("too many entries"))?;
    let mut pool = PoolBuilder::default();

    let mut hot = Vec::with_capacity(parts.entries.len() * ENTRY_HOT_LEN);
    let mut cold = Vec::with_capacity(parts.entries.len() * ENTRY_COLD_LEN);
    for (index, e) in parts.entries.iter().enumerate() {
        let invalid = |reason: &'static str| LexiconError::UnencodableEntry { index, reason };
        check_entry_derivable(e).map_err(invalid)?;
        let text_off = pool.intern(e.text.as_bytes()).map_err(invalid)?;
        let key_off = pool.intern(e.syllable_key.as_bytes()).map_err(invalid)?;
        let tones_off = match e.tones.as_deref() {
            Some(t) if !t.is_empty() => {
                if t.len() > u8::MAX as usize {
                    return Err(invalid("more than 255 tones"));
                }
                let mut blob = Vec::with_capacity(t.len() + 1);
                blob.push(t.len() as u8);
                blob.extend_from_slice(t);
                pool.intern_blob(blob).map_err(invalid)?
            }
            _ => NO_TONES,
        };
        let flags = (if e.is_english { FLAG_ENGLISH } else { 0 }) | ((e.domain as u8 & 0b11) << 1);

        push_u32(&mut hot, text_off);
        push_u32(&mut hot, key_off);
        hot.extend_from_slice(&e.cost.to_le_bytes());
        hot.extend_from_slice(&(e.text.len() as u16).to_le_bytes());
        hot.extend_from_slice(&(e.syllable_key.len() as u16).to_le_bytes());
        hot.push(e.pinyin_syllables.len() as u8);
        hot.push(flags);
        hot.extend_from_slice(&[0, 0]);

        cold.extend_from_slice(&e.raw_weight.to_le_bytes());
        push_u32(&mut cold, tones_off);
    }

    let tries = [
        encode_trie(&parts.syllable_trie, entry_count)?,
        encode_trie(&parts.abbrev_trie, entry_count)?,
        encode_trie(&parts.english_trie, entry_count)?,
    ];

    // Bigram：记录按键字节序排列（保证同一份输入产出的字节确定），再建开放寻址哈希槽
    let mut bigram_keys: Vec<(&str, f32)> = parts.bigrams.iter().map(|(k, &v)| (k.as_str(), v)).collect();
    bigram_keys.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let bigram_count = u32::try_from(bigram_keys.len())
        .ok()
        .filter(|&c| c < u32::MAX / 4)
        .ok_or(LexiconError::Unencodable("too many bigrams"))?;
    let slot_count: u32 = if bigram_count == 0 {
        0
    } else {
        (bigram_count * 2).next_power_of_two().max(8)
    };
    let mut slots = vec![0u8; slot_count as usize * BIGRAM_SLOT_LEN];
    let mut records = Vec::with_capacity(bigram_keys.len() * BIGRAM_RECORD_LEN);
    for (rec_idx, (key, bonus)) in bigram_keys.iter().enumerate() {
        let key_off = pool
            .intern(key.as_bytes())
            .map_err(LexiconError::Unencodable)?;
        push_u32(&mut records, key_off);
        push_u32(&mut records, key.len() as u32);
        records.extend_from_slice(&bonus.to_le_bytes());

        let h = fnv1a64_update(FNV64_OFFSET, key.as_bytes());
        let mask = slot_count as usize - 1;
        let mut slot = h as usize & mask;
        loop {
            let s = &mut slots[slot * BIGRAM_SLOT_LEN..(slot + 1) * BIGRAM_SLOT_LEN];
            if le_u32(&s[4..]) == 0 {
                s[..4].copy_from_slice(&((h >> 32) as u32).to_le_bytes());
                s[4..].copy_from_slice(&(rec_idx as u32 + 1).to_le_bytes());
                break;
            }
            slot = (slot + 1) & mask;
        }
    }

    let [syl, abbrev, english] = tries;
    let node_counts = [syl.node_count, abbrev.node_count, english.node_count];
    let mut sections: Vec<Vec<u8>> = Vec::with_capacity(SECTION_COUNT);
    sections.push(hot);
    sections.push(cold);
    sections.push(pool.bytes);
    for t in [syl, abbrev, english] {
        sections.push(t.child_start);
        sections.push(t.labels);
        sections.push(t.exact_start);
        sections.push(t.exact_ids);
        sections.push(t.prefix_start);
        sections.push(t.prefix_ids);
    }
    sections.push(slots);
    sections.push(records);
    debug_assert_eq!(sections.len(), SECTION_COUNT);

    let body_len: usize = sections.iter().map(|s| (s.len() + 7) & !7).sum();
    let mut out = Vec::with_capacity(HEADER_LEN + body_len);
    out.resize(HEADER_LEN, 0u8);
    let mut table = Vec::with_capacity(SECTION_COUNT * SECTION_ENTRY_LEN);
    for sec in &sections {
        while out.len() % 8 != 0 {
            out.push(0);
        }
        table.extend_from_slice(&(out.len() as u64).to_le_bytes());
        table.extend_from_slice(&(sec.len() as u64).to_le_bytes());
        out.extend_from_slice(sec);
    }
    drop(sections);

    let file_len = out.len() as u64;
    let checksum = content_checksum(&out[HEADER_LEN..]);
    out[0..8].copy_from_slice(LEXICON_MAGIC);
    out[8..12].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    out[12..16].copy_from_slice(&(SECTION_COUNT as u32).to_le_bytes());
    out[16..24].copy_from_slice(&file_len.to_le_bytes());
    out[24..28].copy_from_slice(&entry_count.to_le_bytes());
    out[28..32].copy_from_slice(&bigram_count.to_le_bytes());
    out[32..36].copy_from_slice(&node_counts[0].to_le_bytes());
    out[36..40].copy_from_slice(&node_counts[1].to_le_bytes());
    out[40..44].copy_from_slice(&node_counts[2].to_le_bytes());
    out[44..48].copy_from_slice(&slot_count.to_le_bytes());
    out[48..56].copy_from_slice(&checksum.to_le_bytes());
    out[56..60].copy_from_slice(&0u32.to_le_bytes());
    out[HEADER_FIXED_LEN..HEADER_LEN].copy_from_slice(&table);
    let header_sum = fnv1a32(&[&out[..60], &out[HEADER_FIXED_LEN..HEADER_LEN]]);
    out[60..64].copy_from_slice(&header_sum.to_le_bytes());
    Ok(out)
}

// === 解码：文件头与分区表校验 ===

#[derive(Debug, Clone)]
struct TrieLayout {
    node_count: u32,
    child_start: Range<usize>,
    labels: Range<usize>,
    exact_start: Range<usize>,
    exact_ids: Range<usize>,
    prefix_start: Range<usize>,
    prefix_ids: Range<usize>,
}

#[derive(Debug, Clone)]
struct Layout {
    entry_count: u32,
    bigram_count: u32,
    bigram_slot_count: u32,
    content_checksum: u64,
    entries_hot: Range<usize>,
    entries_cold: Range<usize>,
    pool: Range<usize>,
    tries: [TrieLayout; 3],
    bigram_slots: Range<usize>,
    bigram_records: Range<usize>,
}

impl Layout {
    /// 校验魔数、文件长度、文件头校验和与每个分区的范围和尺寸；全部为 O(1) 工作，不触碰正文页
    fn parse(data: &[u8]) -> Result<Self, LexiconError> {
        if data.len() < 8 {
            return Err(LexiconError::InvalidMagic);
        }
        if &data[..8] == LEGACY_MAGIC_V2 {
            return Err(LexiconError::OutdatedFormat(
                String::from_utf8_lossy(LEGACY_MAGIC_V2).into_owned(),
            ));
        }
        if &data[..8] != LEXICON_MAGIC {
            return Err(LexiconError::InvalidMagic);
        }
        if data.len() < HEADER_LEN {
            return Err(LexiconError::CorruptedBinary(data.len()));
        }
        if le_u32(&data[8..12]) as usize != HEADER_LEN {
            return Err(LexiconError::CorruptedBinary(8));
        }
        if le_u32(&data[12..16]) as usize != SECTION_COUNT {
            return Err(LexiconError::CorruptedBinary(12));
        }
        let expected_sum = fnv1a32(&[&data[..60], &data[HEADER_FIXED_LEN..HEADER_LEN]]);
        if le_u32(&data[60..64]) != expected_sum {
            return Err(LexiconError::CorruptedBinary(60));
        }
        let file_len = le_u64(&data[16..24]);
        if file_len != data.len() as u64 {
            return Err(LexiconError::SizeMismatch {
                expected: file_len,
                actual: data.len() as u64,
            });
        }
        if le_u32(&data[56..60]) != 0 {
            return Err(LexiconError::CorruptedBinary(56));
        }

        let entry_count = le_u32(&data[24..28]);
        let bigram_count = le_u32(&data[28..32]);
        let node_counts = [
            le_u32(&data[32..36]),
            le_u32(&data[36..40]),
            le_u32(&data[40..44]),
        ];
        let bigram_slot_count = le_u32(&data[44..48]);
        let content_checksum = le_u64(&data[48..56]);

        // 取第 idx 个分区的字节范围；`expect_len` 为 Some 时要求分区长度恰好等于它
        let section = |idx: usize, expect_len: Option<u64>, unit: u64| -> Result<Range<usize>, LexiconError> {
            let at = HEADER_FIXED_LEN + idx * SECTION_ENTRY_LEN;
            let bad = || LexiconError::CorruptedBinary(at);
            let off = le_u64(&data[at..at + 8]);
            let len = le_u64(&data[at + 8..at + 16]);
            let end = off.checked_add(len).ok_or_else(bad)?;
            if off < HEADER_LEN as u64 || off % 8 != 0 || end > file_len || len % unit != 0 {
                return Err(bad());
            }
            if expect_len.is_some_and(|e| e != len) {
                return Err(bad());
            }
            // off / end 均不超过 data.len()，转换为 usize 不会截断
            Ok(off as usize..end as usize)
        };

        let entries_hot = section(
            SEC_ENTRY_HOT,
            Some(entry_count as u64 * ENTRY_HOT_LEN as u64),
            ENTRY_HOT_LEN as u64,
        )?;
        let entries_cold = section(
            SEC_ENTRY_COLD,
            Some(entry_count as u64 * ENTRY_COLD_LEN as u64),
            ENTRY_COLD_LEN as u64,
        )?;
        let pool = section(SEC_POOL, None, 1)?;

        let trie = |t: usize| -> Result<TrieLayout, LexiconError> {
            let base = SEC_TRIE_BASE + t * SEC_PER_TRIE;
            let node_count = node_counts[t];
            if node_count == 0 {
                return Err(LexiconError::CorruptedBinary(32 + t * 4));
            }
            let starts_len = (node_count as u64 + 1) * 4;
            let prefix_start = section(base + 4, None, 4)?;
            if !prefix_start.is_empty() && prefix_start.len() as u64 != starts_len {
                return Err(LexiconError::CorruptedBinary(
                    HEADER_FIXED_LEN + (base + 4) * SECTION_ENTRY_LEN,
                ));
            }
            Ok(TrieLayout {
                node_count,
                child_start: section(base, Some(starts_len), 4)?,
                labels: section(base + 1, Some(node_count as u64), 1)?,
                exact_start: section(base + 2, Some(starts_len), 4)?,
                exact_ids: section(base + 3, None, 4)?,
                prefix_start,
                prefix_ids: section(base + 5, None, 4)?,
            })
        };
        let tries = [trie(0)?, trie(1)?, trie(2)?];

        let slots_ok = if bigram_count == 0 {
            bigram_slot_count == 0
        } else {
            bigram_slot_count.is_power_of_two() && bigram_slot_count > bigram_count
        };
        if !slots_ok {
            return Err(LexiconError::CorruptedBinary(44));
        }
        let bigram_slots = section(
            SEC_BIGRAM_SLOTS,
            Some(bigram_slot_count as u64 * BIGRAM_SLOT_LEN as u64),
            BIGRAM_SLOT_LEN as u64,
        )?;
        let bigram_records = section(
            SEC_BIGRAM_RECORDS,
            Some(bigram_count as u64 * BIGRAM_RECORD_LEN as u64),
            BIGRAM_RECORD_LEN as u64,
        )?;

        Ok(Layout {
            entry_count,
            bigram_count,
            bigram_slot_count,
            content_checksum,
            entries_hot,
            entries_cold,
            pool,
            tries,
            bigram_slots,
            bigram_records,
        })
    }
}

// === 只读视图 ===

enum Backing {
    Mmap(Mmap),
    Owned(Vec<u8>),
}

/// 编译完成的多源融合词库镜像：`AIMELX03` 字节上的零拷贝只读视图。
///
/// 底层存储要么是只读文件映射（[`CompiledLexicon::load_mmap`]），要么是自有的 `Vec<u8>`
/// （[`CompiledLexicon::from_bytes`] / `MultiSourceBuilder::build`，无需落盘）。
/// `clone()` 只增加引用计数，多个引擎可以共享同一份映射。
#[derive(Clone)]
pub struct CompiledLexicon {
    backing: Arc<Backing>,
    layout: Layout,
}

impl std::fmt::Debug for CompiledLexicon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledLexicon")
            .field("bytes", &self.data().len())
            .field("mmap", &matches!(*self.backing, Backing::Mmap(_)))
            .field("entries", &self.layout.entry_count)
            .field("syllable_trie_nodes", &self.layout.tries[0].node_count)
            .field("abbrev_trie_nodes", &self.layout.tries[1].node_count)
            .field("english_trie_nodes", &self.layout.tries[2].node_count)
            .field("bigrams", &self.layout.bigram_count)
            .finish()
    }
}

impl CompiledLexicon {
    pub(crate) fn from_mmap(mmap: Mmap) -> Result<Self, LexiconError> {
        let layout = Layout::parse(&mmap)?;
        Ok(Self {
            backing: Arc::new(Backing::Mmap(mmap)),
            layout,
        })
    }

    /// 直接以内存中的 `AIMELX03` 字节作为底层存储创建词库视图（仅校验文件头与分区表）
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, LexiconError> {
        let layout = Layout::parse(&bytes)?;
        Ok(Self {
            backing: Arc::new(Backing::Owned(bytes)),
            layout,
        })
    }

    /// 把构建期的堆结构编码成 `AIMELX03` 并在其上建立视图（不落盘）。
    /// 词条的派生字段与音节列表不自洽、或 Trie 不是合法的树时返回错误。
    pub fn from_parts(parts: &LexiconParts) -> Result<Self, LexiconError> {
        Self::from_bytes(encode(parts)?)
    }

    #[inline]
    fn data(&self) -> &[u8] {
        match &*self.backing {
            Backing::Mmap(m) => m,
            Backing::Owned(v) => v,
        }
    }

    #[inline]
    fn sec(&self, r: &Range<usize>) -> &[u8] {
        // 分区范围在加载时已对照文件长度校验
        &self.data()[r.start..r.end]
    }

    /// 完整的 `AIMELX03` 字节镜像（`save_to_file` 写出的就是它）
    pub fn as_bytes(&self) -> &[u8] {
        self.data()
    }

    /// 底层存储是否为只读文件映射
    pub fn is_mmap(&self) -> bool {
        matches!(*self.backing, Backing::Mmap(_))
    }

    /// 词条总数
    #[inline]
    pub fn entry_count(&self) -> usize {
        self.layout.entry_count as usize
    }

    /// 按下标取词条句柄（下标越界时 panic，与此前的 `entries[id]` 行为一致）
    #[inline]
    pub fn entry(&self, id: u32) -> EntryRef<'_> {
        match self.get_entry(id) {
            Some(e) => e,
            None => panic!(
                "lexicon entry id {id} out of range (entry_count = {})",
                self.layout.entry_count
            ),
        }
    }

    /// 按下标取词条句柄，越界时返回 `None`
    #[inline]
    pub fn get_entry(&self, id: u32) -> Option<EntryRef<'_>> {
        let start = (id as usize).checked_mul(ENTRY_HOT_LEN)?;
        let rec = self
            .sec(&self.layout.entries_hot)
            .get(start..start.checked_add(ENTRY_HOT_LEN)?)?;
        let hot: &[u8; ENTRY_HOT_LEN] = rec.try_into().ok()?;
        Some(EntryRef { lex: self, hot, id })
    }

    /// 按 cost 升序遍历全部词条
    pub fn entries(&self) -> impl ExactSizeIterator<Item = EntryRef<'_>> + '_ {
        (0..self.layout.entry_count).map(move |id| self.entry(id))
    }

    #[inline]
    fn trie(&self, t: usize) -> TrieView<'_> {
        let l = &self.layout.tries[t];
        TrieView {
            node_count: l.node_count,
            child_start: self.sec(&l.child_start),
            labels: self.sec(&l.labels),
            exact_start: self.sec(&l.exact_start),
            exact_ids: self.sec(&l.exact_ids),
            prefix_start: self.sec(&l.prefix_start),
            prefix_ids: self.sec(&l.prefix_ids),
        }
    }

    /// 精确音节序列索引（键格式：`"bu'zai'yi'le"` 或 `"xi'an"`）
    #[inline]
    pub fn syllable_trie(&self) -> TrieView<'_> {
        self.trie(0)
    }

    /// 首字母简拼索引（键格式：`"bzyl"` -> `"不在意了"`, `"jsj"` -> `"计算机"`）
    #[inline]
    pub fn abbrev_trie(&self) -> TrieView<'_> {
        self.trie(1)
    }

    /// 英文与混输大小写规范前缀树（键格式：小写 `"typescript"` -> 词条 `"TypeScript"`）
    #[inline]
    pub fn english_trie(&self) -> TrieView<'_> {
        self.trie(2)
    }

    /// 二元语法转移对的数量
    #[inline]
    pub fn bigram_count(&self) -> usize {
        self.layout.bigram_count as usize
    }

    /// 查询二元语法转移加成（支持中文-中文、中文-英文、英文-中文跨语种转移）。
    /// 直接对 `prev \t next` 的字节流求哈希并在映射上探测，不分配任何内存。
    #[inline]
    pub fn bigram_bonus(&self, prev_word: &str, next_word: &str) -> f32 {
        if prev_word.is_empty() || self.layout.bigram_count == 0 {
            return 0.0;
        }
        self.bigram_lookup(prev_word.as_bytes(), next_word.as_bytes())
            .unwrap_or(0.0)
    }

    fn bigram_lookup(&self, prev: &[u8], next: &[u8]) -> Option<f32> {
        let slots = self.sec(&self.layout.bigram_slots);
        let slot_count = self.layout.bigram_slot_count as usize;
        let mask = slot_count.checked_sub(1)?;
        let h = bigram_hash(prev, next);
        let fingerprint = (h >> 32) as u32;
        let key_len = prev.len() + 1 + next.len();
        let mut slot = h as usize & mask;
        // 探测次数以槽数为上限：即使文件损坏成“满表”也不会死循环
        for _ in 0..slot_count {
            let s = slots.get(slot * BIGRAM_SLOT_LEN..(slot + 1) * BIGRAM_SLOT_LEN)?;
            let rec_plus_one = le_u32(&s[4..]);
            if rec_plus_one == 0 {
                return None;
            }
            if le_u32(&s[..4]) == fingerprint {
                if let Some((key, bonus)) = self.bigram_record(rec_plus_one - 1) {
                    if key.len() == key_len
                        && key[prev.len()] == b'\t'
                        && &key[..prev.len()] == prev
                        && &key[prev.len() + 1..] == next
                    {
                        return Some(bonus);
                    }
                }
            }
            slot = (slot + 1) & mask;
        }
        None
    }

    #[inline]
    fn bigram_record(&self, idx: u32) -> Option<(&[u8], f32)> {
        let start = (idx as usize).checked_mul(BIGRAM_RECORD_LEN)?;
        let rec = self
            .sec(&self.layout.bigram_records)
            .get(start..start.checked_add(BIGRAM_RECORD_LEN)?)?;
        let off = le_u32(&rec[..4]) as usize;
        let len = le_u32(&rec[4..8]) as usize;
        let key = pool_slice(self.sec(&self.layout.pool), off, len)?;
        Some((key, f32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]])))
    }

    /// 遍历全部二元语法转移对 `(键 "prev\tnext", bonus)`，按键的字节序排列
    pub fn bigram_entries(&self) -> impl Iterator<Item = (&str, f32)> + '_ {
        (0..self.layout.bigram_count).filter_map(move |i| {
            let (key, bonus) = self.bigram_record(i)?;
            Some((std::str::from_utf8(key).ok()?, bonus))
        })
    }

    /// 完整性深度校验：比对正文校验和，并逐项检查所有偏移、下标、UTF-8、cost 是否为有限值与 Trie 结构
    /// （每棵 Trie 必须是一棵所有节点都可达的树，不存在环）。
    /// 需要读完整个文件（全量词库约几十毫秒），适合在安装/拷贝词库之后执行一次，而不是每次加载都执行。
    /// 通过校验的词库不会让访问器 panic，[`CompiledLexicon::entry`] 也不会收到越界的词条下标。
    pub fn verify(&self) -> Result<(), LexiconError> {
        let data = self.data();
        if content_checksum(&data[HEADER_LEN..]) != self.layout.content_checksum {
            return Err(LexiconError::CorruptedBinary(48));
        }
        let l = &self.layout;
        let pool = self.sec(&l.pool);

        // 词条：字符串范围、UTF-8、音节数与音节键一致、声调块
        let hot = self.sec(&l.entries_hot);
        let cold = self.sec(&l.entries_cold);
        for i in 0..l.entry_count as usize {
            let at = l.entries_hot.start + i * ENTRY_HOT_LEN;
            let bad = || LexiconError::CorruptedBinary(at);
            let h = &hot[i * ENTRY_HOT_LEN..(i + 1) * ENTRY_HOT_LEN];
            let text_off = le_u32(&h[0..4]) as usize;
            let key_off = le_u32(&h[4..8]) as usize;
            let text_len = u16::from_le_bytes([h[12], h[13]]) as usize;
            let key_len = u16::from_le_bytes([h[14], h[15]]) as usize;
            let syl_count = h[16] as usize;
            let text = pool_slice(pool, text_off, text_len).ok_or_else(bad)?;
            std::str::from_utf8(text).map_err(|_| bad())?;
            let key = pool_slice(pool, key_off, key_len).ok_or_else(bad)?;
            let key = std::str::from_utf8(key).map_err(|_| bad())?;
            if syl_count > 0 && key.split('\'').count() != syl_count {
                return Err(bad());
            }
            if h[17] & !0b0000_0111 != 0 || h[18] != 0 || h[19] != 0 {
                return Err(bad());
            }
            // NaN / 无穷大的 cost 会让解码器里的排序比较失去全序（标准库会因此 panic）
            if !f32::from_le_bytes([h[8], h[9], h[10], h[11]]).is_finite() {
                return Err(bad());
            }
            let c = &cold[i * ENTRY_COLD_LEN..(i + 1) * ENTRY_COLD_LEN];
            let tones_off = le_u32(&c[8..12]);
            if tones_off != NO_TONES {
                let at = l.entries_cold.start + i * ENTRY_COLD_LEN;
                let off = tones_off as usize;
                let len = *pool.get(off).ok_or(LexiconError::CorruptedBinary(at))? as usize;
                if len == 0 || pool_slice(pool, off + 1, len).is_none() {
                    return Err(LexiconError::CorruptedBinary(at));
                }
            }
        }

        // Trie：子节点范围单调且恰好铺满 [1, node_count)，子节点下标必须大于父节点（BFS 编号下恒成立；
        // 两者合起来保证这是一棵所有节点都可达的树、不存在环），同一父节点下标签严格升序，词条下标不越界
        for tl in &l.tries {
            let n = tl.node_count as usize;
            let child_start = self.sec(&tl.child_start);
            let labels = self.sec(&tl.labels);
            let bad_at = |i: usize| LexiconError::CorruptedBinary(tl.child_start.start + i * 4);
            let mut expected_lo = 1usize;
            for i in 0..n {
                let lo = le_u32(&child_start[i * 4..]) as usize;
                let hi = le_u32(&child_start[(i + 1) * 4..]) as usize;
                if lo != expected_lo || lo <= i {
                    return Err(bad_at(i));
                }
                if hi < lo || hi > n || hi - lo > MAX_CHILDREN {
                    return Err(bad_at(i));
                }
                if labels[lo..hi].windows(2).any(|w| w[0] >= w[1]) {
                    return Err(LexiconError::CorruptedBinary(tl.labels.start + lo));
                }
                expected_lo = hi;
            }
            if expected_lo != n {
                return Err(bad_at(n));
            }
            for (starts_r, ids_r) in [(&tl.exact_start, &tl.exact_ids), (&tl.prefix_start, &tl.prefix_ids)] {
                let starts = self.sec(starts_r);
                let ids = self.sec(ids_r);
                if starts.is_empty() {
                    if !ids.is_empty() {
                        return Err(LexiconError::CorruptedBinary(ids_r.start));
                    }
                    continue;
                }
                let mut prev = 0usize;
                for i in 0..=n {
                    let v = le_u32(&starts[i * 4..]) as usize;
                    if v < prev || v > ids.len() / 4 || (i == 0 && v != 0) {
                        return Err(LexiconError::CorruptedBinary(starts_r.start + i * 4));
                    }
                    prev = v;
                }
                if prev != ids.len() / 4 {
                    return Err(LexiconError::CorruptedBinary(starts_r.start + n * 4));
                }
                for (k, id) in ids.chunks_exact(4).enumerate() {
                    if le_u32(id) >= l.entry_count {
                        return Err(LexiconError::CorruptedBinary(ids_r.start + k * 4));
                    }
                }
            }
        }

        // Bigram：每条记录的键在池内且为 UTF-8，并且能通过哈希槽查回自己
        for i in 0..l.bigram_count {
            let at = l.bigram_records.start + i as usize * BIGRAM_RECORD_LEN;
            let (key, bonus) = self.bigram_record(i).ok_or(LexiconError::CorruptedBinary(at))?;
            std::str::from_utf8(key).map_err(|_| LexiconError::CorruptedBinary(at))?;
            // 不含制表符的键无法通过 (prev, next) 查到，此前的 HashMap 实现同样如此，这里不视为损坏
            if let Some(tab) = key.iter().position(|&b| b == b'\t') {
                let found = self.bigram_lookup(&key[..tab], &key[tab + 1..]);
                if found.map(f32::to_bits) != Some(bonus.to_bits()) {
                    return Err(LexiconError::CorruptedBinary(at));
                }
            }
        }
        Ok(())
    }
}

/// 词条句柄：指向映射中一条 20 字节热区记录的轻量 `Copy` 视图
#[derive(Clone, Copy)]
pub struct EntryRef<'a> {
    lex: &'a CompiledLexicon,
    hot: &'a [u8; ENTRY_HOT_LEN],
    id: u32,
}

impl<'a> EntryRef<'a> {
    /// 词条下标（即 Trie 中记录的词条 ID）
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    #[inline]
    fn pool_str(&self, off: u32, len: u16) -> &'a str {
        pool_slice(self.lex.sec(&self.lex.layout.pool), off as usize, len as usize)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }

    /// 最终上屏文本（如 `"不在意了"`, `"TypeScript"`）。每次访问都会校验 UTF-8，文件损坏时返回空串。
    #[inline]
    pub fn text(&self) -> &'a str {
        self.pool_str(le_u32(&self.hot[0..4]), u16::from_le_bytes([self.hot[12], self.hot[13]]))
    }

    /// 音节分隔检索键（如 `"bu'zai'yi'le"`，英文词为小写 `"typescript"`）
    #[inline]
    pub fn syllable_key(&self) -> &'a str {
        self.pool_str(le_u32(&self.hot[4..8]), u16::from_le_bytes([self.hot[14], self.hot[15]]))
    }

    /// 标准无调拼音音节数量（纯英文词条为 0）
    #[inline]
    pub fn syllable_count(&self) -> usize {
        self.hot[16] as usize
    }

    /// 标准无调拼音音节列表（由 `syllable_key` 按 `'` 切分得到，纯英文词条为空）
    #[inline]
    pub fn syllables(&self) -> Syllables<'a> {
        Syllables {
            inner: if self.syllable_count() == 0 {
                None
            } else {
                Some(self.syllable_key().split('\''))
            },
        }
    }

    /// 连续无分隔检索键（如 `"buzaiyile"`）。英文词条与单音节词条直接借用 `syllable_key`，
    /// 只有多音节中文词条需要去掉分隔符而分配一次（热路径只对英文词条取该键）。
    #[inline]
    pub fn flat_key(&self) -> Cow<'a, str> {
        let key = self.syllable_key();
        if self.syllable_count() <= 1 {
            Cow::Borrowed(key)
        } else {
            Cow::Owned(key.chars().filter(|&c| c != '\'').collect())
        }
    }

    /// 首字母简拼检索键（如 `"bzyl"`，英文词条为空串）。由音节推导并分配，仅供构建与离线工具使用。
    pub fn abbrev_key(&self) -> String {
        self.syllables().filter_map(|s| s.chars().next()).collect()
    }

    /// 归一化后的负对数概率代价，数值越小优先级越高
    #[inline]
    pub fn cost(&self) -> f32 {
        f32::from_le_bytes([self.hot[8], self.hot[9], self.hot[10], self.hot[11]])
    }

    /// 是否为英文/混输词条
    #[inline]
    pub fn is_english(&self) -> bool {
        self.hot[17] & FLAG_ENGLISH != 0
    }

    /// 垂直领域分类
    #[inline]
    pub fn domain(&self) -> DomainTag {
        DomainTag::from_u8((self.hot[17] >> 1) & 0b11)
    }

    #[inline]
    fn cold(&self) -> Option<&'a [u8]> {
        pool_slice(
            self.lex.sec(&self.lex.layout.entries_cold),
            (self.id as usize).checked_mul(ENTRY_COLD_LEN)?,
            ENTRY_COLD_LEN,
        )
    }

    /// 原始融合频次权重（冷数据，位于独立分区）
    pub fn raw_weight(&self) -> f64 {
        self.cold()
            .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .unwrap_or(0.0)
    }

    /// 可选的万象声调信息（1=阴平, 2=阳平, 3=上声, 4=去声, 5=轻声）
    pub fn tones(&self) -> Option<&'a [u8]> {
        let off = le_u32(&self.cold()?[8..12]);
        if off == NO_TONES {
            return None;
        }
        let pool = self.lex.sec(&self.lex.layout.pool);
        let off = off as usize;
        let len = *pool.get(off)? as usize;
        pool_slice(pool, off.checked_add(1)?, len)
    }

    /// 还原为自有的堆上词条结构（分配多个字符串，仅供测试与离线工具使用）
    pub fn to_entry(&self) -> LexiconEntry {
        LexiconEntry {
            text: self.text().to_string(),
            pinyin_syllables: self.syllables().map(|s| s.to_string()).collect(),
            syllable_key: self.syllable_key().to_string(),
            flat_key: self.flat_key().into_owned(),
            abbrev_key: self.abbrev_key(),
            cost: self.cost(),
            raw_weight: self.raw_weight(),
            is_english: self.is_english(),
            domain: self.domain(),
            tones: self.tones().map(|t| t.to_vec()),
        }
    }
}

impl std::fmt::Debug for EntryRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryRef")
            .field("id", &self.id)
            .field("text", &self.text())
            .field("syllable_key", &self.syllable_key())
            .field("cost", &self.cost())
            .field("is_english", &self.is_english())
            .field("domain", &self.domain())
            .finish()
    }
}

/// 词条音节迭代器（见 [`EntryRef::syllables`]）
#[derive(Debug, Clone)]
pub struct Syllables<'a> {
    inner: Option<std::str::Split<'a, char>>,
}

impl<'a> Iterator for Syllables<'a> {
    type Item = &'a str;

    #[inline]
    fn next(&mut self) -> Option<&'a str> {
        self.inner.as_mut()?.next()
    }
}

impl DoubleEndedIterator for Syllables<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        self.inner.as_mut()?.next_back()
    }
}

/// 词条 ID 列表：映射中一段 Little-Endian `u32` 的轻量 `Copy` 视图（顺序与构建时写入的顺序一致，即按 cost 升序）
#[derive(Clone, Copy, Default)]
pub struct IdList<'a> {
    bytes: &'a [u8],
}

impl<'a> IdList<'a> {
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() < 4
    }

    #[inline]
    pub fn get(&self, idx: usize) -> Option<u32> {
        let start = idx.checked_mul(4)?;
        self.bytes.get(start..start.checked_add(4)?).map(le_u32)
    }

    #[inline]
    pub fn first(&self) -> Option<u32> {
        self.get(0)
    }

    #[inline]
    pub fn iter(&self) -> IdIter<'a> {
        IdIter {
            chunks: self.bytes.chunks_exact(4),
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        self.iter().collect()
    }
}

impl std::fmt::Debug for IdList<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for IdList<'a> {
    type Item = u32;
    type IntoIter = IdIter<'a>;

    #[inline]
    fn into_iter(self) -> IdIter<'a> {
        self.iter()
    }
}

/// [`IdList`] 的迭代器，按值产出 `u32`
#[derive(Debug, Clone)]
pub struct IdIter<'a> {
    chunks: std::slice::ChunksExact<'a, u8>,
}

impl Iterator for IdIter<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        self.chunks.next().map(le_u32)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}

impl DoubleEndedIterator for IdIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<u32> {
        self.chunks.next_back().map(le_u32)
    }
}

impl ExactSizeIterator for IdIter<'_> {}

/// 扁平化前缀树的只读视图（`Copy`，可按值传递）。节点下标 0 为根节点。
///
/// 语义与构建期的 `FlatTrie` 完全一致：`find_child` / `advance_node` / `exact_lookup` /
/// `prefix_lookup` / `common_prefix_search` 返回的词条 ID 顺序即插入顺序（cost 升序），
/// 子节点按边字节升序遍历。节点下标是文件内部编号（BFS 序），不要跨词库文件保存。
#[derive(Clone, Copy)]
pub struct TrieView<'a> {
    node_count: u32,
    child_start: &'a [u8],
    labels: &'a [u8],
    exact_start: &'a [u8],
    exact_ids: &'a [u8],
    prefix_start: &'a [u8],
    prefix_ids: &'a [u8],
}

impl std::fmt::Debug for TrieView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrieView")
            .field("node_count", &self.node_count)
            .finish()
    }
}

impl<'a> TrieView<'a> {
    /// 节点总数（含根节点）
    #[inline]
    pub fn node_count(&self) -> usize {
        self.node_count as usize
    }

    /// 节点 `node` 的子节点：返回 (首个子节点下标, 各子节点的边标签)。文件损坏导致范围非法时视为无子节点。
    #[inline]
    fn child_labels(&self, node: u32) -> (u32, &'a [u8]) {
        let Some((lo, hi)) = u32_pair(self.child_start, node) else {
            return (0, &[]);
        };
        // lo > hi 时 wrapping_sub 得到极大值，同样被上限挡掉
        if (hi as usize).wrapping_sub(lo as usize) > MAX_CHILDREN {
            return (0, &[]);
        }
        match self.labels.get(lo as usize..hi as usize) {
            Some(labels) => (lo, labels),
            None => (0, &[]),
        }
    }

    #[inline]
    fn id_list(starts: &'a [u8], ids: &'a [u8], node: u32) -> IdList<'a> {
        let Some((lo, hi)) = u32_pair(starts, node) else {
            return IdList::default();
        };
        let range = (lo as usize).checked_mul(4).zip((hi as usize).checked_mul(4));
        match range.and_then(|(lo, hi)| ids.get(lo..hi)) {
            Some(bytes) => IdList { bytes },
            None => IdList::default(),
        }
    }

    /// 沿字节 `b` 的边走到子节点
    #[inline]
    pub fn find_child(&self, node: u32, b: u8) -> Option<u32> {
        let (first, labels) = self.child_labels(node);
        let pos = if labels.len() <= 8 {
            labels.iter().position(|&l| l == b)?
        } else {
            labels.binary_search(&b).ok()?
        };
        Some(first + pos as u32)
    }

    /// 按边字节升序遍历节点 `node` 的全部转移 `(byte, child_node_index)`
    #[inline]
    pub fn children(&self, node: u32) -> TrieChildren<'a> {
        let (first, labels) = self.child_labels(node);
        TrieChildren {
            labels: labels.iter(),
            next_child: first,
        }
    }

    /// 精确命中节点 `node` 的词条 ID 列表（已按 cost 升序排列）
    #[inline]
    pub fn exact_entries(&self, node: u32) -> IdList<'a> {
        Self::id_list(self.exact_start, self.exact_ids, node)
    }

    /// 以节点 `node` 为前缀的 Top-N 高优词条 ID 缓存（仅英文树建立；其余树恒为空）
    #[inline]
    pub fn top_prefix_entries(&self, node: u32) -> IdList<'a> {
        Self::id_list(self.prefix_start, self.prefix_ids, node)
    }

    /// 节点句柄（把节点下标与所属的树绑在一起，便于连续访问同一节点的多个属性）
    #[inline]
    pub fn node(&self, node: u32) -> TrieNodeRef<'a> {
        TrieNodeRef { trie: *self, node }
    }

    /// 从指定节点 `start_node` 沿 `segment` 字节序列前进，若路径不存在则立即返回 `None`（用于晶格图 DFS 零拷贝剪枝）
    #[inline]
    pub fn advance_node(&self, start_node: u32, segment: &str) -> Option<u32> {
        let mut curr = start_node;
        for &b in segment.as_bytes() {
            curr = self.find_child(curr, b)?;
        }
        Some(curr)
    }

    /// 精确查找键对应的词条 ID 列表
    #[inline]
    pub fn exact_lookup(&self, key: &str) -> IdList<'a> {
        match self.advance_node(0, key) {
            Some(node) => self.exact_entries(node),
            None => IdList::default(),
        }
    }

    /// 前缀联想查找（用于英文自动补全如输入 `typesc` -> `TypeScript`）
    #[inline]
    pub fn prefix_lookup(&self, prefix: &str) -> IdList<'a> {
        match self.advance_node(0, prefix) {
            Some(node) => self.top_prefix_entries(node),
            None => IdList::default(),
        }
    }

    /// 单趟扫描，惰性产出 `input` 所有命中的 (前缀长度, 词条 ID 列表)：$O(|input|)$，不分配内存
    #[inline]
    pub fn common_prefix_search<'k>(&self, input: &'k str) -> CommonPrefixSearch<'a, 'k> {
        CommonPrefixSearch {
            trie: *self,
            input: input.as_bytes(),
            pos: 0,
            node: 0,
        }
    }
}

/// 节点句柄（见 [`TrieView::node`]）
#[derive(Debug, Clone, Copy)]
pub struct TrieNodeRef<'a> {
    trie: TrieView<'a>,
    node: u32,
}

impl<'a> TrieNodeRef<'a> {
    /// 节点下标
    #[inline]
    pub fn index(&self) -> u32 {
        self.node
    }

    #[inline]
    pub fn find_child(&self, b: u8) -> Option<u32> {
        self.trie.find_child(self.node, b)
    }

    /// 子节点转移列表 `(byte, child_node_index)`，按 byte 升序
    #[inline]
    pub fn transitions(&self) -> TrieChildren<'a> {
        self.trie.children(self.node)
    }

    #[inline]
    pub fn exact_entries(&self) -> IdList<'a> {
        self.trie.exact_entries(self.node)
    }

    #[inline]
    pub fn top_prefix_entries(&self) -> IdList<'a> {
        self.trie.top_prefix_entries(self.node)
    }
}

/// 节点转移迭代器，产出 `(byte, child_node_index)`
#[derive(Debug, Clone)]
pub struct TrieChildren<'a> {
    labels: std::slice::Iter<'a, u8>,
    next_child: u32,
}

impl Iterator for TrieChildren<'_> {
    type Item = (u8, u32);

    #[inline]
    fn next(&mut self) -> Option<(u8, u32)> {
        let b = *self.labels.next()?;
        let child = self.next_child;
        self.next_child += 1;
        Some((b, child))
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.labels.size_hint()
    }
}

impl ExactSizeIterator for TrieChildren<'_> {}

/// [`TrieView::common_prefix_search`] 的惰性迭代器
#[derive(Debug, Clone)]
pub struct CommonPrefixSearch<'a, 'k> {
    trie: TrieView<'a>,
    input: &'k [u8],
    pos: usize,
    node: u32,
}

impl<'a> Iterator for CommonPrefixSearch<'a, '_> {
    type Item = (usize, IdList<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.input.len() {
            match self.trie.find_child(self.node, self.input[self.pos]) {
                Some(next) => {
                    self.node = next;
                    self.pos += 1;
                    let entries = self.trie.exact_entries(next);
                    if !entries.is_empty() {
                        return Some((self.pos, entries));
                    }
                }
                None => {
                    self.pos = self.input.len();
                    break;
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{legacy, FlatTrieNode, MultiSourceBuilder};
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// 每个测试独占的临时目录（测试结束时自行清理）
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("ime-lexicon-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 种子词库 + 若干刻意构造的边界词条（带声调、同音字、单字、长词、英文前缀族、跨语种 Bigram）
    fn sample_parts() -> LexiconParts {
        let seed = repo_root().join("data/seed");
        let mut builder = MultiSourceBuilder::new();
        builder
            .load_rime_dict_file(seed.join("rime_ice.base.dict.yaml"), 1.0, DomainTag::General)
            .unwrap();
        builder
            .load_english_dict_file(seed.join("melt_eng.dict.yaml"), 1.0)
            .unwrap();
        builder.load_bigram_file(seed.join("wanxiang.bigram.tsv")).unwrap();
        builder.load_rime_dict_str(
            "中国\tzhong1 guo2\t90000\n女儿\tnv3 er2\t5000\n吉\tji\t800\n级\tji\t700\n\
             西安\txi an\t30000\n先\txian\t30000\n一二三四五六七八九十百千\tyi er san si wu liu qi ba jiu shi bai qian\t10\n\
             聊天\tliao tian\t4000\tchat\n矩阵\tju zhen\t3000\tacademic\n",
            1.0,
            DomainTag::General,
        );
        builder.load_english_dict_str("Type\ttype\t900\nTypeScript\ttypescript\t150000\ttech\nTypes\ttypes\t50\nA1\ta1\t10\n", 1.0);
        builder.load_bigram_str("傅里叶\t级数\t5.5\n用\tRust\t2.25\nRust\t写\t1.5\n");
        builder.build_parts()
    }

    fn assert_entry_matches(view: EntryRef<'_>, e: &LexiconEntry) {
        assert_eq!(view.text(), e.text);
        assert_eq!(view.syllable_key(), e.syllable_key);
        assert_eq!(view.syllable_count(), e.pinyin_syllables.len());
        assert!(view.syllables().eq(e.pinyin_syllables.iter().map(|s| s.as_str())), "{e:?}");
        assert_eq!(
            view.syllables().next_back(),
            e.pinyin_syllables.last().map(|s| s.as_str())
        );
        assert_eq!(view.flat_key(), e.flat_key.as_str());
        assert_eq!(view.abbrev_key(), e.abbrev_key);
        assert_eq!(view.cost().to_bits(), e.cost.to_bits());
        assert_eq!(view.raw_weight().to_bits(), e.raw_weight.to_bits());
        assert_eq!(view.is_english(), e.is_english);
        assert_eq!(view.domain(), e.domain);
        assert_eq!(view.tones(), e.tones.as_deref());
        if e.is_english {
            // 热路径依赖：英文词条取 flat_key 不分配
            assert!(matches!(view.flat_key(), Cow::Borrowed(_)));
        }
    }

    /// 视图与堆上前缀树并行 DFS：逐节点比对转移标签、精确命中列表与前缀 Top 列表（含顺序）
    fn assert_trie_matches(view: TrieView<'_>, heap: &FlatTrie) {
        assert_eq!(view.node_count(), heap.nodes.len());
        let mut stack = vec![(0u32, 0u32)];
        let mut visited = 0usize;
        while let Some((v, h)) = stack.pop() {
            visited += 1;
            let hn = &heap.nodes[h as usize];
            let vt: Vec<(u8, u32)> = view.children(v).collect();
            assert_eq!(
                vt.iter().map(|&(b, _)| b).collect::<Vec<_>>(),
                hn.transitions.iter().map(|&(b, _)| b).collect::<Vec<_>>()
            );
            assert_eq!(view.node(v).transitions().len(), hn.transitions.len());
            assert_eq!(view.exact_entries(v).to_vec(), hn.exact_entries);
            assert_eq!(view.node(v).exact_entries().len(), hn.exact_entries.len());
            assert_eq!(view.exact_entries(v).first(), hn.exact_entries.first().copied());
            assert_eq!(view.top_prefix_entries(v).to_vec(), hn.top_prefix_entries);
            for (&(b, vc), &(_, hc)) in vt.iter().zip(&hn.transitions) {
                assert_eq!(view.find_child(v, b), Some(vc));
                assert_eq!(view.node(v).find_child(b), Some(vc));
                stack.push((vc, hc));
            }
            for b in [0u8, b'\'', b'0', b'a', b'm', b'z', 0xff] {
                assert_eq!(view.find_child(v, b).is_some(), hn.find_child(b).is_some());
            }
        }
        assert_eq!(visited, heap.nodes.len());
    }

    /// 对同一个键比对四种查询（及其所有前缀）在视图与堆上前缀树上的结果
    fn assert_queries_match(view: TrieView<'_>, heap: &FlatTrie, key: &str) {
        for end in (0..=key.len()).filter(|&i| key.is_char_boundary(i)) {
            let prefix = &key[..end];
            assert_eq!(view.exact_lookup(prefix).to_vec(), heap.exact_lookup(prefix), "{prefix}");
            assert_eq!(view.prefix_lookup(prefix).to_vec(), heap.prefix_lookup(prefix), "{prefix}");
            assert_eq!(
                view.advance_node(0, prefix).is_some(),
                heap.advance_node(0, prefix).is_some()
            );
        }
        for probe in [key.to_string(), format!("{key}'zz"), format!("{key}x"), format!("q{key}")] {
            let got: Vec<(usize, Vec<u32>)> = view
                .common_prefix_search(&probe)
                .map(|(len, ids)| (len, ids.to_vec()))
                .collect();
            let want: Vec<(usize, Vec<u32>)> = heap
                .common_prefix_search(&probe)
                .into_iter()
                .map(|(len, ids)| (len, ids.to_vec()))
                .collect();
            assert_eq!(got, want, "{probe}");
            assert_eq!(
                view.common_prefix_search(&probe).next().is_none(),
                want.is_empty()
            );
        }
    }

    fn assert_lexicon_matches(lex: &CompiledLexicon, parts: &LexiconParts, with_queries: bool) {
        assert_eq!(lex.entry_count(), parts.entries.len());
        assert_eq!(lex.entries().len(), parts.entries.len());
        for (id, e) in parts.entries.iter().enumerate() {
            let view = lex.entry(id as u32);
            assert_eq!(view.id(), id as u32);
            assert_entry_matches(view, e);
        }
        assert!(lex.get_entry(parts.entries.len() as u32).is_none());

        assert_trie_matches(lex.syllable_trie(), &parts.syllable_trie);
        assert_trie_matches(lex.abbrev_trie(), &parts.abbrev_trie);
        assert_trie_matches(lex.english_trie(), &parts.english_trie);
        if with_queries {
            for e in &parts.entries {
                assert_queries_match(lex.syllable_trie(), &parts.syllable_trie, &e.syllable_key);
                assert_queries_match(lex.abbrev_trie(), &parts.abbrev_trie, &e.abbrev_key);
                assert_queries_match(lex.english_trie(), &parts.english_trie, &e.flat_key);
            }
        }

        assert_eq!(lex.bigram_count(), parts.bigrams.len());
        for (key, &bonus) in &parts.bigrams {
            let (prev, next) = key.split_once('\t').unwrap();
            assert_eq!(lex.bigram_bonus(prev, next).to_bits(), bonus.to_bits(), "{key}");
        }
        let listed: HashMap<String, f32> = lex.bigram_entries().map(|(k, v)| (k.to_string(), v)).collect();
        assert_eq!(listed.len(), parts.bigrams.len());
        for (k, v) in &listed {
            assert_eq!(parts.bigrams.get(k).map(|b| b.to_bits()), Some(v.to_bits()));
        }
    }

    /// 改动正文后重新盖章（重算正文校验和与文件头校验和），用来绕过校验和、直接考验结构校验与访问器
    fn reseal(bytes: &mut [u8]) {
        let sum = content_checksum(&bytes[HEADER_LEN..]);
        bytes[48..56].copy_from_slice(&sum.to_le_bytes());
        let header_sum = fnv1a32(&[&bytes[..60], &bytes[HEADER_FIXED_LEN..HEADER_LEN]]);
        bytes[60..64].copy_from_slice(&header_sum.to_le_bytes());
    }

    /// 在可能已损坏的词库上把全部访问器扫一遍（只做有界查询：损坏的 Trie 可能成环，不能做无界遍历）
    fn sweep_accessors(lex: &CompiledLexicon, parts: &LexiconParts) -> usize {
        let mut touched = 0usize;
        for e in lex.entries() {
            touched += e.text().len() + e.syllable_key().len() + e.syllables().count();
            touched += e.flat_key().len() + e.abbrev_key().len() + e.tones().map_or(0, |t| t.len());
            touched += (e.cost().is_nan() as usize) + (e.raw_weight().is_nan() as usize);
            touched += e.is_english() as usize + e.domain() as usize;
        }
        for (trie, keys) in [
            (lex.syllable_trie(), parts.entries.iter().map(|e| &e.syllable_key).collect::<Vec<_>>()),
            (lex.abbrev_trie(), parts.entries.iter().map(|e| &e.abbrev_key).collect()),
            (lex.english_trie(), parts.entries.iter().map(|e| &e.flat_key).collect()),
        ] {
            for key in keys {
                for id in trie.exact_lookup(key).iter().chain(trie.prefix_lookup(key).iter()) {
                    touched += lex.get_entry(id).map_or(0, |e| e.text().len());
                }
                for (len, ids) in trie.common_prefix_search(key) {
                    touched += len + ids.len();
                }
            }
            // 有界深度的结构遍历
            let mut stack = vec![(0u32, 0u8)];
            let mut budget = 20_000usize;
            while let Some((node, depth)) = stack.pop() {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                touched += trie.exact_entries(node).len() + trie.top_prefix_entries(node).len();
                if depth < 3 {
                    for (_, child) in trie.children(node) {
                        stack.push((child, depth + 1));
                    }
                }
            }
        }
        for key in parts.bigrams.keys() {
            let (prev, next) = key.split_once('\t').unwrap();
            touched += lex.bigram_bonus(prev, next).is_nan() as usize;
            touched += lex.bigram_bonus(next, prev).is_nan() as usize;
        }
        touched += lex.bigram_entries().count();
        touched
    }

    #[test]
    fn test_roundtrip_build_save_load_matches_in_memory_build() {
        let parts = sample_parts();
        assert!(parts.entries.len() > 500);
        assert!(parts.entries.iter().any(|e| e.tones.is_some()));
        assert!(parts.english_trie.nodes.iter().any(|n| !n.top_prefix_entries.is_empty()));

        // 内存构建（自有 Vec<u8>，不落盘）
        let built = CompiledLexicon::from_parts(&parts).unwrap();
        assert!(!built.is_mmap());
        built.verify().unwrap();
        assert_lexicon_matches(&built, &parts, true);

        // 落盘再以 mmap 加载
        let dir = TempDir::new("roundtrip");
        let path = dir.0.join("nested/dir/merged_lexicon.bin");
        built.save_to_file(&path).unwrap();
        let loaded = CompiledLexicon::load_mmap(&path).unwrap();
        assert!(loaded.is_mmap());
        assert_eq!(loaded.as_bytes(), built.as_bytes());
        assert_eq!(&loaded.as_bytes()[..8], LEXICON_MAGIC);
        loaded.verify().unwrap();
        assert_lexicon_matches(&loaded, &parts, true);

        // 逐条词条再与内存构建的视图互相比对
        for (a, b) in built.entries().zip(loaded.entries()) {
            assert_entry_matches(a, &b.to_entry());
        }

        // 覆盖写入同一路径（临时文件 + 改名）后目录里不应留下临时文件；已有的映射仍然可读
        built.save_to_file(&path).unwrap();
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("merged_lexicon.bin")]);
        assert_lexicon_matches(&loaded, &parts, false);

        // clone 共享同一份底层存储
        let shared = loaded.clone();
        assert_eq!(shared.as_bytes().as_ptr(), loaded.as_bytes().as_ptr());

        // 同一份输入编码结果确定（Bigram 记录按键排序，不受 HashMap 遍历顺序影响）
        assert_eq!(encode(&parts.clone()).unwrap(), built.as_bytes());
    }

    #[test]
    fn test_empty_lexicon_roundtrip() {
        let lex = MultiSourceBuilder::new().build();
        lex.verify().unwrap();
        assert_eq!(lex.entry_count(), 0);
        assert_eq!(lex.bigram_count(), 0);
        assert_eq!(lex.syllable_trie().node_count(), 1);
        assert!(lex.syllable_trie().exact_lookup("ni").is_empty());
        assert!(lex.english_trie().prefix_lookup("").is_empty());
        assert!(lex.abbrev_trie().common_prefix_search("abc").next().is_none());
        assert_eq!(lex.bigram_bonus("你", "好"), 0.0);
        let again = CompiledLexicon::from_bytes(lex.as_bytes().to_vec()).unwrap();
        assert_eq!(again.entry_count(), 0);
    }

    #[test]
    fn test_rejects_truncated_and_resized_files() {
        let parts = sample_parts();
        let bytes = encode(&parts).unwrap();
        let dir = TempDir::new("truncated");
        let path = dir.0.join("lex.bin");

        for cut in [0usize, 1, 7, 8, 63, 64, HEADER_LEN - 1, HEADER_LEN, bytes.len() / 2, bytes.len() - 1] {
            std::fs::write(&path, &bytes[..cut]).unwrap();
            let err = CompiledLexicon::load_mmap(&path).expect_err("truncated file must be rejected");
            if cut >= HEADER_LEN {
                assert!(
                    matches!(err, LexiconError::SizeMismatch { expected, actual }
                        if expected == bytes.len() as u64 && actual == cut as u64),
                    "cut {cut}: {err}"
                );
            }
            assert!(CompiledLexicon::from_bytes(bytes[..cut].to_vec()).is_err());
        }

        // 末尾多出字节同样拒绝
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(
            CompiledLexicon::from_bytes(longer),
            Err(LexiconError::SizeMismatch { .. })
        ));

        // 不存在的文件是 IO 错误
        assert!(matches!(
            CompiledLexicon::load_mmap(dir.0.join("missing.bin")),
            Err(LexiconError::Io(_))
        ));
    }

    #[test]
    fn test_rejects_any_corrupted_header_byte() {
        let bytes = encode(&sample_parts()).unwrap();
        // 文件头与分区表的每一个字节都受魔数或文件头校验和保护：逐字节翻转必须全部被拒绝
        for pos in 0..HEADER_LEN {
            for flip in [0x01u8, 0x80, 0xff] {
                let mut bad = bytes.clone();
                bad[pos] ^= flip;
                assert!(
                    CompiledLexicon::from_bytes(bad).is_err(),
                    "header byte {pos} ^ {flip:#x} was accepted"
                );
            }
        }
    }

    #[test]
    fn test_rejects_inconsistent_section_table_even_with_valid_checksum() {
        let bytes = encode(&sample_parts()).unwrap();
        let section_at = |idx: usize| HEADER_FIXED_LEN + idx * SECTION_ENTRY_LEN;
        let cases: Vec<(&str, Box<dyn Fn(&mut Vec<u8>)>)> = vec![
            ("分区越过文件末尾", Box::new(move |b| {
                let at = section_at(SEC_POOL) + 8;
                let len = le_u64(&b[at..at + 8]) + 1_000_000_000;
                b[at..at + 8].copy_from_slice(&len.to_le_bytes());
            })),
            ("分区偏移溢出", Box::new(move |b| {
                let at = section_at(SEC_ENTRY_COLD);
                b[at..at + 8].copy_from_slice(&(u64::MAX - 7).to_le_bytes());
            })),
            ("分区落在文件头内", Box::new(move |b| {
                let at = section_at(SEC_BIGRAM_RECORDS);
                b[at..at + 8].copy_from_slice(&8u64.to_le_bytes());
            })),
            ("词条数与热区长度不符", Box::new(|b| {
                let n = le_u32(&b[24..28]) + 1;
                b[24..28].copy_from_slice(&n.to_le_bytes());
            })),
            ("节点数与标签分区长度不符", Box::new(|b| {
                let n = le_u32(&b[32..36]) - 1;
                b[32..36].copy_from_slice(&n.to_le_bytes());
            })),
            ("节点数为零", Box::new(|b| b[36..40].copy_from_slice(&0u32.to_le_bytes()))),
            ("哈希槽数量不是 2 的幂", Box::new(|b| {
                let n = le_u32(&b[44..48]) + 1;
                b[44..48].copy_from_slice(&n.to_le_bytes());
            })),
            ("分区数量不符", Box::new(|b| b[12..16].copy_from_slice(&22u32.to_le_bytes()))),
            ("保留标志位非零", Box::new(|b| b[56..60].copy_from_slice(&1u32.to_le_bytes()))),
        ];
        for (name, mutate) in cases {
            let mut bad = bytes.clone();
            mutate(&mut bad);
            reseal(&mut bad);
            assert!(CompiledLexicon::from_bytes(bad).is_err(), "{name}");
        }
    }

    #[test]
    fn test_rejects_old_and_unknown_magic() {
        let parts = sample_parts();
        let dir = TempDir::new("magic");

        // 真正的旧版 AIMELX02 镜像：load_mmap 给出明确的“格式过旧”错误
        let v2 = legacy::write_v2(&parts);
        let v2_path = dir.0.join("old.bin");
        std::fs::write(&v2_path, &v2).unwrap();
        let err = CompiledLexicon::load_mmap(&v2_path).unwrap_err();
        assert!(matches!(&err, LexiconError::OutdatedFormat(found) if found == "AIMELX02"), "{err}");
        assert!(err.to_string().contains("AIMELX02") && err.to_string().contains("AIMELX03"));
        assert!(matches!(
            CompiledLexicon::from_bytes(b"AIMELX02".to_vec()),
            Err(LexiconError::OutdatedFormat(_))
        ));

        // 未知魔数 / 空文件
        for junk in [&b"AIMELX01 some old data......."[..], b"AIMELX04", b"not a lexicon at all", b"AIM", b""] {
            let path = dir.0.join("junk.bin");
            std::fs::write(&path, junk).unwrap();
            assert!(matches!(
                CompiledLexicon::load_mmap(&path),
                Err(LexiconError::InvalidMagic)
            ));
        }

        // 迁移通道：旧镜像 -> 堆上结构 -> AIMELX03，与直接编码逐字节一致
        let migrated = LexiconParts::load_legacy_v2(&v2_path).unwrap();
        let lex = CompiledLexicon::from_parts(&migrated).unwrap();
        assert_eq!(lex.as_bytes(), encode(&parts).unwrap());
        assert_lexicon_matches(&lex, &parts, false);
        assert!(matches!(
            legacy::read_v2(&v2[..v2.len() / 2]),
            Err(LexiconError::CorruptedBinary(_))
        ));
        assert!(matches!(legacy::read_v2(lex.as_bytes()), Err(LexiconError::InvalidMagic)));
    }

    #[test]
    fn test_load_or_build_treats_old_image_as_missing_without_clobbering_it() {
        let dir = TempDir::new("fallback");
        let bin = dir.0.join("data/compiled/merged_lexicon.bin");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let v2 = legacy::write_v2(&sample_parts());
        std::fs::write(&bin, &v2).unwrap();
        std::fs::create_dir_all(dir.0.join("data/seed")).unwrap();
        std::fs::write(
            dir.0.join("data/seed/rime_ice.base.dict.yaml"),
            "你好\tni hao\t1000\n",
        )
        .unwrap();

        // 只在临时目录里查找：公开入口 `load_or_build_default` 还会查找可执行文件目录、当前目录与
        // %LOCALAPPDATA%\RustAiPinyinIME，本机若装有输入法就会加载到那份真实词库，测试结果因机器而异
        let roots = [dir.0.clone()];
        let load = || CompiledLexicon::load_or_build_from(&roots, &dir.0).unwrap();

        // 旧镜像视同不存在：回退到种子词库现场编译，但不覆盖那份旧镜像
        let lex = load();
        assert_eq!(lex.entry_count(), 1);
        assert_eq!(lex.entry(0).text(), "你好");
        assert_eq!(std::fs::read(&bin).unwrap(), v2);

        // 真的没有镜像时：编译并回写 AIMELX03，下次直接 mmap 加载
        std::fs::remove_file(&bin).unwrap();
        let built = load();
        assert!(!built.is_mmap());
        let reloaded = load();
        assert!(reloaded.is_mmap());
        assert_eq!(reloaded.as_bytes(), built.as_bytes());

        // 损坏（被截断）的镜像同样视同不存在，并被重新编译的结果替换
        let good = std::fs::read(&bin).unwrap();
        drop(reloaded);
        std::fs::write(&bin, &good[..good.len() - 3]).unwrap();
        let rebuilt = load();
        assert_eq!(rebuilt.entry(0).text(), "你好");
        assert_eq!(std::fs::read(&bin).unwrap(), good);

        // 候选根目录按顺序查找：前一个目录里没有镜像时使用后一个目录里的，且不向回写目录写任何东西
        let empty = TempDir::new("fallback-empty");
        let second = CompiledLexicon::load_or_build_from(&[empty.0.clone(), dir.0.clone()], &empty.0).unwrap();
        assert!(second.is_mmap());
        assert_eq!(second.as_bytes(), good.as_slice());
        assert!(!empty.0.join("data").exists());
    }

    #[test]
    fn test_public_load_or_build_default_loads_image_under_given_root() {
        // 公开入口把传入的根目录排在候选列表首位：那里有可用镜像时必定加载它（与本机其他位置无关）
        let dir = TempDir::new("default-root");
        let built = CompiledLexicon::from_parts(&sample_parts()).unwrap();
        built.save_to_file(dir.0.join("data/compiled/merged_lexicon.bin")).unwrap();
        let lex = CompiledLexicon::load_or_build_default(&dir.0).unwrap();
        assert!(lex.is_mmap());
        assert_eq!(lex.as_bytes(), built.as_bytes());
    }

    #[test]
    fn test_builder_skips_overlong_lines_so_build_cannot_fail() {
        let long_text = "长".repeat(30_000); // 90,000 字节
        let long_syllable = "a".repeat(70_000);
        let long_english_key = "k".repeat(70_000);
        let mut builder = MultiSourceBuilder::new();
        builder.load_rime_dict_str(
            &format!("{long_text}\tchang\t100\n超长音节\t{long_syllable}\t100\n正常\tzheng chang\t100\n"),
            1.0,
            DomainTag::General,
        );
        builder.load_english_dict_str(&format!("Key\t{long_english_key}\t100\nOk\tok\t100\n"), 1.0);
        // 此前这里会 panic（更早的 AIMELX02 写盘则会把长度截断成 u16，产出损坏的文件）
        let lex = builder.build();
        lex.verify().unwrap();
        let mut texts: Vec<&str> = lex.entries().map(|e| e.text()).collect();
        texts.sort_unstable();
        assert_eq!(texts, ["Ok", "正常"]);

        // 恰好 65535 字节的文本仍然收录
        let max_text = "x".repeat(u16::MAX as usize);
        let mut builder = MultiSourceBuilder::new();
        builder.load_rime_dict_str(&format!("{max_text}\tx\t100\n"), 1.0, DomainTag::General);
        let lex = builder.build();
        lex.verify().unwrap();
        assert_eq!(lex.entry_count(), 1);
        assert_eq!(lex.entry(0).text().len(), u16::MAX as usize);
    }

    #[test]
    fn test_replace_failure_on_mapped_target_is_reported_as_in_use() {
        use std::io::{Error, ErrorKind};
        let path = std::path::Path::new("data/compiled/merged_lexicon.bin");
        // Windows 上目标词库被映射时，改名覆盖得到的三种错误码
        for code in [5, 32, 1224] {
            let err = crate::replace_error(Error::from_raw_os_error(code), path, true);
            assert!(matches!(&err, LexiconError::TargetInUse { path: p, .. } if p == path), "{code}: {err}");
            assert!(err.to_string().contains("merged_lexicon.bin"));
        }
        assert!(matches!(
            crate::replace_error(Error::from(ErrorKind::PermissionDenied), path, true),
            LexiconError::TargetInUse { .. }
        ));
        // 其他平台 / 目标不存在 / 其他错误：保持普通 IO 错误
        assert!(matches!(
            crate::replace_error(Error::from_raw_os_error(5), path, false),
            LexiconError::Io(_)
        ));
        assert!(matches!(
            crate::replace_error(Error::from(ErrorKind::NotFound), path, true),
            LexiconError::Io(_)
        ));
    }

    #[test]
    fn test_corrupted_body_is_detected_by_verify_and_never_misbehaves() {
        let parts = sample_parts();
        let bytes = encode(&parts).unwrap();
        let clean = CompiledLexicon::from_bytes(bytes.clone()).unwrap();
        let clean_sum = sweep_accessors(&clean, &parts);
        assert!(clean_sum > 0);

        // 确定性的 xorshift 伪随机数
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let body_len = bytes.len() - HEADER_LEN;
        let mut structural_rejections = 0usize;
        for round in 0..400 {
            let mut bad = bytes.clone();
            // 奇数轮改 1 个字节，偶数轮连续改一小段
            let pos = HEADER_LEN + (next() as usize % body_len);
            let span = if round % 2 == 0 { 1 + next() as usize % 8 } else { 1 };
            for b in bad.iter_mut().skip(pos).take(span) {
                *b ^= 1 + (next() % 255) as u8;
            }

            // 1) 不重新盖章：文件头仍然合法所以能加载，但 verify 必定发现正文校验和不符
            let lex = CompiledLexicon::from_bytes(bad.clone()).unwrap();
            assert!(matches!(lex.verify(), Err(LexiconError::CorruptedBinary(48))), "pos {pos}");

            // 2) 重新盖章绕过校验和：访问器只允许返回（可能是垃圾的）结果或安全 panic，不允许挂死
            reseal(&mut bad);
            let lex = CompiledLexicon::from_bytes(bad).unwrap();
            if lex.verify().is_err() {
                structural_rejections += 1;
            }
            // 直接调用而不包 catch_unwind：任何访问器在损坏数据上 panic 都要让测试失败
            sweep_accessors(&lex, &parts);
        }
        // 结构校验本身确实在起作用（而不是全靠校验和）
        assert!(structural_rejections > 50, "only {structural_rejections} structural rejections");
    }

    #[test]
    fn test_verify_catches_structural_damage_behind_a_valid_checksum() {
        let parts = sample_parts();
        let bytes = encode(&parts).unwrap();
        let layout = Layout::parse(&bytes).unwrap();
        let syl = &layout.tries[0];
        let cases: Vec<(&str, usize, Vec<u8>)> = vec![
            ("词条文本偏移越界", layout.entries_hot.start, u32::MAX.to_le_bytes().to_vec()),
            ("词条文本不是 UTF-8", layout.pool.start + le_u32(&bytes[layout.entries_hot.start..]) as usize, vec![0xff]),
            ("词条 cost 为 NaN", layout.entries_hot.start + 8, f32::NAN.to_le_bytes().to_vec()),
            ("词条 cost 为无穷大", layout.entries_hot.start + ENTRY_HOT_LEN + 8, f32::INFINITY.to_le_bytes().to_vec()),
            ("词条下标越界", syl.exact_ids.start, u32::MAX.to_le_bytes().to_vec()),
            ("子节点范围倒挂", syl.child_start.start + 4, 0u32.to_le_bytes().to_vec()),
            ("子节点范围越过节点数", syl.child_start.end - 4, u32::MAX.to_le_bytes().to_vec()),
            ("精确命中范围越界", syl.exact_start.end - 4, u32::MAX.to_le_bytes().to_vec()),
            ("Bigram 键偏移越界", layout.bigram_records.start, (u32::MAX - 1).to_le_bytes().to_vec()),
            ("Bigram 哈希槽被清空", layout.bigram_slots.start, vec![0u8; layout.bigram_slots.len()]),
        ];
        for (name, at, patch) in cases {
            let mut bad = bytes.clone();
            bad[at..at + patch.len()].copy_from_slice(&patch);
            reseal(&mut bad);
            let lex = CompiledLexicon::from_bytes(bad).unwrap();
            assert!(lex.verify().is_err(), "{name}");
            // 不包 catch_unwind：访问器在结构损坏的数据上也不允许 panic
            sweep_accessors(&lex, &parts);
        }

        // 同一父节点下标签乱序
        let mut bad = bytes.clone();
        let labels = &mut bad[syl.labels.clone()];
        let root_children = le_u32(&bytes[syl.child_start.start + 4..]) as usize - 1;
        assert!(root_children >= 2);
        labels.swap(1, 2);
        reseal(&mut bad);
        assert!(CompiledLexicon::from_bytes(bad).unwrap().verify().is_err());

        // 子节点范围指向节点自身（成环，且根节点下的节点不可达）：单词条词库 "啊 a" 的音节 Trie 只有根与 `a` 两个节点，
        // child_start 为 [1, 2, 2]；改成 [1, 1, 2] 后范围依旧单调并铺满 [1, 2)，但节点 1 成了自己的子节点
        let mut builder = MultiSourceBuilder::new();
        builder.load_rime_dict_str("啊\ta\t100\n", 1.0, DomainTag::General);
        let tiny = builder.build();
        tiny.verify().unwrap();
        let tiny_syl = tiny.layout.tries[0].clone();
        let starts: Vec<u32> = tiny.sec(&tiny_syl.child_start).chunks_exact(4).map(le_u32).collect();
        assert_eq!(starts, [1, 2, 2]);
        let mut bad = tiny.as_bytes().to_vec();
        bad[tiny_syl.child_start.start + 4..tiny_syl.child_start.start + 8].copy_from_slice(&1u32.to_le_bytes());
        reseal(&mut bad);
        let cyclic = CompiledLexicon::from_bytes(bad).unwrap();
        assert!(cyclic.verify().is_err());
        // 访问器在成环的 Trie 上依然有界：沿环前进的步数只取决于输入长度
        assert!(cyclic.syllable_trie().exact_lookup("a").is_empty());
        assert_eq!(cyclic.syllable_trie().advance_node(1, "aaaaaaaa"), Some(1));
    }

    #[test]
    fn test_bigram_lookups() {
        let mut parts = sample_parts();
        parts.bigrams.clear();
        // 足够多的键以覆盖哈希槽的线性探测与指纹比对
        for i in 0..5000u32 {
            parts.bigrams.insert(format!("前词{i}\t后词{}", i * 7 % 13), i as f32 * 0.5 - 100.0);
        }
        parts.bigrams.insert("傅里叶\t级数".to_string(), 5.5);
        parts.bigrams.insert("用\tRust".to_string(), 2.25);
        parts.bigrams.insert("Rust\t写".to_string(), 1.5);
        parts.bigrams.insert("a\tb".to_string(), -0.75);
        parts.bigrams.insert("空后词\t".to_string(), 3.0);
        let lex = CompiledLexicon::from_parts(&parts).unwrap();
        lex.verify().unwrap();
        assert_eq!(lex.bigram_count(), 5005);

        // 命中：中文-中文、中文-英文、英文-中文、负数 bonus、空的后词
        assert_eq!(lex.bigram_bonus("傅里叶", "级数"), 5.5);
        assert_eq!(lex.bigram_bonus("用", "Rust"), 2.25);
        assert_eq!(lex.bigram_bonus("Rust", "写"), 1.5);
        assert_eq!(lex.bigram_bonus("a", "b"), -0.75);
        assert_eq!(lex.bigram_bonus("空后词", ""), 3.0);
        for (key, &bonus) in &parts.bigrams {
            let (prev, next) = key.split_once('\t').unwrap();
            assert_eq!(lex.bigram_bonus(prev, next).to_bits(), bonus.to_bits(), "{key}");
        }

        // 未命中：方向相反、前词为空、前后缀相近、大小写不同、不存在的词
        assert_eq!(lex.bigram_bonus("级数", "傅里叶"), 0.0);
        assert_eq!(lex.bigram_bonus("", "级数"), 0.0);
        assert_eq!(lex.bigram_bonus("", ""), 0.0);
        assert_eq!(lex.bigram_bonus("傅里", "叶级数"), 0.0);
        assert_eq!(lex.bigram_bonus("傅里叶", "级"), 0.0);
        assert_eq!(lex.bigram_bonus("傅里叶级", "数"), 0.0);
        assert_eq!(lex.bigram_bonus("用", "rust"), 0.0);
        assert_eq!(lex.bigram_bonus("前词1", "后词8"), 0.0);
        assert_eq!(lex.bigram_bonus("不存在", "的词"), 0.0);
        // 与此前 `HashMap<"prev\tnext">` 的语义一致：比较的是拼接后的整串
        assert_eq!(lex.bigram_bonus("a\tb", ""), 0.0);
        assert_eq!(lex.bigram_bonus("a", "b\t"), 0.0);
        for i in 0..5000u32 {
            assert_eq!(lex.bigram_bonus(&format!("前词{i}"), "后词99"), 0.0);
        }

        // 遍历结果按键字节序排列且与输入一致
        let listed: Vec<(&str, f32)> = lex.bigram_entries().collect();
        assert_eq!(listed.len(), 5005);
        assert!(listed.windows(2).all(|w| w[0].0.as_bytes() < w[1].0.as_bytes()));

        // 空表
        parts.bigrams.clear();
        let empty = CompiledLexicon::from_parts(&parts).unwrap();
        empty.verify().unwrap();
        assert_eq!(empty.bigram_count(), 0);
        assert_eq!(empty.bigram_bonus("傅里叶", "级数"), 0.0);
        assert_eq!(empty.bigram_entries().count(), 0);
    }

    #[test]
    fn test_from_parts_rejects_entries_whose_derived_fields_disagree() {
        let base = sample_parts();
        let cases: Vec<(&str, Box<dyn Fn(&mut LexiconParts)>)> = vec![
            ("flat_key 与音节不符", Box::new(|p| {
                let e = p.entries.iter_mut().find(|e| e.pinyin_syllables.len() >= 2).unwrap();
                e.flat_key.push('x');
            })),
            ("abbrev_key 与音节不符", Box::new(|p| {
                let e = p.entries.iter_mut().find(|e| e.pinyin_syllables.len() >= 2).unwrap();
                e.abbrev_key = "zz".to_string();
            })),
            ("syllable_key 与音节不符", Box::new(|p| {
                let e = p.entries.iter_mut().find(|e| e.pinyin_syllables.len() >= 2).unwrap();
                e.pinyin_syllables.pop();
            })),
            ("英文词条带简拼键", Box::new(|p| {
                let e = p.entries.iter_mut().find(|e| e.is_english).unwrap();
                e.abbrev_key = "t".to_string();
            })),
        ];
        for (name, mutate) in cases {
            let mut parts = base.clone();
            mutate(&mut parts);
            assert!(
                matches!(CompiledLexicon::from_parts(&parts), Err(LexiconError::UnencodableEntry { .. })),
                "{name}"
            );
        }

        // 前缀树引用了不存在的词条 / 不是一棵树
        let mut parts = base.clone();
        parts.syllable_trie.nodes[0].exact_entries.push(u32::MAX);
        assert!(matches!(CompiledLexicon::from_parts(&parts), Err(LexiconError::Unencodable(_))));
        let mut parts = base.clone();
        parts.abbrev_trie.nodes.push(FlatTrieNode {
            transitions: vec![(b'a', 0)],
            exact_entries: Vec::new(),
            top_prefix_entries: Vec::new(),
        });
        let last = parts.abbrev_trie.nodes.len() as u32 - 1;
        parts.abbrev_trie.nodes[0].transitions.insert(0, (0, last));
        assert!(matches!(CompiledLexicon::from_parts(&parts), Err(LexiconError::Unencodable(_))));
    }

    /// 全量词库迁移校验（默认忽略）：把旧版 AIMELX02 镜像与迁移出的 AIMELX03 镜像逐词条、逐 Trie 节点比对。
    /// 用法：`IME_LEGACY_V2_LEXICON=<旧镜像> cargo test --release -p ime-lexicon -- --ignored full_lexicon`
    #[test]
    #[ignore]
    fn test_full_lexicon_matches_legacy_v2_image() {
        // 显式运行本测试却没给旧镜像时必须失败，不能悄悄“通过”
        let v2_path = std::env::var_os("IME_LEGACY_V2_LEXICON")
            .expect("请把 IME_LEGACY_V2_LEXICON 设为旧版 AIMELX02 镜像的路径后再运行本测试");
        let parts = LexiconParts::load_legacy_v2(v2_path).unwrap();
        let lex = CompiledLexicon::load_mmap(repo_root().join("data/compiled/merged_lexicon.bin")).unwrap();
        lex.verify().unwrap();
        assert_lexicon_matches(&lex, &parts, true);
        assert_eq!(lex.as_bytes(), encode(&parts).unwrap());
    }
}
