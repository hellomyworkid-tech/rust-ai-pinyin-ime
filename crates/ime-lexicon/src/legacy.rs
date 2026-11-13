//! `legacy`: 旧版 `AIMELX02` 镜像的只读解析器
//!
//! `AIMELX02` 是变长记录的流式格式，加载时必须把每个词条、每个 Trie 节点逐个反序列化到堆上
//! （全量词库约 425 MiB）。运行期已不再支持它；这里只保留解析能力，供
//! `lexicon-builder --migrate-v2` 把已有镜像原样（词条顺序、代价、前缀树内容都不变）转成 `AIMELX03`。

use crate::format::LEGACY_MAGIC_V2;
use crate::{DomainTag, FlatTrie, FlatTrieNode, LexiconEntry, LexiconError, LexiconParts};
use std::collections::HashMap;

/// 解析 `AIMELX02` 字节镜像
pub(crate) fn read_v2(buf: &[u8]) -> Result<LexiconParts, LexiconError> {
    if buf.len() < 8 || &buf[..8] != LEGACY_MAGIC_V2 {
        return Err(LexiconError::InvalidMagic);
    }
    let mut cursor = BinCursor { buf, pos: 8 };

    let entry_count = cursor.read_u32()? as usize;
    let mut entries = Vec::with_capacity(entry_count.min(buf.len() / 16));
    for _ in 0..entry_count {
        let text = cursor.read_str()?;
        let syl_len = cursor.read_u8()? as usize;
        let mut pinyin_syllables = Vec::with_capacity(syl_len);
        for _ in 0..syl_len {
            pinyin_syllables.push(cursor.read_str()?);
        }
        let syllable_key = cursor.read_str()?;
        let flat_key = cursor.read_str()?;
        let abbrev_key = cursor.read_str()?;
        let cost = cursor.read_f32()?;
        let raw_weight = cursor.read_f64()?;
        let is_english = cursor.read_u8()? != 0;
        let domain = DomainTag::from_u8(cursor.read_u8()?);
        let tone_len = cursor.read_u8()? as usize;
        let tones = if tone_len > 0 {
            Some(cursor.read_bytes(tone_len)?.to_vec())
        } else {
            None
        };
        entries.push(LexiconEntry {
            text,
            pinyin_syllables,
            syllable_key,
            flat_key,
            abbrev_key,
            cost,
            raw_weight,
            is_english,
            domain,
            tones,
        });
    }

    let syllable_trie = cursor.read_trie()?;
    let abbrev_trie = cursor.read_trie()?;
    let english_trie = cursor.read_trie()?;

    let bg_count = cursor.read_u32()? as usize;
    let mut bigrams = HashMap::with_capacity(bg_count.min(buf.len() / 6));
    for _ in 0..bg_count {
        let k = cursor.read_str()?;
        let v = cursor.read_f32()?;
        bigrams.insert(k, v);
    }

    Ok(LexiconParts {
        entries,
        syllable_trie,
        abbrev_trie,
        english_trie,
        bigrams,
    })
}

struct BinCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BinCursor<'a> {
    #[inline]
    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], LexiconError> {
        if self.pos + len > self.buf.len() {
            return Err(LexiconError::CorruptedBinary(self.pos));
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    #[inline]
    fn read_u8(&mut self) -> Result<u8, LexiconError> {
        Ok(self.read_bytes(1)?[0])
    }

    #[inline]
    fn read_u16(&mut self) -> Result<u16, LexiconError> {
        let b = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    #[inline]
    fn read_u32(&mut self) -> Result<u32, LexiconError> {
        let b = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    #[inline]
    fn read_f32(&mut self) -> Result<f32, LexiconError> {
        let b = self.read_bytes(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    #[inline]
    fn read_f64(&mut self) -> Result<f64, LexiconError> {
        let b = self.read_bytes(8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    #[inline]
    fn read_str(&mut self) -> Result<String, LexiconError> {
        let len = self.read_u16()? as usize;
        let b = self.read_bytes(len)?;
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| LexiconError::CorruptedBinary(self.pos))
    }

    fn read_trie(&mut self) -> Result<FlatTrie, LexiconError> {
        let node_count = self.read_u32()? as usize;
        let mut nodes = Vec::with_capacity(node_count.min(self.buf.len() / 3));
        for _ in 0..node_count {
            let t_len = self.read_u8()? as usize;
            let mut transitions = Vec::with_capacity(t_len);
            for _ in 0..t_len {
                let ch = self.read_u8()?;
                let child = self.read_u32()?;
                transitions.push((ch, child));
            }
            let e_len = self.read_u8()? as usize;
            let mut exact_entries = Vec::with_capacity(e_len);
            for _ in 0..e_len {
                exact_entries.push(self.read_u32()?);
            }
            let p_len = self.read_u8()? as usize;
            let mut top_prefix_entries = Vec::with_capacity(p_len);
            for _ in 0..p_len {
                top_prefix_entries.push(self.read_u32()?);
            }
            nodes.push(FlatTrieNode {
                transitions,
                exact_entries,
                top_prefix_entries,
            });
        }
        Ok(FlatTrie { nodes })
    }
}

/// 旧版写出逻辑，仅供测试构造 `AIMELX02` 样本
#[cfg(test)]
pub(crate) fn write_v2(parts: &LexiconParts) -> Vec<u8> {
    fn put_str(w: &mut Vec<u8>, s: &str) {
        w.extend_from_slice(&(s.len() as u16).to_le_bytes());
        w.extend_from_slice(s.as_bytes());
    }
    fn put_trie(w: &mut Vec<u8>, trie: &FlatTrie) {
        w.extend_from_slice(&(trie.nodes.len() as u32).to_le_bytes());
        for node in &trie.nodes {
            w.push(node.transitions.len() as u8);
            for &(ch, child) in &node.transitions {
                w.push(ch);
                w.extend_from_slice(&child.to_le_bytes());
            }
            w.push(node.exact_entries.len().min(255) as u8);
            for &id in node.exact_entries.iter().take(255) {
                w.extend_from_slice(&id.to_le_bytes());
            }
            w.push(node.top_prefix_entries.len().min(255) as u8);
            for &id in node.top_prefix_entries.iter().take(255) {
                w.extend_from_slice(&id.to_le_bytes());
            }
        }
    }

    let mut w = Vec::new();
    w.extend_from_slice(LEGACY_MAGIC_V2);
    w.extend_from_slice(&(parts.entries.len() as u32).to_le_bytes());
    for e in &parts.entries {
        put_str(&mut w, &e.text);
        w.push(e.pinyin_syllables.len() as u8);
        for s in &e.pinyin_syllables {
            put_str(&mut w, s);
        }
        put_str(&mut w, &e.syllable_key);
        put_str(&mut w, &e.flat_key);
        put_str(&mut w, &e.abbrev_key);
        w.extend_from_slice(&e.cost.to_le_bytes());
        w.extend_from_slice(&e.raw_weight.to_le_bytes());
        w.push(if e.is_english { 1 } else { 0 });
        w.push(e.domain as u8);
        match &e.tones {
            Some(t) => {
                w.push(t.len() as u8);
                w.extend_from_slice(t);
            }
            None => w.push(0),
        }
    }
    put_trie(&mut w, &parts.syllable_trie);
    put_trie(&mut w, &parts.abbrev_trie);
    put_trie(&mut w, &parts.english_trie);
    w.extend_from_slice(&(parts.bigrams.len() as u32).to_le_bytes());
    for (k, &v) in &parts.bigrams {
        put_str(&mut w, k);
        w.extend_from_slice(&v.to_le_bytes());
    }
    w
}
