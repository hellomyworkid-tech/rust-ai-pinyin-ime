//! `lexicon-builder`: 词库融合编译 CLI 工具 (支持 AIMELX02 紧凑二进制格式与十万级开源词库融合)

use ime_lexicon::{CompiledLexicon, DomainTag, MultiSourceBuilder};
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    println!("=== [AI Pinyin IME] 多源词库融合编译器 v2 (AIMELX02 Binary Format) ===");
    let start_build = Instant::now();

    let mut builder = MultiSourceBuilder::new();
    let seed_dir = root.join("data/seed");

    let rime_ice_base = seed_dir.join("rime_ice.base.dict.yaml");
    let melt_eng = seed_dir.join("melt_eng.dict.yaml");
    let wanxiang_bigram = seed_dir.join("wanxiang.bigram.tsv");

    println!("1. 正在加载高优精校核心种子词库: {}", rime_ice_base.display());
    builder.load_rime_dict_file(&rime_ice_base, 10.0, DomainTag::General)?;

    println!("2. 正在加载 melt_eng + 程序员规范大小写核心词库: {}", melt_eng.display());
    builder.load_english_dict_file(&melt_eng, 10.0)?;

    println!("3. 正在加载万象双语二元语法模型 (Bigram): {}", wanxiang_bigram.display());
    builder.load_bigram_file(&wanxiang_bigram)?;

    // 扫描 data/raw/ 下的全量开源词库（如雾凇 8105、base、ext、melt_eng_full）
    let raw_dir = root.join("data/raw");
    if raw_dir.exists() {
        for entry in std::fs::read_dir(&raw_dir)?.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.ends_with(".dict.yaml") {
                println!("   [+] 正在融合全量开源词表: {}", path.display());
                if name.contains("eng") {
                    builder.load_english_dict_file(&path, 1.0)?;
                } else {
                    builder.load_rime_dict_file(&path, 1.0, DomainTag::General)?;
                }
            }
        }
    }

    let lexicon = builder.build();
    let build_elapsed = start_build.elapsed();

    let out_path = root.join("data/compiled/merged_lexicon.bin");
    lexicon.save_to_file(&out_path)?;
    let file_size_kb = std::fs::metadata(&out_path)?.len() / 1024;

    let start_mmap = Instant::now();
    let loaded = CompiledLexicon::load_mmap(&out_path)?;
    let mmap_elapsed = start_mmap.elapsed();

    let cn_count = loaded.entries.iter().filter(|e| !e.is_english).count();
    let en_count = loaded.entries.iter().filter(|e| e.is_english).count();

    println!("------------------------------------------------------------");
    println!("编译完成！产物路径: {} ({} KB)", out_path.display(), file_size_kb);
    println!("  - 中文词条总数   : {}", cn_count);
    println!("  - 英文混输词条数 : {}", en_count);
    println!("  - Bigram 转移对数: {}", loaded.bigrams.len());
    println!("  - 音节 Trie 节点 : {}", loaded.syllable_trie.nodes.len());
    println!("  - 简拼 Trie 节点 : {}", loaded.abbrev_trie.nodes.len());
    println!("  - 英文 Trie 节点 : {}", loaded.english_trie.nodes.len());
    println!("  - 编译构建耗时   : {:?}", build_elapsed);
    println!("  - Mmap 加载耗时  : {:?}", mmap_elapsed);
    println!("------------------------------------------------------------");

    Ok(())
}
