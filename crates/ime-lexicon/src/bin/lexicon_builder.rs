//! `lexicon-builder`: 词库融合编译 CLI 工具 (支持 AIMELX03 零拷贝二进制格式与十万级开源词库融合)
//!
//! 用法：
//! - `lexicon-builder`：融合 `data/seed` 与 `data/raw` 下的词表（`data/raw` 按文件名顺序加载），
//!   写出 `data/compiled/merged_lexicon.bin`。同样的词表每次编译出逐字节相同的镜像
//! - `lexicon-builder --migrate-v2 <旧 AIMELX02 镜像> [输出路径]`：把旧版镜像原样转成 `AIMELX03`
//!   （保留词条顺序与前缀树内容；省略输出路径时就地替换）
//! - `lexicon-builder --verify <镜像>`：只做完整性校验并打印统计，校验不过时以非零状态退出
//!   （打包进安装包或 App 之前用）
//! - `lexicon-builder --build <输出镜像> <RIME 词表>...`：只用给定的词表（权重原样）编译，不读 `data/seed`、
//!   `data/raw`。iOS 键盘的基础词库（宽松协议的小词库，`fallback_lexicon.bin`）用它生成

use ime_lexicon::{CompiledLexicon, DomainTag, LexiconParts, MultiSourceBuilder};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 打印一份已落盘镜像的统计信息（重新以 mmap 加载并做一次完整性深度校验）
fn report(out_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let file_size_kb = std::fs::metadata(out_path)?.len() / 1024;

    let start_mmap = Instant::now();
    let loaded = CompiledLexicon::load_mmap(out_path)?;
    let mmap_elapsed = start_mmap.elapsed();
    let start_verify = Instant::now();
    loaded.verify()?;
    let verify_elapsed = start_verify.elapsed();

    let cn_count = loaded.entries().filter(|e| !e.is_english()).count();
    let en_count = loaded.entries().filter(|e| e.is_english()).count();

    println!("------------------------------------------------------------");
    println!("校验通过，镜像路径: {} ({} KB)", out_path.display(), file_size_kb);
    println!("  - 中文词条总数   : {}", cn_count);
    println!("  - 英文混输词条数 : {}", en_count);
    println!("  - Bigram 转移对数: {}", loaded.bigram_count());
    println!("  - 音节 Trie 节点 : {}", loaded.syllable_trie().node_count());
    println!("  - 简拼 Trie 节点 : {}", loaded.abbrev_trie().node_count());
    println!("  - 英文 Trie 节点 : {}", loaded.english_trie().node_count());
    println!("  - Mmap 加载耗时  : {:?}", mmap_elapsed);
    println!("  - 完整性校验耗时 : {:?}", verify_elapsed);
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s.as_str()) == Some("--verify") {
        let Some(path) = args.get(1) else {
            eprintln!("用法: lexicon-builder --verify <镜像>");
            std::process::exit(2);
        };
        report(Path::new(path))?;
        println!("------------------------------------------------------------");
        return Ok(());
    }
    if args.first().map(|s| s.as_str()) == Some("--build") {
        let (Some(out), true) = (args.get(1), args.len() > 2) else {
            eprintln!("用法: lexicon-builder --build <输出镜像> <RIME 词表>...");
            std::process::exit(2);
        };
        println!("=== [AI Pinyin IME] 按给定词表编译 ===");
        let start = Instant::now();
        let mut builder = MultiSourceBuilder::new();
        for path in &args[2..] {
            println!("   [+] {}", path);
            builder.load_rime_dict_file(path, 1.0, DomainTag::General)?;
        }
        let lexicon = builder.build();
        lexicon.save_to_file(Path::new(out))?;
        report(Path::new(out))?;
        println!("  - 编译构建耗时   : {:?}", start.elapsed());
        println!("------------------------------------------------------------");
        return Ok(());
    }
    if args.first().map(|s| s.as_str()) == Some("--migrate-v2") {
        let Some(src) = args.get(1) else {
            eprintln!("用法: lexicon-builder --migrate-v2 <旧 AIMELX02 镜像> [输出路径]");
            std::process::exit(2);
        };
        let dst = PathBuf::from(args.get(2).unwrap_or(src));
        println!("=== [AI Pinyin IME] AIMELX02 -> AIMELX03 镜像迁移 ===");
        let start = Instant::now();
        let parts = LexiconParts::load_legacy_v2(src)?;
        let lexicon = CompiledLexicon::from_parts(&parts)?;
        drop(parts);
        lexicon.save_to_file(&dst)?;
        println!("迁移耗时: {:?}", start.elapsed());
        report(&dst)?;
        println!("------------------------------------------------------------");
        return Ok(());
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    println!("=== [AI Pinyin IME] 多源词库融合编译器 v3 (AIMELX03 Zero-Copy Binary Format) ===");
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

    // 扫描 data/raw/ 下的全量开源词库（如雾凇 8105、base、ext、melt_eng_full）。
    // read_dir 的顺序由文件系统决定（APFS、ext4、NTFS 各不相同），而同一词条出现在多个文件里时，
    // 先加载的文件决定它的声调、领域等字段：按文件名排序后再加载，换一台机器也编译出相同的词库。
    // 读目录出错就直接报错，不悄悄少加载一个文件
    let raw_dir = root.join("data/raw");
    if raw_dir.exists() {
        let mut raw_files: Vec<(String, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&raw_dir)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // 跳过隐藏文件（比如 macOS 打包带出来的 ._xxx.dict.yaml）
            if name.ends_with(".dict.yaml") && !name.starts_with('.') {
                raw_files.push((name.to_string(), path));
            }
        }
        raw_files.sort();
        for (name, path) in &raw_files {
            println!("   [+] 正在融合全量开源词表: {}", path.display());
            if name.contains("eng") {
                builder.load_english_dict_file(path, 1.0)?;
            } else {
                builder.load_rime_dict_file(path, 1.0, DomainTag::General)?;
            }
        }
    }

    let lexicon = builder.build();
    let build_elapsed = start_build.elapsed();

    let out_path = root.join("data/compiled/merged_lexicon.bin");
    lexicon.save_to_file(&out_path)?;
    report(&out_path)?;
    println!("  - 编译构建耗时   : {:?}", build_elapsed);
    println!("------------------------------------------------------------");

    Ok(())
}
