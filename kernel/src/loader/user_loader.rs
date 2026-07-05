use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::mem::size_of;
use common::{PAGE_SIZE, align_down, align_up, elf::*};
use kernel_intf::KError::InvalidArgument;
use kernel_intf::{KError, info};
use kernel_intf::list::{List, DynList};
use kernel_intf::mem::PoolAllocatorGlobal;
use crate::fs::{self, FileBuffer, open};
use crate::hal::copy_user_memory;
use crate::loader::{LoadedImage, PREDEFINED_DIRECTORIES, read_cstr};
use crate::mem::{PageDescriptor, allocate_memory, deallocate_memory, map_memory, unmap_memory};
use crate::sched::get_current_process;
use crate::sync::{KSem, Once, Spinlock, semaphore_guard};
use super::module::{ModuleDescriptor, ModuleType, SharedUserModule, SharedUserModuleRef, UserModule, UserModuleSegment};

pub static USER_MODULES: Spinlock<DynList<Weak<Spinlock<SharedUserModule>, PoolAllocatorGlobal>>> = Spinlock::new(List::new());

static USER_LOAD_LOCK: Once<KSem> = Once::new();

pub(super) fn init() {
    USER_LOAD_LOCK.call_once(|| KSem::new(1, 1));
}

impl Drop for SharedUserModule {
    fn drop(&mut self) {
        {
            let mut registry = USER_MODULES.lock();
            crate::loader_log!("Dropping user image, with registry_len={}", registry.get_nodes());

            // Cleanup: Remove all weak refs from the registry
            while registry.find_and_remove(|entry| {
                let item = entry.upgrade();
                item.is_none()
            }).is_some() {}

            crate::loader_log!("New user registry len = {}", registry.get_nodes());
        }

        // All the virtual memory is destroyed as part of process destruction anyway
        // We just need to remove the backing physical memory
        for region in self.segments.iter() {
            crate::loader_log!("Deallocating segment phy: {:#X} with size:{}", region.phys_addr, region.size);
            deallocate_memory(
                region.phys_addr as *mut u8,
                Layout::from_size_align(region.size, PAGE_SIZE).unwrap(),
                0
            ).expect("Failed to deallocate physical memory for user module!");
        }
    }
}

// Maps caller-owned physical pages into kernel space temporarily so that
// module pages (which are otherwise only user-mapped) can be read without
// SMAP gymnastics. Unmapped on drop
struct TempKernelMapping {
    kva: usize,
    size: usize
}

impl TempKernelMapping {
    fn new(phys_addr: usize, size: usize) -> Result<Self, KError> {
        let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
        let kva = allocate_memory(layout, PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)?.addr();
        map_memory(phys_addr, kva, size, PageDescriptor::VIRTUAL)?;

        Ok(Self { kva, size })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self.kva as *const u8, self.size)
        }
    }
}

impl Drop for TempKernelMapping {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.size, PAGE_SIZE).unwrap();
        unmap_memory(self.kva, self.size, 0)
        .expect("Failed to unmap temporary kernel mapping!");
        deallocate_memory(self.kva as *mut u8, layout, PageDescriptor::VIRTUAL | PageDescriptor::NO_ALLOC)
        .expect("Failed to release temporary kernel mapping VA!");
    }
}

// One PT_LOAD segment, page-aligned into a chunk for mapping/sharing purposes
struct ParsedSegment {
    file_off: usize,
    file_size: usize,
    vaddr: usize,
    mem_size: usize,
    writable: bool,
    chunk_off: usize,
    chunk_size: usize
}

struct DynFileInfo {
    symtab_va: Option<usize>,
    strtab_va: Option<usize>,
    strsz: usize,
    rela_va: Option<usize>,
    relasz: usize,
    relacount: usize,
    plt_va: Option<usize>,
    pltsz: usize,
    num_syms: usize,
    // String table offsets of DT_NEEDED entries
    needed: Vec<usize>,
    // String table offsets of DT_RUNPATH / DT_RPATH, if present
    runpath_off: Option<usize>,
    rpath_off: Option<usize>
}

struct ParsedImage {
    segments: Vec<ParsedSegment>,
    dyn_info: Option<DynFileInfo>,
    // Module relative entry point
    entry: usize,
    // Page-rounded linear span of the image
    total_size: usize
}

impl ParsedImage {
    // Translate a module-relative VA to its file offset (filesz-backed parts only)
    fn va_to_off(&self, va: usize) -> Option<usize> {
        self.segments.iter()
        .find(|s| va >= s.vaddr && va < s.vaddr + s.file_size)
        .map(|s| s.file_off + (va - s.vaddr))
    }

    fn segment_for_va(&self, va: usize) -> Option<&ParsedSegment> {
        self.segments.iter().find(|s| va >= s.vaddr && va < s.vaddr + s.mem_size)
    }
}

fn parse_user_elf(bytes: &[u8]) -> Result<ParsedImage, KError> {
    if bytes.len() < size_of::<Elf64Ehdr>() {
        return Err(InvalidArgument);
    }

    let ehdr = unsafe { &*(bytes.as_ptr() as *const Elf64Ehdr) };
    if ehdr.e_ident[0..4] != [0x7F, b'E', b'L', b'F']
        || ehdr.e_ident[4] != ELFCLASS64
        || ehdr.e_phentsize as usize != size_of::<Elf64Phdr>()
    {
        return Err(InvalidArgument);
    }

    let phdrs = unsafe {
        core::slice::from_raw_parts(bytes.as_ptr().add(ehdr.e_phoff as usize) as *const Elf64Phdr, ehdr.e_phnum as usize)
    };

    let mut segments: Vec<ParsedSegment> = Vec::new();
    let mut dynamic_off = None;
    let mut dynamic_size = 0usize;

    for phdr in phdrs {
        match phdr.p_type {
            PT_LOAD => {
                if phdr.p_align as usize > PAGE_SIZE {
                    info!("User image segment alignment {} not supported", phdr.p_align);
                    return Err(InvalidArgument);
                }

                let vaddr = phdr.p_vaddr as usize;
                let mem_size = phdr.p_memsz as usize;
                let chunk_off = align_down(vaddr, PAGE_SIZE);
                let chunk_size = align_up(vaddr + mem_size, PAGE_SIZE) - chunk_off;

                let file_off = phdr.p_offset as usize;
                let file_size = phdr.p_filesz as usize;
                if file_off + file_size > bytes.len() {
                    return Err(InvalidArgument);
                }

                segments.push(ParsedSegment {
                    file_off, file_size, vaddr, mem_size,
                    writable: phdr.p_flags & PF_W != 0,
                    chunk_off, chunk_size
                });
            },
            PT_DYNAMIC => {
                dynamic_off = Some(phdr.p_offset as usize);
                dynamic_size = phdr.p_filesz as usize;
            },
            _ => {}
        }
    }

    if segments.is_empty() {
        return Err(InvalidArgument);
    }

    segments.sort_by(|a, b| a.chunk_off.cmp(&b.chunk_off));

    // Read-only chunks are shared between processes while write chunks are
    // per process, so a page must never straddle segments of different kinds
    for window in segments.windows(2) {
        if window[0].chunk_off + window[0].chunk_size > window[1].chunk_off {
            info!("User image has overlapping segment pages; rejecting");
            return Err(InvalidArgument);
        }
    }

    let last = segments.last().unwrap();
    let total_size = last.chunk_off + last.chunk_size;

    let img = ParsedImage {
        segments,
        dyn_info: None,
        entry: ehdr.e_entry as usize,
        total_size
    };

    // Parse the dynamic section entries straight from the file
    let dyn_info = if let Some(dyn_off) = dynamic_off {
        if dyn_off + dynamic_size > bytes.len() {
            return Err(InvalidArgument);
        }

        let entries = unsafe {
            core::slice::from_raw_parts(bytes.as_ptr().add(dyn_off) as *const ElfDyn, dynamic_size / size_of::<ElfDyn>())
        };

        let mut info = DynFileInfo {
            symtab_va: None, strtab_va: None, strsz: 0,
            rela_va: None, relasz: 0, relacount: 0,
            plt_va: None, pltsz: 0,
            num_syms: 0, needed: Vec::new(),
            runpath_off: None, rpath_off: None
        };

        let mut hash_va = None;
        for dynent in entries {
            match dynent.tag {
                DT_NULL      => break,
                DT_NEEDED    => info.needed.push(dynent.val as usize),
                DT_SYMTAB    => info.symtab_va = Some(dynent.val as usize),
                DT_STRTAB    => info.strtab_va = Some(dynent.val as usize),
                DT_STRSZ     => info.strsz     = dynent.val as usize,
                DT_RELA      => info.rela_va   = Some(dynent.val as usize),
                DT_RELASZ    => info.relasz    = dynent.val as usize,
                DT_RELACOUNT => info.relacount = dynent.val as usize,
                DT_JMPREL    => info.plt_va   = Some(dynent.val as usize),
                DT_PLTRELSZ  => info.pltsz    = dynent.val as usize,
                DT_RELAENT   => assert_eq!(dynent.val as usize, size_of::<Elf64Rela>()),
                DT_HASH      => hash_va        = Some(dynent.val as usize),
                DT_RUNPATH   => info.runpath_off = Some(dynent.val as usize),
                DT_RPATH     => info.rpath_off    = Some(dynent.val as usize),
                _ => {}
            }
        }

        // Derive symbol count from DT_HASH: nchain (2nd 32-bit word) equals nsyms
        if let Some(h) = hash_va {
            if let Some(hash_off) = img.va_to_off(h) {
                info.num_syms = unsafe {
                    *(bytes.as_ptr().add(hash_off + size_of::<u32>()) as *const u32)
                } as usize;
            }
        }

        Some(info)
    }
    else {
        None
    };

    Ok(ParsedImage { dyn_info, ..img })
}

struct StagedChunk {
    offset: usize,
    writable: bool,
    data: Vec<u8>
}

// Build kernel-side staging buffers for the chunks this load has to populate
// (all of them on a cold load, only the per-process write chunks on warm load)
fn build_staging_chunks(img: &ParsedImage, bytes: &[u8], cold: bool) -> Vec<StagedChunk> {
    img.segments.iter()
    .filter(|s| cold || s.writable)
    .map(|s| {
        let mut data = vec![0u8; s.chunk_size];
        let dst = s.vaddr - s.chunk_off;
        data[dst..dst + s.file_size].copy_from_slice(&bytes[s.file_off..s.file_off + s.file_size]);

        StagedChunk { offset: s.chunk_off, writable: s.writable, data }
    })
    .collect()
}

// Look up an exported symbol in a dependency. The dep's dynsym/dynstr live in
// its potentially another process, so temp-map their physical chunks into
// kernel space for the scan
fn resolve_user_import(name: &str, deps: &[LoadedImage]) -> Option<usize> {
    for dep in deps {
        let guard = dep.lock();
        let dep_module = guard.user();
        let shared = dep_module.shared.lock();

        let (tab_off, str_off) = match (shared.dyn_tab_off, shared.dyn_str_off) {
            (Some(t), Some(s)) => (t, s),
            _ => continue
        };

        if shared.num_syms == 0 {
            continue;
        }

        let tab_map = match map_module_range(&shared, tab_off, shared.num_syms * size_of::<Elf64Sym>()) {
            Some(m) => m,
            None => continue
        };
        let str_map = match map_module_range(&shared, str_off, shared.dyn_str_size) {
            Some(m) => m,
            None => continue
        };

        let syms = unsafe {
            core::slice::from_raw_parts(
                tab_map.0.as_slice().as_ptr().add(tab_map.1) as *const Elf64Sym,
                shared.num_syms
            )
        };

        let str_base = str_map.0.as_slice().as_ptr().addr() + str_map.1;
        for sym in syms {
            if sym.st_shndx == SHN_UNDEF {
                continue;
            }

            let sym_name = unsafe { read_cstr(str_base, sym.st_name as usize) };
            if sym_name == name {
                return Some(dep_module.base + sym.st_value as usize);
            }
        }
    }

    None
}

// Temp-map the chunk that fully contains [module_off, module_off + len) into
// kernel space. Returns the mapping and the offset of module_off within it
fn map_module_range(shared: &SharedUserModule, module_off: usize, len: usize) -> Option<(TempKernelMapping, usize)> {
    let seg = shared.segments.iter()
    .find(|s| module_off >= s.offset && module_off + len <= s.offset + s.size)?;

    let mapping = TempKernelMapping::new(seg.phys_addr, seg.size).ok()?;
    Some((mapping, module_off - seg.offset))
}

fn process_rela_entries(
    img: &ParsedImage,
    bytes: &[u8],
    base: usize,
    chunks: &mut [StagedChunk],
    deps: &[LoadedImage],
    rela_off: usize,
    rela_count: usize,
    symtab_off: Option<usize>,
    strtab_off: Option<usize>
) -> Result<(), KError> {
    let entries = unsafe {
        core::slice::from_raw_parts(bytes.as_ptr().add(rela_off) as *const Elf64Rela, rela_count)
    };

    for entry in entries {
        let target_va = entry.r_offset as usize;
        let rtype = (entry.r_info & 0xffffffff) as u32;

        let seg = img.segment_for_va(target_va).ok_or(InvalidArgument)?;

        if !seg.writable {
            info!("User image relocation targets read-only address {:#X}; rejecting image", target_va);
            return Err(InvalidArgument);
        }

        let value: u64 = match rtype {
            R_X86_64_RELATIVE => (base as i64 + entry.r_addend) as u64,
            R_X86_64_64 | R_GLOB_DAT | R_JUMP_SLOT => {
                let symtab_off = symtab_off.ok_or(InvalidArgument)?;
                let sym_idx = (entry.r_info >> 32) as usize;
                let sym = unsafe {
                    core::ptr::read_unaligned(bytes.as_ptr().add(symtab_off + sym_idx * size_of::<Elf64Sym>()) as *const Elf64Sym)
                };

                let resolved = if sym.st_shndx == SHN_UNDEF {
                    let strtab_off = strtab_off.ok_or(InvalidArgument)?;
                    let name = unsafe { read_cstr(bytes.as_ptr().addr(), strtab_off + sym.st_name as usize) };
                    match resolve_user_import(name, deps) {
                        Some(v) => v,
                        None => {
                            info!("Unresolved user import symbol: {}", name);
                            return Err(InvalidArgument);
                        }
                    }
                }
                else {
                    base + sym.st_value as usize
                };

                if rtype == R_JUMP_SLOT {
                    resolved as u64
                }
                else {
                    (resolved as i64 + entry.r_addend) as u64
                }
            },
            _ => continue
        };

        let chunk = chunks.iter_mut()
        .find(|c| target_va >= c.offset && target_va + size_of::<u64>() <= c.offset + c.data.len())
        .ok_or(InvalidArgument)?;

        let off = target_va - chunk.offset;
        chunk.data[off..off + size_of::<u64>()].copy_from_slice(&value.to_ne_bytes());
    }

    Ok(())
}

fn apply_user_relocations(
    img: &ParsedImage,
    bytes: &[u8],
    base: usize,
    chunks: &mut [StagedChunk],
    deps: &[LoadedImage]
) -> Result<(), KError> {
    let dyn_info = match &img.dyn_info { Some(d) => d, None => return Ok(()) };

    let symtab_off = dyn_info.symtab_va.and_then(|v| img.va_to_off(v));
    let strtab_off = dyn_info.strtab_va.and_then(|v| img.va_to_off(v));

    if let Some(rela_va) = dyn_info.rela_va {
        let rela_off = img.va_to_off(rela_va).ok_or(InvalidArgument)?;
        if rela_off + dyn_info.relasz > bytes.len() {
            return Err(InvalidArgument);
        }
        process_rela_entries(img, bytes, base, chunks, deps, rela_off, dyn_info.relasz / size_of::<Elf64Rela>(), symtab_off, strtab_off)?;
    }

    if let Some(plt_va) = dyn_info.plt_va {
        let plt_off = img.va_to_off(plt_va).ok_or(InvalidArgument)?;
        if plt_off + dyn_info.pltsz > bytes.len() {
            return Err(InvalidArgument);
        }
        process_rela_entries(img, bytes, base, chunks, deps, plt_off, dyn_info.pltsz / size_of::<Elf64Rela>(), symtab_off, strtab_off)?;
    }

    Ok(())
}

// Check if this module has been loaded in some process space
fn find_user_module(path: &str) -> Option<SharedUserModuleRef> {
    let canonical = fs::resolve_symlink(&path).unwrap_or_else(|_| path.to_owned());

    let candidates: Vec<SharedUserModuleRef> = {
        let registry = USER_MODULES.lock();
        registry.iter().filter_map(|node| node.upgrade()).collect()
    };

    for entry in candidates {
        let matches = {
            let guard = entry.lock();
            guard.canonical_file_path == canonical
        };
        if matches {
            return Some(entry);
        }
    }
    None
}

pub fn load_user_image(path: &str) -> Result<LoadedImage, KError> {
    crate::loader_log!("Start load_user_image for {}", path);

    // Serialize the whole recursive load
    let _guard = semaphore_guard(USER_LOAD_LOCK.get().expect("loader::init() not called before load_user_image()"));

    let mut in_progress: Vec<String> = Vec::new();
    load_user_inner(path, &mut in_progress, &[])
}

fn load_user_inner(path: &str, in_progress: &mut Vec<String>, extra_dirs: &[String]) -> Result<LoadedImage, KError> {
    // This is an absolute path
    if path.starts_with("/") {
        return do_load_user_inner(path, in_progress);
    }

    // Check for the file in all the predefined directories, then any
    // caller-supplied extra directories
    for prefix in PREDEFINED_DIRECTORIES.iter().copied().chain(extra_dirs.iter().map(|s| s.as_str())) {
        let filename = format!("{}/{}", prefix, path);
        let res = do_load_user_inner(filename.as_str(), in_progress);

        match res {
            Err(KError::NotFound) | Err(InvalidArgument) => continue,
            other => return other
        }
    }

    // Check cwd
    let filename = fs::make_absolute(&crate::sched::get_cwd(), path);
    let res = do_load_user_inner(filename.as_str(), in_progress);

    if res.is_ok() {
        return res;
    }

    Err(KError::NotFound)
}

fn do_load_user_inner(path: &str, in_progress: &mut Vec<String>) -> Result<LoadedImage, KError> {
    // If this process already mapped this module, hand back the existing
    // mapping. The snapshot is cloned out so that the shared/file locks are
    // taken without holding the process lock
    let snapshot = {
        let proc = get_current_process().expect("load_user_image() called from idle task!");
        let guard = proc.lock();
        guard.get_user_modules()
    };

    let canonical = fs::resolve_symlink(&path).unwrap_or_else(|_| path.to_owned());
    for module in snapshot {
        let (name_matches, base) = {
            let module_guard = module.lock();
            let desc_guard = module_guard.user().shared.lock();
            (desc_guard.canonical_file_path == canonical, module_guard.user().base)
        };

        if name_matches {
            crate::loader_log!("User image {} already mapped in current process at {:#X}", path, base);
            return Ok(module.clone());
        }
    }

    let existing = find_user_module(path);

    if in_progress.iter().any(|n| n == path) {
        return Err(KError::CircularDependency);
    }
    in_progress.push(path.to_owned());

    let result = load_user_into_process(path, existing, in_progress);

    in_progress.pop();
    result
}

// Maps a user module into the current process. Cold load (no shared descriptor
// yet) populates every chunk; warm load maps the shared read-only chunks and
// only re-populates the per-process write chunks from the file
fn load_user_into_process(
    path: &str,
    existing: Option<SharedUserModuleRef>,
    in_progress: &mut Vec<String>
) -> Result<LoadedImage, KError> {
    let cold = existing.is_none();
    crate::loader_log!("Loading user image {} from {}", path, if cold { "disk" } else { "shared cache" });

    let file = open(path)?;
    let file_size = file.len();
    let canonical_file_path = fs::resolve_symlink(&path).expect("File path could not be resolved!");

    let buf = FileBuffer::new(file_size, false)
    .or_else(|e| {
        info!("Failed to allocate filebuffer for user image {}", path);
        Err(e)
    })?;

    let read_len = file.read(&buf).map_err(|e| {
        info!("Read failed for user image {}: {}", path, e);
        e
    })?;
    if read_len != file_size {
        info!("read_len={} doesn't match file_size={} for user image {}", read_len, file_size, path);
        return Err(InvalidArgument);
    }
    let bytes = buf.as_slice();

    let img = parse_user_elf(bytes)?;

    // Reserve the linear VA span for the image in this process. Every process
    // maps the module at whatever address its allocator hands out — only the
    // per-process write chunks carry relocated addresses
    let base = allocate_memory(
        Layout::from_size_align(img.total_size, PAGE_SIZE).unwrap(),
        PageDescriptor::VIRTUAL | PageDescriptor::USER | PageDescriptor::NO_ALLOC
    )?.addr();

    // Load dependencies into this process first; imports resolve against them
    let mut deps: Vec<LoadedImage> = Vec::new();
    if let Some(dyn_info) = &img.dyn_info {
        let strtab_off = dyn_info.strtab_va.and_then(|v| img.va_to_off(v));

        // This module's own DT_RUNPATH/DT_RPATH applies when resolving its DT_NEEDED entries
        let extra_dirs: Vec<String> = match (dyn_info.runpath_off.or(dyn_info.rpath_off), strtab_off) {
            (Some(off), Some(strtab_off)) => {
                let raw = unsafe { read_cstr(bytes.as_ptr().addr(), strtab_off + off) };
                raw.split(':').filter(|s| !s.is_empty()).map(|s| s.to_owned()).collect()
            },
            _ => Vec::new()
        };

        for &needed_off in dyn_info.needed.iter() {
            let strtab_off = strtab_off.ok_or(InvalidArgument)?;
            let name = unsafe { read_cstr(bytes.as_ptr().addr(), strtab_off + needed_off) };

            crate::loader_log!("Loading user dependency {}", name);
            deps.push(load_user_inner(&name, in_progress, &extra_dirs)?);
        }
    }

    let mut chunks = build_staging_chunks(&img, bytes, cold);
    apply_user_relocations(&img, bytes, base, &mut chunks, &deps)?;

    // Warm load: map the shared read-only chunks into this process
    if let Some(shared) = &existing {
        let guard = shared.lock();
        for seg in guard.segments.iter().filter(|s| !s.writable) {
            map_memory(seg.phys_addr, base + seg.offset, seg.size, PageDescriptor::VIRTUAL | PageDescriptor::USER)?;
        }
    }

    // Commit the staged chunks: fresh physical pages, mapped into this process
    // and filled from staging.
    let mut seg_meta: Vec<UserModuleSegment> = Vec::new();
    for chunk in chunks.iter().filter(|p| {existing.is_none() || p.writable})  {
        let layout = Layout::from_size_align(chunk.data.len(), PAGE_SIZE).unwrap();
        let phys = allocate_memory(layout, 0)?.addr();

        map_memory(phys, base + chunk.offset, chunk.data.len(), PageDescriptor::VIRTUAL | PageDescriptor::USER)?;
        unsafe {
            copy_user_memory((base + chunk.offset) as *mut u8, chunk.data.as_ptr(), chunk.data.len());
        }

        // Track both writable segments (which is unique per process)
        // and shared readable segments
        seg_meta.push(UserModuleSegment {
            phys_addr: phys,
            offset: chunk.offset,
            size: chunk.data.len(),
            writable: chunk.writable
        });
    }

    let (shared, entry_offset) = match existing {
        Some(shared) => {
            let entry_offset = shared.lock().entry_offset;
            (shared, entry_offset)
        },
        None => {
            let dyn_info = img.dyn_info.as_ref();
            let shared = Arc::new_in(
                Spinlock::new(SharedUserModule {
                    canonical_file_path,
                    segments: seg_meta,
                    total_size: img.total_size,
                    entry_offset: img.entry,
                    dyn_tab_off: dyn_info.and_then(|d| d.symtab_va),
                    num_syms: dyn_info.map(|d| d.num_syms).unwrap_or(0),
                    dyn_str_off: dyn_info.and_then(|d| d.strtab_va),
                    dyn_str_size: dyn_info.map(|d| d.strsz).unwrap_or(0),
                    _deps: deps.iter().map(|d| d.lock().user().shared.clone()).collect()
                }),
                PoolAllocatorGlobal
            );

            USER_MODULES.lock().add_node(Arc::downgrade(&shared))
            .expect("Failed to add user image to module registry");

            (shared, img.entry)
        }
    };

    let new_img = Arc::new_in(
        Spinlock::new(
            ModuleDescriptor {
                mod_type: ModuleType::User(UserModule {
                    base,
                    entry: base + entry_offset,
                    shared
                })
            }),
            PoolAllocatorGlobal
        );

    // Track the mapping in the per-process registry; the strong shared Arc
    // stored there keeps the module alive for this process's lifetime
    let proc = get_current_process().expect("load_user_image() called from idle task!");
    proc.lock().register_user_module(&new_img);

    crate::loader_log!("Loaded user image '{}' at base={:#X} entry={:#X}", path, base, base + entry_offset);

    Ok(new_img)
}
