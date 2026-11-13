//! Measures what the current lexicon format costs in memory and time once loaded,
//! using the same metric iOS jetsam uses for its per-process limit (phys_footprint).
use ime_core::{AppScene, ImeCoreEngine};
use ime_lexicon::CompiledLexicon;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
struct RUsageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
}

extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RUsageInfoV2) -> i32;
    fn getpid() -> i32;
}

/// (phys_footprint, resident) in MiB.
fn mem() -> (f64, f64) {
    let mut r = RUsageInfoV2::default();
    unsafe { proc_pid_rusage(getpid(), 2, &mut r) };
    (r.ri_phys_footprint as f64 / 1048576.0, r.ri_resident_size as f64 / 1048576.0)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: lexprobe <merged_lexicon.bin> [--verify]");
    // --verify: run the full integrity check right after loading. It reads every page of the
    // mapping, so the decode timings below are then free of first-touch page faults.
    let verify = std::env::args().any(|a| a == "--verify");
    let file_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1048576.0;
    let (f0, r0) = mem();
    println!("bin file: {file_mb:.1} MiB");
    println!("baseline: footprint {f0:.1} MiB, resident {r0:.1} MiB");

    let t = Instant::now();
    let lex = CompiledLexicon::load_mmap(&path).unwrap();
    let load = t.elapsed();
    let (f1, r1) = mem();
    println!(
        "after load_mmap ({load:.2?}): footprint {f1:.1} MiB (+{:.1}), resident {r1:.1} MiB",
        f1 - f0
    );
    println!(
        "entries {}, syllable trie nodes {}, abbrev trie nodes {}, english trie nodes {}, bigrams {}",
        lex.entry_count(),
        lex.syllable_trie().node_count(),
        lex.abbrev_trie().node_count(),
        lex.english_trie().node_count(),
        lex.bigram_count()
    );

    if verify {
        let t = Instant::now();
        lex.verify().expect("lexicon integrity check");
        let el = t.elapsed();
        let (f, r) = mem();
        println!(
            "after verify ({el:.2?}): footprint {f:.1} MiB (+{:.1}), resident {r:.1} MiB",
            f - f0
        );
    }

    let engine = ImeCoreEngine::new(lex);
    let (f2, _) = mem();
    println!("after ImeCoreEngine::new: footprint {f2:.1} MiB");

    let inputs = [
        "nihao",
        "wozaiyongrustxieapi",
        "jintiantianqihenhao",
        "fanzhongli",
        "zhegecishishenmeyisi",
        "zenmehuishi",
        "woxiangqugongyuanwan",
        "fuliyejishu",
        "bzyl",
        "zhongguorenminyinhang",
    ];
    for inp in inputs {
        // Type it one letter at a time, as a keyboard would, and keep the slowest keystroke.
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        let mut last = None;
        for end in 1..=inp.len() {
            let t = Instant::now();
            let out = engine.decode(&inp[..end], AppScene::General, "");
            let el = t.elapsed();
            worst = worst.max(el);
            total += el;
            last = Some(out);
        }
        let out = last.unwrap();
        let top: Vec<String> = out.candidates.iter().take(3).map(|c| c.text.clone()).collect();
        println!(
            "{inp:>24}: worst key {worst:>9.2?}, mean {:>9.2?}, edges {:>5}, top {top:?}",
            total / inp.len() as u32,
            out.lattice_edge_count
        );
    }
    let (f3, r3) = mem();
    println!("after decoding: footprint {f3:.1} MiB, resident {r3:.1} MiB");
}
