//! 构建脚本：将 `assets/rust_ai_ime.ico` 嵌入所有可执行文件的 Win32 资源段（图标资源 ID = 1）。
//!
//! 工具链为 `x86_64-pc-windows-gnu` 且本机没有 `windres` / `rc.exe`，因此这里直接生成一个
//! 只含 `.rsrc` 节的 COFF 目标文件（与 `windres` 产物格式一致），交给链接器合并进 PE。
//! 这样任务栏、资源管理器、桌面快捷方式、Alt+Tab 都能显示正确的输入法图标。

use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const LANG_NEUTRAL: u32 = 0;

struct IconImage {
    width: u8,
    height: u8,
    color_count: u8,
    planes: u16,
    bit_count: u16,
    data: Vec<u8>,
}

fn parse_ico(bytes: &[u8]) -> Vec<IconImage> {
    let rd16 = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
    let rd32 = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    assert!(bytes.len() >= 6 && rd16(2) == 1, "assets/rust_ai_ime.ico 不是有效的 ICO 文件");
    let count = rd16(4) as usize;
    (0..count)
        .map(|i| {
            let e = 6 + 16 * i;
            let size = rd32(e + 8) as usize;
            let off = rd32(e + 12) as usize;
            IconImage {
                width: bytes[e],
                height: bytes[e + 1],
                color_count: bytes[e + 2],
                planes: rd16(e + 4),
                bit_count: rd16(e + 6),
                data: bytes[off..off + size].to_vec(),
            }
        })
        .collect()
}

/// 资源树中的一个叶子：(类型, 名称 ID, 原始数据)
struct Leaf {
    ty: u32,
    id: u32,
    data: Vec<u8>,
}

fn build_rsrc_section(leaves: &[Leaf]) -> (Vec<u8>, Vec<u32>) {
    // 按类型分组（类型与 ID 均需升序排列）
    let mut types: Vec<u32> = leaves.iter().map(|l| l.ty).collect();
    types.sort_unstable();
    types.dedup();

    let dir_size = |n: usize| 16 + 8 * n;
    // 布局：根目录 → 每个类型的名称目录 → 每个名称的语言目录 → 数据项 → 数据
    let mut offset = dir_size(types.len());
    let mut name_dir_off = Vec::new();
    for &t in &types {
        name_dir_off.push(offset);
        offset += dir_size(leaves.iter().filter(|l| l.ty == t).count());
    }
    let mut ordered: Vec<&Leaf> = Vec::new();
    for &t in &types {
        let mut ls: Vec<&Leaf> = leaves.iter().filter(|l| l.ty == t).collect();
        ls.sort_by_key(|l| l.id);
        ordered.extend(ls);
    }
    let mut lang_dir_off = Vec::new();
    for _ in &ordered {
        lang_dir_off.push(offset);
        offset += dir_size(1);
    }
    let mut data_entry_off = Vec::new();
    for _ in &ordered {
        data_entry_off.push(offset);
        offset += 16;
    }
    let mut data_off = Vec::new();
    for l in &ordered {
        offset = (offset + 7) & !7;
        data_off.push(offset);
        offset += l.data.len();
    }
    let total = (offset + 7) & !7;

    let mut buf = vec![0u8; total];
    let w16 = |b: &mut Vec<u8>, o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    let w32 = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let write_dir = |b: &mut Vec<u8>, at: usize, entries: &[(u32, u32)]| {
        w16(b, at + 14, entries.len() as u16); // NumberOfIdEntries
        for (i, &(name, target)) in entries.iter().enumerate() {
            w32(b, at + 16 + 8 * i, name);
            w32(b, at + 16 + 8 * i + 4, target);
        }
    };

    // 根目录
    let root_entries: Vec<(u32, u32)> = types
        .iter()
        .zip(&name_dir_off)
        .map(|(&t, &o)| (t, 0x8000_0000 | o as u32))
        .collect();
    write_dir(&mut buf, 0, &root_entries);

    // 名称目录
    let mut idx = 0usize;
    for (ti, &t) in types.iter().enumerate() {
        let n = ordered.iter().filter(|l| l.ty == t).count();
        let entries: Vec<(u32, u32)> = (0..n)
            .map(|k| (ordered[idx + k].id, 0x8000_0000 | lang_dir_off[idx + k] as u32))
            .collect();
        write_dir(&mut buf, name_dir_off[ti], &entries);
        idx += n;
    }

    // 语言目录 + 数据项 + 数据
    let mut reloc_offsets = Vec::new();
    for (i, l) in ordered.iter().enumerate() {
        write_dir(&mut buf, lang_dir_off[i], &[(LANG_NEUTRAL, data_entry_off[i] as u32)]);
        let de = data_entry_off[i];
        w32(&mut buf, de, data_off[i] as u32); // OffsetToData（RVA，需 ADDR32NB 重定位）
        w32(&mut buf, de + 4, l.data.len() as u32);
        reloc_offsets.push(de as u32);
        buf[data_off[i]..data_off[i] + l.data.len()].copy_from_slice(&l.data);
    }
    (buf, reloc_offsets)
}

fn build_coff_object(section: &[u8], relocs: &[u32]) -> Vec<u8> {
    const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
    const IMAGE_REL_AMD64_ADDR32NB: u16 = 3;
    const SCN_FLAGS: u32 = 0x0000_0040 | 0x4000_0000 | 0x0030_0000; // INITIALIZED_DATA | MEM_READ | ALIGN_4BYTES

    let header_size = 20 + 40;
    let raw_ptr = header_size;
    let reloc_ptr = raw_ptr + section.len();
    let sym_ptr = reloc_ptr + relocs.len() * 10;

    let mut o = Vec::new();
    o.extend_from_slice(&IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    o.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    o.extend_from_slice(&(sym_ptr as u32).to_le_bytes());
    o.extend_from_slice(&2u32.to_le_bytes()); // NumberOfSymbols（节符号 + 1 个辅助记录）
    o.extend_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    o.extend_from_slice(&0u16.to_le_bytes()); // Characteristics

    o.extend_from_slice(b".rsrc\0\0\0");
    o.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
    o.extend_from_slice(&0u32.to_le_bytes()); // VirtualAddress
    o.extend_from_slice(&(section.len() as u32).to_le_bytes());
    o.extend_from_slice(&(raw_ptr as u32).to_le_bytes());
    o.extend_from_slice(&(reloc_ptr as u32).to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes()); // PointerToLinenumbers
    o.extend_from_slice(&(relocs.len() as u16).to_le_bytes());
    o.extend_from_slice(&0u16.to_le_bytes());
    o.extend_from_slice(&SCN_FLAGS.to_le_bytes());

    o.extend_from_slice(section);
    for &r in relocs {
        o.extend_from_slice(&r.to_le_bytes());
        o.extend_from_slice(&0u32.to_le_bytes()); // 符号索引 0 = .rsrc 节符号
        o.extend_from_slice(&IMAGE_REL_AMD64_ADDR32NB.to_le_bytes());
    }

    // 符号表：.rsrc 节符号 + 节定义辅助记录
    o.extend_from_slice(b".rsrc\0\0\0");
    o.extend_from_slice(&0u32.to_le_bytes()); // Value
    o.extend_from_slice(&1i16.to_le_bytes()); // SectionNumber
    o.extend_from_slice(&0u16.to_le_bytes()); // Type
    o.push(3); // IMAGE_SYM_CLASS_STATIC
    o.push(1); // NumberOfAuxSymbols
    o.extend_from_slice(&(section.len() as u32).to_le_bytes());
    o.extend_from_slice(&(relocs.len() as u16).to_le_bytes());
    o.extend_from_slice(&0u16.to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes()); // CheckSum
    o.extend_from_slice(&0u16.to_le_bytes()); // Number
    o.push(0); // Selection
    o.extend_from_slice(&[0u8; 3]);
    // 字符串表（仅含 4 字节长度）
    o.extend_from_slice(&4u32.to_le_bytes());
    o
}

fn main() {
    println!("cargo:rerun-if-changed=assets/rust_ai_ime.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico_bytes = fs::read(manifest_dir.join("assets").join("rust_ai_ime.ico"))
        .expect("缺少 assets/rust_ai_ime.ico");
    let images = parse_ico(&ico_bytes);

    let mut leaves = Vec::new();
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        let icon_id = (i + 1) as u32;
        group.push(img.width);
        group.push(img.height);
        group.push(img.color_count);
        group.push(0);
        group.extend_from_slice(&img.planes.to_le_bytes());
        group.extend_from_slice(&img.bit_count.to_le_bytes());
        group.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        group.extend_from_slice(&(icon_id as u16).to_le_bytes());
        leaves.push(Leaf {
            ty: RT_ICON,
            id: icon_id,
            data: img.data.clone(),
        });
    }
    leaves.push(Leaf {
        ty: RT_GROUP_ICON,
        id: 1,
        data: group,
    });

    let (section, relocs) = build_rsrc_section(&leaves);
    let obj = build_coff_object(&section, &relocs);
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("rust_ai_ime_icon_res.o");
    fs::write(&out, obj).expect("写入图标资源目标文件失败");
    println!("cargo:rustc-link-arg-bins={}", out.display());
}
