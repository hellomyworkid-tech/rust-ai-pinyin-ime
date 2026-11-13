//! 从命令行经由 `ImeEngine`（即 iOS 键盘用的同一层封装）打字，看候选。
//!
//! 用法：cargo run -q -p ime-ffi --example ffi_type -- [--lexicon <文件>] [--top N] <上文>:<拼音> ...
//!   例：cargo run -q -p ime-ffi --example ffi_type -- :nihao 宪法保障公民的合法:quanli
//! 每个参数先把「上文」设为光标前文本，再逐个字母输入「拼音」，打印最后一键之后的前 N 个候选。

use ime_ffi::{ImeEngine, IME_KEY_CHAR};
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut lexicon = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/compiled/merged_lexicon.bin");
    let mut top = 5usize;
    let mut cases: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--lexicon" => lexicon = PathBuf::from(args.next().expect("--lexicon 需要路径")),
            "--top" => top = args.next().and_then(|v| v.parse().ok()).expect("--top 需要数字"),
            _ => cases.push(arg),
        }
    }
    let engine = match ImeEngine::open(&lexicon, None) {
        Ok(engine) => engine,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    for case in cases {
        let (context, pinyin) = case.split_once(':').unwrap_or(("", case.as_str()));
        engine.reset();
        engine.set_context(Some(context));
        let mut last = String::new();
        let mut worst = std::time::Duration::ZERO;
        for ch in pinyin.chars() {
            let start = Instant::now();
            last = engine.key(IME_KEY_CHAR, ch as u32);
            worst = worst.max(start.elapsed());
        }
        let value: serde_json::Value = serde_json::from_str(&last).unwrap_or_default();
        let view = &value["view"];
        println!(
            "[{context}] {pinyin} -> {} (共 {} 个候选，最慢一键 {:?}，JSON {} 字节)",
            view["composed_full_sentence"].as_str().unwrap_or("(无)"),
            view["total_candidates"],
            worst,
            last.len()
        );
        if let Some(candidates) = view["visible_candidates"].as_array() {
            for (i, c) in candidates.iter().take(top).enumerate() {
                println!(
                    "  {}. {} {}{}",
                    i + 1,
                    c["text"].as_str().unwrap_or(""),
                    c["tag"].as_str().unwrap_or(""),
                    if c["ai_boosted"] == true { " ★AI" } else { "" }
                );
            }
        }
    }
}
