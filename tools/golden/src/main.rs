//! golden dump <repo_root> <inputs.tsv> <out.jsonl>
//! Uses only the stable engine API, so it builds before and after the lexicon format change.
use ime_ai::{DualModeAiOrchestrator, OpenAiCompatibleConfig};
use ime_core::{AppScene, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use std::io::{BufRead, BufWriter, Write};
use std::time::Instant;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "dump" {
        eprintln!("usage: golden dump <repo_root> <inputs.tsv> <out.jsonl>");
        std::process::exit(2);
    }
    let bin = std::path::Path::new(&args[2]).join("data/compiled/merged_lexicon.bin");
    let t = Instant::now();
    let lexicon = CompiledLexicon::load_mmap(&bin).expect("load lexicon");
    let load = t.elapsed();
    let engine = ImeCoreEngine::new(lexicon);
    let mut ai = DualModeAiOrchestrator::new(OpenAiCompatibleConfig::default());
    let input = std::io::BufReader::new(std::fs::File::open(&args[3]).expect("inputs"));
    let mut out = BufWriter::new(std::fs::File::create(&args[4]).expect("out"));
    let mut n = 0usize;
    let mut worst = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    for line in input.lines() {
        let line = line.unwrap();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let pinyin = parts.next().unwrap();
        let context = parts.next().unwrap_or("");
        let t = Instant::now();
        let decoded = engine.decode(pinyin, AppScene::General, context);
        let el = t.elapsed();
        worst = worst.max(el);
        total += el;
        let reranked = ai.rerank(pinyin, context, AppScene::General, decoded.candidates.clone());
        // One compact line per input: a hash of everything, plus the top texts for reading diffs.
        // Set GOLDEN_FULL=1 to write the complete candidate lists instead.
        let full = serde_json::json!({
            "edges": decoded.lattice_edge_count,
            "decoded": decoded.candidates,
            "reranked": reranked.candidates,
        });
        let row = if std::env::var_os("GOLDEN_FULL").is_some() {
            serde_json::json!({ "input": pinyin, "context": context, "full": full })
        } else {
            let top = |c: &[ime_core::Candidate]| -> Vec<String> {
                c.iter().take(5).map(|x| format!("{}|{}", x.text, x.consumed_len)).collect()
            };
            serde_json::json!({
                "input": pinyin,
                "context": context,
                "n": decoded.candidates.len(),
                "top": top(&decoded.candidates),
                "rtop": top(&reranked.candidates),
                "hash": format!("{:016x}", fnv1a(full.to_string().as_bytes())),
            })
        };
        writeln!(out, "{row}").unwrap();
        n += 1;
    }
    out.flush().unwrap();
    eprintln!(
        "golden: {n} inputs, lexicon load {load:.2?}, decode mean {:.1?}, worst {worst:.2?}",
        total / n.max(1) as u32
    );
}
