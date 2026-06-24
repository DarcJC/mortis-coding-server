//! Pure binary-analysis helpers over downloaded bytes.
//!
//! Detection/metadata use the [`object`] crate (PE/ELF/Mach-O, incl. fat);
//! disassembly uses [`capstone`]. Every function takes the *whole downloaded
//! file* and internally resolves the "effective object slice" — the whole file
//! for a thin binary, or the chosen architecture slice for a fat Mach-O — so
//! callers never have to think about fat offsets.

use capstone::prelude::*;
use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
use object::{
    Architecture, FileKind, Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolKind,
};

use mortis_core::asm::{
    BinaryFormat, BinaryInfo, BinaryOs, FunctionResolution, Instruction, SectionInfo, SegmentInfo,
};
use mortis_core::error::{CoreError, Result};

/// Human-readable architecture name (`Debug`-derived, lowercased), e.g.
/// `x86_64`, `aarch64`, `arm`, `i386`.
fn arch_name(arch: Architecture) -> String {
    format!("{arch:?}").to_ascii_lowercase()
}

/// Map an [`object::FileKind`] to our `(format, os)`, or `None` for kinds we do
/// not treat as a supported executable (archives, COFF objects, WASM, …).
fn os_format_for(kind: FileKind) -> Option<(BinaryFormat, BinaryOs)> {
    match kind {
        FileKind::Pe32 | FileKind::Pe64 => Some((BinaryFormat::Pe, BinaryOs::Windows)),
        FileKind::Elf32 | FileKind::Elf64 => Some((BinaryFormat::Elf, BinaryOs::Linux)),
        FileKind::MachO32 | FileKind::MachO64 => Some((BinaryFormat::MachO, BinaryOs::Apple)),
        FileKind::MachOFat32 | FileKind::MachOFat64 => {
            Some((BinaryFormat::MachOFat, BinaryOs::Apple))
        }
        _ => None,
    }
}

/// Resolve the bytes that should be parsed as a single object file: the whole
/// file for a thin binary, or the chosen architecture slice for a fat Mach-O.
///
/// Returns the slice together with the detected format/OS and (for fat
/// binaries) the list of all architectures present.
fn effective_slice(data: &[u8]) -> Result<(&[u8], BinaryFormat, BinaryOs, Vec<String>)> {
    let kind = FileKind::parse(data)
        .map_err(|e| CoreError::invalid(format!("unrecognized binary: {e}")))?;
    match kind {
        FileKind::MachOFat32 => {
            let fat = MachOFatFile32::parse(data)
                .map_err(|e| CoreError::invalid(format!("invalid fat mach-o: {e}")))?;
            let (slice, subs) = pick_fat_slice(fat.arches(), data)?;
            Ok((slice, BinaryFormat::MachOFat, BinaryOs::Apple, subs))
        }
        FileKind::MachOFat64 => {
            let fat = MachOFatFile64::parse(data)
                .map_err(|e| CoreError::invalid(format!("invalid fat mach-o: {e}")))?;
            let (slice, subs) = pick_fat_slice(fat.arches(), data)?;
            Ok((slice, BinaryFormat::MachOFat, BinaryOs::Apple, subs))
        }
        other => {
            let (format, os) = os_format_for(other)
                .ok_or_else(|| CoreError::invalid(format!("unsupported binary format: {other:?}")))?;
            Ok((data, format, os, Vec::new()))
        }
    }
}

/// From a fat Mach-O's architecture table, pick a primary slice (preferring
/// arm64, then x86_64, else the first) and collect every architecture's name.
fn pick_fat_slice<'data, A: FatArch>(
    arches: &[A],
    data: &'data [u8],
) -> Result<(&'data [u8], Vec<String>)> {
    if arches.is_empty() {
        return Err(CoreError::invalid("fat mach-o has no architectures"));
    }
    let subs: Vec<String> = arches.iter().map(|a| arch_name(a.architecture())).collect();
    let chosen = arches
        .iter()
        .find(|a| a.architecture() == Architecture::Aarch64)
        .or_else(|| arches.iter().find(|a| a.architecture() == Architecture::X86_64))
        .unwrap_or(&arches[0]);
    let slice = chosen
        .data(data)
        .map_err(|e| CoreError::invalid(format!("fat slice out of range: {e}")))?;
    Ok((slice, subs))
}

/// Detect, validate, and summarize a binary's headers/sections/symbols.
///
/// Errors with [`CoreError::InvalidInput`] when the bytes are not a recognized,
/// supported executable (PE/ELF/Mach-O/fat Mach-O).
pub fn detect_and_describe(data: &[u8]) -> Result<BinaryInfo> {
    let (slice, format, os, sub_archs) = effective_slice(data)?;
    let file = object::File::parse(slice)
        .map_err(|e| CoreError::invalid(format!("cannot parse binary: {e}")))?;

    let sections = file
        .sections()
        .map(|s| SectionInfo {
            name: s.name().unwrap_or("").to_string(),
            address: s.address(),
            size: s.size(),
            file_offset: s.file_range().map(|(off, _)| off).unwrap_or(0),
        })
        .collect();

    let segments = file
        .segments()
        .map(|s| {
            let (file_offset, file_size) = s.file_range();
            SegmentInfo {
                name: s.name().ok().flatten().unwrap_or("").to_string(),
                address: s.address(),
                size: s.size(),
                file_offset,
                file_size,
            }
        })
        .collect();

    Ok(BinaryInfo {
        format,
        os,
        arch: arch_name(file.architecture()),
        bits: if file.is_64() { 64 } else { 32 },
        little_endian: file.is_little_endian(),
        entry: file.entry(),
        sections,
        segments,
        symbol_count: file.symbols().count(),
        import_count: file.imports().map(|v| v.len()).unwrap_or(0),
        export_count: file.exports().map(|v| v.len()).unwrap_or(0),
        sub_archs,
    })
}

/// Map a virtual-address range to the file-backed bytes that contain it.
///
/// Prefers sections (finer-grained) and falls back to loadable segments. An
/// address in a region with no file bytes (e.g. `.bss`) or outside any mapped
/// range is rejected, never silently clamped to the wrong location.
fn va_to_bytes<'a>(
    file: &object::File<'a>,
    slice: &'a [u8],
    start_va: u64,
    len: u64,
) -> Result<&'a [u8]> {
    // Sections first.
    for section in file.sections() {
        let addr = section.address();
        let size = section.size();
        if size == 0 || start_va < addr || start_va >= addr + size {
            continue;
        }
        if let Some((file_off, file_size)) = section.file_range() {
            if let Some(bytes) = clamp_slice(slice, file_off, file_size, start_va - addr, len) {
                return Ok(bytes);
            }
        }
        break;
    }
    // Loadable segments.
    for seg in file.segments() {
        let addr = seg.address();
        let size = seg.size();
        if size == 0 || start_va < addr || start_va >= addr + size {
            continue;
        }
        let (file_off, file_size) = seg.file_range();
        if let Some(bytes) = clamp_slice(slice, file_off, file_size, start_va - addr, len) {
            return Ok(bytes);
        }
        break;
    }
    Err(CoreError::invalid(
        "address not in a mapped, file-backed range",
    ))
}

/// Compute `slice[file_off + delta .. + min(len, file_size - delta)]`, or
/// `None` if `delta` is past the file-backed bytes or the range overruns.
fn clamp_slice(
    slice: &[u8],
    file_off: u64,
    file_size: u64,
    delta: u64,
    len: u64,
) -> Option<&[u8]> {
    if delta >= file_size {
        return None;
    }
    let avail = file_size - delta;
    let take = len.min(avail);
    let begin = file_off.checked_add(delta)? as usize;
    let end = begin.checked_add(take as usize)?;
    if end <= slice.len() { Some(&slice[begin..end]) } else { None }
}

/// Disassemble `len` bytes starting at virtual address `start`.
pub fn disassemble_range(data: &[u8], start: u64, len: u64) -> Result<Vec<Instruction>> {
    if len == 0 {
        return Err(CoreError::invalid("length must be greater than zero"));
    }
    let (slice, _, _, _) = effective_slice(data)?;
    let file = object::File::parse(slice)
        .map_err(|e| CoreError::invalid(format!("cannot parse binary: {e}")))?;
    let code = va_to_bytes(&file, slice, start, len)?;
    let cs = build_capstone(file.architecture())?;
    run_disasm(&cs, code, start)
}

/// Resolve a virtual address to the function symbol containing (or nearest
/// preceding) it. A stripped binary yields `name: None`, not an error.
pub fn resolve_function(data: &[u8], address: u64) -> Result<FunctionResolution> {
    let (slice, _, _, _) = effective_slice(data)?;
    let file = object::File::parse(slice)
        .map_err(|e| CoreError::invalid(format!("cannot parse binary: {e}")))?;

    let mut exact: Option<(u64, String)> = None;
    let mut prev: Option<(u64, String)> = None;
    for sym in file.symbols() {
        if sym.kind() != SymbolKind::Text {
            continue;
        }
        let Ok(name) = sym.name() else { continue };
        if name.is_empty() {
            continue;
        }
        let value = sym.address();
        let size = sym.size();
        if size > 0 && address >= value && address < value + size {
            exact = Some((value, name.to_string()));
            break;
        }
        if value <= address && prev.as_ref().is_none_or(|(v, _)| value > *v) {
            prev = Some((value, name.to_string()));
        }
    }

    let (name, symbol_start, is_exact) = match (exact, prev) {
        (Some((v, n)), _) => (Some(n), Some(v), true),
        (None, Some((v, n))) => (Some(n), Some(v), false),
        (None, None) => (None, None, false),
    };
    Ok(FunctionResolution {
        address,
        name,
        symbol_start,
        offset: symbol_start.map(|v| address - v),
        exact: is_exact,
    })
}

/// Build a capstone engine for the binary's architecture.
///
/// Only the little-endian architectures we target (Windows/Linux/Android/
/// iOS/macOS on x86/x64/ARM/ARM64) are supported; others are rejected cleanly.
/// 32-bit ARM defaults to the ARM (not Thumb) instruction set.
fn build_capstone(arch: Architecture) -> Result<Capstone> {
    let res = match arch {
        Architecture::X86_64 => Capstone::new()
            .x86()
            .mode(capstone::arch::x86::ArchMode::Mode64)
            .build(),
        Architecture::I386 => Capstone::new()
            .x86()
            .mode(capstone::arch::x86::ArchMode::Mode32)
            .build(),
        Architecture::Aarch64 => Capstone::new()
            .arm64()
            .mode(capstone::arch::arm64::ArchMode::Arm)
            .build(),
        Architecture::Arm => Capstone::new()
            .arm()
            .mode(capstone::arch::arm::ArchMode::Arm)
            .build(),
        other => {
            return Err(CoreError::invalid(format!(
                "unsupported architecture for disassembly: {}",
                arch_name(other)
            )));
        }
    };
    res.map_err(|e| CoreError::Other(format!("capstone init failed: {e}")))
}

/// Disassemble `code` (mapped at `start`) into structured instructions.
fn run_disasm(cs: &Capstone, code: &[u8], start: u64) -> Result<Vec<Instruction>> {
    let insns = cs
        .disasm_all(code, start)
        .map_err(|e| CoreError::Other(format!("disassembly failed: {e}")))?;
    Ok(insns
        .iter()
        .map(|i| Instruction {
            address: i.address(),
            bytes: hex_encode(i.bytes()),
            mnemonic: i.mnemonic().unwrap_or("").to_string(),
            operands: i.op_str().unwrap_or("").to_string(),
        })
        .collect())
}

/// Lowercase hex with no separators (e.g. `&[0x48, 0x89]` → `"4889"`).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::write::{Object, StandardSection, Symbol, SymbolSection};
    use object::{
        BinaryFormat as WriteFormat, Endianness, SymbolFlags, SymbolKind as WSymbolKind,
        SymbolScope,
    };

    /// `push rbp; mov rbp, rsp; ret` — a tiny, well-known x86-64 prologue/epilogue.
    const X64_CODE: [u8; 5] = [0x55, 0x48, 0x89, 0xe5, 0xc3];

    /// Synthesize a relocatable object with a `.text` section holding `code`
    /// and one function symbol `sym` covering it.
    fn obj_with_code(format: WriteFormat, arch: Architecture, code: &[u8], sym: &str) -> Vec<u8> {
        let mut obj = Object::new(format, arch, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        obj.append_section_data(text, code, 1);
        obj.add_symbol(Symbol {
            name: sym.as_bytes().to_vec(),
            value: 0,
            size: code.len() as u64,
            kind: WSymbolKind::Text,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        obj.write().unwrap()
    }

    /// Wrap thin Mach-O slices in a big-endian FAT_MAGIC (32-bit) container.
    fn fat_macho(slices: &[(u32, &[u8])]) -> Vec<u8> {
        const ALIGN: u32 = 0x4000;
        let align_up = |x: u32| x.div_ceil(ALIGN) * ALIGN;
        let header = (8 + 20 * slices.len()) as u32;
        let mut offsets = Vec::new();
        let mut cur = align_up(header);
        for (_, data) in slices {
            offsets.push(cur);
            cur = align_up(cur + data.len() as u32);
        }
        let mut out = vec![0u8; cur as usize];
        out[0..4].copy_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        out[4..8].copy_from_slice(&(slices.len() as u32).to_be_bytes());
        for (i, (cputype, data)) in slices.iter().enumerate() {
            let b = 8 + i * 20;
            out[b..b + 4].copy_from_slice(&cputype.to_be_bytes());
            // cpusubtype (b+4..b+8) left zero
            out[b + 8..b + 12].copy_from_slice(&offsets[i].to_be_bytes());
            out[b + 12..b + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
            out[b + 16..b + 20].copy_from_slice(&14u32.to_be_bytes()); // align 2^14
            let o = offsets[i] as usize;
            out[o..o + data.len()].copy_from_slice(data);
        }
        out
    }

    #[test]
    fn detect_elf_is_linux() {
        let data = obj_with_code(WriteFormat::Elf, Architecture::X86_64, &X64_CODE, "f");
        let info = detect_and_describe(&data).unwrap();
        assert_eq!(info.format, BinaryFormat::Elf);
        assert_eq!(info.os, BinaryOs::Linux);
        assert_eq!(info.arch, "x86_64");
        assert_eq!(info.bits, 64);
        assert!(info.little_endian);
    }

    #[test]
    fn detect_macho_is_apple() {
        let data = obj_with_code(WriteFormat::MachO, Architecture::X86_64, &X64_CODE, "f");
        let info = detect_and_describe(&data).unwrap();
        assert_eq!(info.format, BinaryFormat::MachO);
        assert_eq!(info.os, BinaryOs::Apple);
        assert_eq!(info.arch, "x86_64");
    }

    #[test]
    fn classify_maps_each_os_and_format() {
        // The format→OS mapping, including PE→Windows (hand-building a PE that
        // `object` fully accepts is impractical, so the mapping is tested
        // directly; the ELF/Mach-O paths above exercise the full parse).
        assert_eq!(
            os_format_for(FileKind::Pe64),
            Some((BinaryFormat::Pe, BinaryOs::Windows))
        );
        assert_eq!(
            os_format_for(FileKind::Pe32),
            Some((BinaryFormat::Pe, BinaryOs::Windows))
        );
        assert_eq!(
            os_format_for(FileKind::Elf64),
            Some((BinaryFormat::Elf, BinaryOs::Linux))
        );
        assert_eq!(
            os_format_for(FileKind::MachO64),
            Some((BinaryFormat::MachO, BinaryOs::Apple))
        );
        assert_eq!(
            os_format_for(FileKind::MachOFat64),
            Some((BinaryFormat::MachOFat, BinaryOs::Apple))
        );
        // Non-executable kinds are unsupported.
        assert_eq!(os_format_for(FileKind::Archive), None);
        assert_eq!(os_format_for(FileKind::Coff), None);
    }

    #[test]
    fn detect_fat_macho_lists_sub_archs() {
        let x64 = obj_with_code(WriteFormat::MachO, Architecture::X86_64, &X64_CODE, "f");
        let arm = obj_with_code(WriteFormat::MachO, Architecture::Aarch64, &X64_CODE, "f");
        // cputype: x86_64 = 0x01000007, arm64 = 0x0100000C.
        let data = fat_macho(&[(0x0100_0007, &x64), (0x0100_000C, &arm)]);
        let info = detect_and_describe(&data).unwrap();
        assert_eq!(info.format, BinaryFormat::MachOFat);
        assert_eq!(info.os, BinaryOs::Apple);
        assert_eq!(info.sub_archs.len(), 2);
        assert!(info.sub_archs.contains(&"x86_64".to_string()));
        assert!(info.sub_archs.contains(&"aarch64".to_string()));
    }

    #[test]
    fn reject_plain_text() {
        let err = detect_and_describe(b"this is not a binary, just text\n").unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn reject_static_archive() {
        // `!<arch>\n` is the Unix ar magic — recognized but not an executable.
        let err = detect_and_describe(b"!<arch>\n").unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn disassemble_elf_text_decodes_known_bytes() {
        let data = obj_with_code(WriteFormat::Elf, Architecture::X86_64, &X64_CODE, "f");
        // Relocatable .text is mapped at address 0.
        let insns = disassemble_range(&data, 0, X64_CODE.len() as u64).unwrap();
        let mnemonics: Vec<&str> = insns.iter().map(|i| i.mnemonic.as_str()).collect();
        assert_eq!(mnemonics, vec!["push", "mov", "ret"]);
        assert_eq!(insns[0].bytes, "55");
        assert_eq!(insns[0].address, 0);
    }

    #[test]
    fn disassemble_unmapped_address_is_invalid() {
        let data = obj_with_code(WriteFormat::Elf, Architecture::X86_64, &X64_CODE, "f");
        let err = disassemble_range(&data, 0xdead_0000, 4).unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "got {err:?}");
    }

    #[test]
    fn resolve_symbol_exact_and_offset() {
        let data = obj_with_code(WriteFormat::Elf, Architecture::X86_64, &X64_CODE, "myfunc");
        let r = resolve_function(&data, 2).unwrap();
        assert_eq!(r.name.as_deref(), Some("myfunc"));
        assert_eq!(r.symbol_start, Some(0));
        assert_eq!(r.offset, Some(2));
        assert!(r.exact);
    }

    #[test]
    fn capstone_raw_x86_64() {
        let cs = build_capstone(Architecture::X86_64).unwrap();
        let insns = run_disasm(&cs, &X64_CODE, 0x1000).unwrap();
        assert_eq!(insns.len(), 3);
        assert_eq!(insns[0].mnemonic, "push");
        assert_eq!(insns[0].address, 0x1000);
    }

    #[test]
    fn build_capstone_rejects_unsupported_arch() {
        let err = build_capstone(Architecture::Mips).unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "got {err:?}");
    }
}
