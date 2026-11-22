//! Minimal ELF64 loader for static user programs.
//!
//! This is not a dynamic loader: it accepts statically linked `ET_EXEC` and
//! `ET_DYN` x86-64 binaries with `PT_LOAD` segments, maps them into a private
//! [`AddressSpace`], and returns the entry point and initial stack. PIE binaries
//! are relocated by a load bias chosen so the first segment starts at
//! [`ELF_IMAGE_BASE`], well above the legacy flat-binary window. PIE guests may
//! also carry `R_X86_64_RELATIVE` relocations, which are applied after the
//! segments are mapped.
//!
//! No ELF parser crate is used; the on-disk structures are defined here so the
//! kernel stays dependency-free and the format is auditable in one place.

use crate::address_space::AddressSpace;
use core::mem::size_of;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const EM_X86_64: u16 = 62;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PF_W: u32 = 0x2;
const PF_X: u32 = 0x1;

const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const R_X86_64_NONE: u32 = 0;
const R_X86_64_RELATIVE: u32 = 8;
const PAGE_BYTES: u64 = 4096;

/// Where the first ELF segment is placed inside the user half.
const ELF_IMAGE_BASE: u64 = crate::usermode::USER_BASE + 0x0010_0000;
/// Top of the user region; stack pages are placed just below it.
const USER_REGION_END: u64 = crate::usermode::USER_BASE + 0x0100_0000;
/// User stack reserved for an ELF process (64 KiB).
const USER_STACK_BYTES: u64 = 0x0001_0000;

// The field names mirror the ELF64 specification on purpose.
#[allow(clippy::struct_field_names)]
#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[allow(clippy::struct_field_names)]
#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

#[allow(clippy::struct_field_names)]
#[repr(C)]
#[derive(Clone, Copy)]
struct Elf64Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

/// The result of loading an ELF image.
#[derive(Clone, Copy, Debug)]
pub struct LoadedElf {
    /// The address the kernel enters with `enter_user_mode`.
    pub entry: u64,
    /// The initial user stack pointer.
    pub stack_top: u64,
    /// The first address available to `brk`.
    pub brk_start: u64,
}

fn read_unaligned<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(size_of::<T>())?;
    if end > bytes.len() {
        return None;
    }
    // SAFETY: the range was checked and the read is unaligned, so no alignment
    // invariant is required of the input slice.
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}

const fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    align_down(value + alignment - 1, alignment)
}

fn parse_header(bytes: &[u8]) -> Result<Elf64Ehdr, &'static str> {
    if bytes.len() < size_of::<Elf64Ehdr>()
        || bytes.get(..4) != Some(&[0x7f, b'E', b'L', b'F'])
        || bytes.get(4) != Some(&ELFCLASS64)
        || bytes.get(5) != Some(&ELFDATA2LSB)
        || bytes.get(6) != Some(&EV_CURRENT)
    {
        return Err("not a little-endian ELF64 file");
    }

    let header: Elf64Ehdr = read_unaligned(bytes, 0).ok_or("truncated ELF header")?;
    if !matches!(header.e_type, ET_EXEC | ET_DYN)
        || header.e_machine != EM_X86_64
        || header.e_version != EV_CURRENT as u32
        || header.e_ehsize < size_of::<Elf64Ehdr>() as u16
        || header.e_phentsize < size_of::<Elf64Phdr>() as u16
        || header.e_phnum == 0
    {
        return Err("unsupported ELF header");
    }
    Ok(header)
}

fn program_header(
    bytes: &[u8],
    phoff: usize,
    phentsize: usize,
    index: u16,
) -> Result<Elf64Phdr, &'static str> {
    let offset = phoff
        .checked_add(phentsize * usize::from(index))
        .ok_or("program header overflow")?;
    read_unaligned(bytes, offset).ok_or("truncated program header")
}

fn load_bias(bytes: &[u8], header: &Elf64Ehdr, phoff: usize) -> Result<u64, &'static str> {
    let mut min_vaddr = u64::MAX;
    for index in 0..header.e_phnum {
        let ph = program_header(bytes, phoff, header.e_phentsize.into(), index)?;
        if ph.p_type == PT_LOAD && ph.p_memsz > 0 {
            min_vaddr = min_vaddr.min(align_down(ph.p_vaddr, PAGE_BYTES));
        }
    }
    if min_vaddr == u64::MAX {
        return Err("ELF has no loadable segments");
    }
    ELF_IMAGE_BASE
        .checked_sub(min_vaddr)
        .ok_or("ELF image does not fit the user slot")
}

fn map_segments(
    bytes: &[u8],
    space: &mut AddressSpace,
    header: &Elf64Ehdr,
    phoff: usize,
    bias: u64,
) -> Result<u64, &'static str> {
    let mut max_end = ELF_IMAGE_BASE;
    for index in 0..header.e_phnum {
        let ph = program_header(bytes, phoff, header.e_phentsize.into(), index)?;
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }
        if ph.p_filesz > ph.p_memsz {
            return Err("ELF segment has filesz larger than memsz");
        }
        let file_end = ph
            .p_offset
            .checked_add(ph.p_filesz)
            .ok_or("ELF segment file range overflow")?;
        if file_end > bytes.len() as u64 {
            return Err("ELF segment extends past end of file");
        }

        let start = ph.p_vaddr.checked_add(bias).ok_or("ELF segment overflow")?;
        let file_bytes_end = start
            .checked_add(ph.p_filesz)
            .ok_or("ELF segment overflow")?;
        let end = start
            .checked_add(ph.p_memsz)
            .ok_or("ELF segment overflow")?;
        if start < ELF_IMAGE_BASE || end > crate::usermode::USER_BRK_END {
            return Err("ELF segment outside user region");
        }
        max_end = max_end.max(align_up(end, PAGE_BYTES));

        let writable = ph.p_flags & PF_W != 0;
        let executable = ph.p_flags & PF_X != 0;
        let mut page = align_down(start, PAGE_BYTES);
        let last_page = align_up(end, PAGE_BYTES);
        while page < last_page {
            let frame = space.map_user_page(page, writable, executable)?;
            let copy_start = page.max(start);
            let copy_end = page.saturating_add(PAGE_BYTES).min(file_bytes_end);
            if copy_start < copy_end {
                let file_offset = (ph.p_offset + (copy_start - start)) as usize;
                let count = (copy_end - copy_start) as usize;
                let destination = (frame + (copy_start - page)) as *mut u8;
                // SAFETY: the source range lies in `bytes`, and the destination
                // lies in a freshly mapped 4 KiB user frame owned by `space`.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(file_offset),
                        destination,
                        count,
                    );
                }
            }
            page += PAGE_BYTES;
        }
    }
    Ok(max_end)
}

/// Applies the `R_X86_64_RELATIVE` relocations a position-independent guest may
/// carry in its dynamic section.
///
/// The guest is loaded at an arbitrary slot, so pointer-sized values in `.got`
/// and `.data.rel.ro` still contain link-time addresses. Without this pass a
/// call through the GOT jumps to address zero, which is invisible until the
/// function is actually used.
fn apply_relocations(
    bytes: &[u8],
    space: &mut AddressSpace,
    header: &Elf64Ehdr,
    phoff: usize,
    bias: u64,
) -> Result<(), &'static str> {
    let dynamic = (0..header.e_phnum).find_map(|index| {
        let ph = program_header(bytes, phoff, header.e_phentsize.into(), index).ok()?;
        (ph.p_type == PT_DYNAMIC).then_some(ph)
    });
    let Some(dynamic) = dynamic else {
        return Ok(());
    };

    let dyn_offset = usize::try_from(dynamic.p_offset).map_err(|_| "bad dynamic offset")?;
    let dyn_size = usize::try_from(dynamic.p_filesz).map_err(|_| "bad dynamic size")?;
    if dyn_size % size_of::<Elf64Dyn>() != 0 {
        return Err("truncated dynamic section");
    }
    let dyn_end = dyn_offset
        .checked_add(dyn_size)
        .ok_or("dynamic section overflow")?;
    if dyn_end > bytes.len() {
        return Err("dynamic section outside ELF file");
    }

    let mut rela_offset = None;
    let mut rela_size = None;
    let mut rela_entry_size = None;
    for index in 0..dyn_size / size_of::<Elf64Dyn>() {
        let entry: Elf64Dyn = read_unaligned(bytes, dyn_offset + index * size_of::<Elf64Dyn>())
            .ok_or("truncated dynamic entry")?;
        match entry.d_tag {
            DT_NULL => break,
            DT_RELA => rela_offset = Some(entry.d_val),
            DT_RELASZ => rela_size = Some(entry.d_val),
            DT_RELAENT => rela_entry_size = Some(entry.d_val),
            _ => {}
        }
    }
    let (Some(rela_offset), Some(rela_size)) = (rela_offset, rela_size) else {
        return Ok(());
    };

    let entry_size =
        usize::try_from(rela_entry_size.unwrap_or(24)).map_err(|_| "bad RELA entry size")?;
    if entry_size < size_of::<Elf64Rela>() {
        return Err("RELAs are too small");
    }
    let rela_size = usize::try_from(rela_size).map_err(|_| "bad RELA size")?;
    if rela_size % entry_size != 0 {
        return Err("truncated RELA table");
    }
    let rela_offset = usize::try_from(rela_offset).map_err(|_| "bad RELA offset")?;
    let rela_table_end = rela_offset
        .checked_add(rela_size)
        .ok_or("RELA table overflow")?;
    if rela_table_end > bytes.len() {
        return Err("RELA table outside ELF file");
    }

    for offset in (0..rela_size).step_by(entry_size) {
        let rela: Elf64Rela =
            read_unaligned(bytes, rela_offset + offset).ok_or("truncated RELA entry")?;
        let kind = (rela.r_info & 0xffff_ffff) as u32;
        if kind == R_X86_64_NONE {
            continue;
        }
        if kind != R_X86_64_RELATIVE {
            return Err("unsupported ELF relocation");
        }
        let target = rela
            .r_offset
            .checked_add(bias)
            .ok_or("relocation overflow")?;
        let value = bias
            .checked_add_signed(rela.r_addend)
            .ok_or("relocation value overflow")?;
        let page = target & !(PAGE_BYTES - 1);
        let page_offset = target - page;
        if page_offset + size_of::<u64>() as u64 > PAGE_BYTES {
            return Err("relocation crosses a page boundary");
        }
        let frame = space
            .mapped_frame_address(page)
            .ok_or("relocation outside loaded segments")?;
        // SAFETY: `frame` backs a mapped user page, the offset stays inside that
        // page, and a RELATIVE relocation writes one pointer-sized value.
        unsafe { core::ptr::write_unaligned((frame + page_offset) as *mut u64, value) };
    }
    Ok(())
}

fn map_stack(space: &mut AddressSpace) -> Result<(), &'static str> {
    let mut page = USER_REGION_END - USER_STACK_BYTES;
    while page < USER_REGION_END {
        space.map_user_page(page, true, false)?;
        page += PAGE_BYTES;
    }
    Ok(())
}

/// Loads a static ELF64 image into `space`.
///
/// Returns an error if the header is malformed, the binary is for another
/// architecture, or a segment does not fit in the user region.
pub fn load(bytes: &[u8], space: &mut AddressSpace) -> Result<LoadedElf, &'static str> {
    let header = parse_header(bytes)?;
    let phoff = usize::try_from(header.e_phoff).map_err(|_| "bad program header offset")?;
    let bias = load_bias(bytes, &header, phoff)?;
    let max_end = map_segments(bytes, space, &header, phoff, bias)?;
    apply_relocations(bytes, space, &header, phoff, bias)?;
    map_stack(space)?;

    Ok(LoadedElf {
        entry: header
            .e_entry
            .checked_add(bias)
            .ok_or("ELF entry overflow")?,
        stack_top: USER_REGION_END,
        brk_start: align_up(max_end, PAGE_BYTES),
    })
}
