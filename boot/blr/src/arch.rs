extern crate alloc;

use common::{*, elf::*};
use log::*;
use alloc::vec::Vec;
use core::mem::size_of;
use core::ptr::copy_nonoverlapping;
use core::alloc::Layout;


#[derive(Debug, Clone)]
struct MapRegion {
    src_addr: usize,
    dest_addr: usize,
    src_size: usize,
    dest_size: usize
}

impl MapRegion {
    pub fn create_array_rgn(base: *const u8, offset: u64, size: u64, entry_size: u64) -> Self {
        Self {
            src_addr: unsafe {
                base.add(offset as usize) as usize
            },
            dest_addr: 0, src_size: size as usize, dest_size: entry_size as usize
        }
    }
    
    pub fn create_map_rgn(base: *const u8, offset: u64, size: u64) -> Self {
        Self {
            src_addr: unsafe {
                base.add(offset as usize) as usize
            },
            dest_addr: 0, src_size: size as usize, dest_size: size as usize
        }
    }
}

pub struct DynamicInfo {
    pub symtab: Option<MapRegion>,
    pub strtab: Option<MapRegion>,
    pub rela: Option<MapRegion>,
    pub relacount: usize
}

unsafe extern "Rust" {
    fn loader_alloc(layout: Layout) -> *mut u8;
}

#[cfg(target_arch="x86_64")]
pub fn canonicalize(address: usize) -> u64 {
    let mut addr = address as u64;
    if addr & (1 << 47) != 0 {
        addr |= (0xffff as u64) << 48;
    }
    else {
        addr &= !((0xffff as u64) << 48);
    }

    addr
}

fn load_aux_tables(symtab: &mut (MapRegion, MapRegion), aux_base: usize, aux_alignment: usize) {
    // Now map the symbol table
    test_log!("Loading symbol table");
    let mut current_load_ptr = aux_base as *mut u8;
    for (_idx, shn) in [&mut symtab.0, &mut symtab.1].iter_mut().enumerate() {
        unsafe {
            copy_nonoverlapping(shn.src_addr as *const u8, current_load_ptr, shn.src_size);
            shn.dest_addr = current_load_ptr as usize;
            test_log!("Loaded location:{} from {:#X} to {:#X} of size: {}", _idx, shn.src_addr, current_load_ptr as usize, shn.src_size);
            
            current_load_ptr = current_load_ptr.add(shn.src_size);
            current_load_ptr = current_load_ptr.add(current_load_ptr.align_offset(aux_alignment));
        }
    }
}

fn parse_dynamic_section(load_base: *const u8, dynamic: &MapRegion) -> DynamicInfo {
    let nentries = dynamic.dest_size / core::mem::size_of::<ElfDyn>();
    let dynamic = unsafe {
        let dynamic_shn_base = load_base.add(dynamic.dest_addr) as *const ElfDyn;
        core::slice::from_raw_parts(dynamic_shn_base, nentries)
    };

    let mut info = DynamicInfo {
        symtab: None,
        strtab: None,

        rela: None,
        relacount: 0
    };

    let mut rela_size = 0usize;
    let mut rela_ent_size = core::mem::size_of::<Elf64Rela>();

    let mut strsz = 0usize;
    let mut hash_vma: Option<usize> = None;

    for dynent in dynamic {
        match dynent.tag {
            DT_NULL => break,
            DT_SYMTAB => {
                info.symtab = Some(MapRegion {
                    dest_addr: dynent.val as usize,
                    src_addr: 0,
                    src_size: 0,
                    dest_size: core::mem::size_of::<Elf64Sym>()
                });
            },
            DT_STRTAB => {
                info.strtab = Some(MapRegion {
                    dest_addr: dynent.val as usize,
                    src_addr: 0,
                    src_size: 0,
                    dest_size: 0
                });
            },
            DT_STRSZ => {
                strsz = dynent.val as usize;
            },
            DT_RELA => {
                info.rela = Some(MapRegion {
                    dest_addr: dynent.val as usize,
                    src_addr: 0,
                    src_size: 0,
                    dest_size: core::mem::size_of::<Elf64Rela>(),
                });
            },
            DT_RELASZ => {
                rela_size = dynent.val as usize;
            },
            DT_RELAENT => {
                rela_ent_size = dynent.val as usize;
                assert!(rela_ent_size == core::mem::size_of::<Elf64Rela>());
            },
            DT_HASH => {
                hash_vma = Some(dynent.val as usize);
            },
            _ => {}
        }
    }

    if let Some(rela) = &mut info.rela {
        rela.src_size = rela_size;
        rela.dest_size = rela_ent_size;
    }

    if let Some(strtab) = &mut info.strtab {
        strtab.src_size = strsz;
        strtab.dest_size = strsz;
    }

    // Derive dynamic symbol table size from DT_HASH: the second 32-bit word
    // (nchain) holds the number of symbols.
    if let (Some(symtab), Some(hash)) = (info.symtab.as_mut(), hash_vma) {
        unsafe {
            let nchain = *((load_base.add(hash)) as *const u32).add(1);
            symtab.src_size = nchain as usize * core::mem::size_of::<Elf64Sym>();
        }
    }

    // Instead of iterating 3 different tables, we'll just go over 3 types of entries in 1 table
    info.relacount = rela_size / core::mem::size_of::<Elf64Rela>();

    info
}


fn apply_relocation(load_base: usize, kernel_size: usize, dynamic_shn: &DynamicInfo) {
    let info = |bitmap: u64| {
        (bitmap & 0xffffffff) as u32
    };
    
    let reloc_shn = dynamic_shn.rela.as_ref().and_then(|f| {
        unsafe {
            let base = (f.dest_addr as *const u8).add(load_base) as *const Elf64Rela;
            Some(core::slice::from_raw_parts(base, dynamic_shn.relacount))
        }
    });

    let dyn_tab = dynamic_shn.symtab.as_ref().and_then(|f| {
        assert!(f.dest_size == size_of::<Elf64Sym>());
        unsafe {
            Some((f.dest_addr as *const u8).add(load_base) as *const Elf64Sym)
        }
    });

    let mut rel_relocations = 0;
    let mut jmp_relocations = 0;
    let mut glob_relocations = 0;

    if let Some(entries) = reloc_shn {
        // Here, we are assuming that linker assigned base address of elf as 0
        for entry in entries {
            let address = load_base + entry.r_offset as usize;
            match info(entry.r_info) {
                R_X86_64_RELATIVE => {
                    let value = load_base as i64 + entry.r_addend;
                    assert!(address < load_base + kernel_size);
                    unsafe {
                        *(address as *mut u64) = value as u64;
                    }

                    rel_relocations += 1;
                },
                R_X86_64_64 | R_GLOB_DAT => {
                    assert!(dyn_tab.is_some());

                    let sym_idx = (entry.r_info >> 32) as usize;
                    let sym = unsafe {
                        &*dyn_tab.as_ref().unwrap().add(sym_idx)
                    };

                    #[cfg(not(test))]
                    assert!(sym.st_shndx != SHN_UNDEF, "Undefined symbol in relocation");

                    let value = load_base + sym.st_value as usize + entry.r_addend as usize;

                    unsafe {
                        *(address as *mut u64) = value as u64;
                    }

                    glob_relocations += 1;
                },
                R_JUMP_SLOT => {
                    assert!(dyn_tab.is_some());
                    let sym_idx = (entry.r_info >> 32) as usize;
                    let sym = unsafe {
                        &*dyn_tab.as_ref().unwrap().add(sym_idx)
                    };

                    #[cfg(not(test))] 
                    if sym.st_shndx == SHN_UNDEF {
                        panic!("Undefined symbol found during relocation");
                    }

                    let value = load_base + sym.st_value as usize;

                    unsafe {
                        *(address as *mut u64) = value as u64;
                    }
                    jmp_relocations += 1;
                },
                _=> {}
            }
        }
    }
    debug!("Relative relocations = {}, dynamic relocations = {}, global relocations = {}", rel_relocations, jmp_relocations, glob_relocations);
}

#[cfg(debug_assertions)]
pub fn print_exported_symbols(dynsym: &Option<ArrayTable>, dynstr: &Option<MemoryRegion>) {
    if dynsym.is_none() {
        return;
    }

    let tab = dynsym.as_ref().unwrap();
    let str_tab = dynstr.as_ref().unwrap();

    let stringizer = |str_idx: usize| {
        use core::ffi::CStr;

        let str_base = unsafe {
            (str_tab.base_address as *const u8).add(str_idx)
        };

        unsafe {
            CStr::from_ptr(str_base as *const i8).to_str().unwrap()
        }
    };

    let entries = unsafe {
        core::slice::from_raw_parts(tab.start as *const Elf64Sym, tab.size / tab.entry_size)
    };

    debug!("====Printing kernel exported symbols====");
    for entry in entries {
        let name = stringizer(entry.st_name as usize);
        if !name.trim().is_empty() {
            debug!("Address={:#X}->{}", entry.st_value, name);
        }
    }
}  


#[cfg(target_arch="x86_64")]
pub fn load_kernel_arch(kernel_base: *const u8, hdr: &Elf64Ehdr) -> ModuleInfo {
    assert_eq!(hdr.e_ident[4], ELFCLASS64, "x86_64 arch requires kernel elf file to be of 64 bit type!");
    debug!("Found 64 bit kernel elf header");

    assert_eq!(hdr.e_phentsize, size_of::<Elf64Phdr>() as u16);
    assert_eq!(hdr.e_shentsize, size_of::<Elf64Shdr>() as u16);

    let prog_base = unsafe {
        kernel_base.add(hdr.e_phoff as usize)
    } as *const Elf64Phdr;
    
    let shn_base = unsafe {
        kernel_base.add(hdr.e_shoff as usize)
    } as *const Elf64Shdr;

    let prog_hdrs = unsafe {
        core::slice::from_raw_parts(prog_base, hdr.e_phnum as usize)
    };

    let shn_hdrs = unsafe {
        core::slice::from_raw_parts(shn_base, hdr.e_shnum as usize)
    };

    assert!(prog_hdrs.len() != 0 && shn_hdrs.len() != 0, "No program or section header found in kernel elf file");
    assert!((hdr.e_shstrndx < hdr.e_shnum) && (shn_hdrs[hdr.e_shstrndx as usize].sh_type == SHT_STRTAB), "No string table in elf file!");
    let mut map_regions_list = Vec::new();

    let mut loadable_segments = 0;
    let mut dyn_shn = None;
    let mut max_alignment: usize = 0;

    test_log!("Printing loadable segment descriptors");

    // Get information on all loadable segments (.text, .rodata etc)
    for prog_hdr in prog_hdrs.iter().filter(|entry| {
        entry.p_type == PT_LOAD || entry.p_type == PT_DYNAMIC
    }) {
            map_regions_list.push(MapRegion {src_addr: unsafe {
                kernel_base.add(prog_hdr.p_offset as usize) as usize
            },
            dest_addr: prog_hdr.p_vaddr as usize, src_size: prog_hdr.p_filesz as usize, dest_size: prog_hdr.p_memsz as usize 
            });

            test_log!("src: {:#X}, dest: {:#X}, src-size: {}, dest-size: {}, aligment: {}", map_regions_list.last().unwrap().src_addr,
            map_regions_list.last().unwrap().dest_addr, map_regions_list.last().unwrap().src_size, 
            map_regions_list.last().unwrap().dest_size, prog_hdr.p_align); 

            if prog_hdr.p_align != 0 && prog_hdr.p_align != 1 {
                max_alignment = max_alignment.max(prog_hdr.p_align as usize);
            }

        #[cfg(test)]
            assert!(map_regions_list.last().unwrap().dest_addr % prog_hdr.p_align as usize == 0, "Provided virtual address does not satisfy alignment constraint");

            loadable_segments += 1;
            if prog_hdr.p_type == PT_DYNAMIC {
                dyn_shn = Some(map_regions_list.last().unwrap().clone());
            }
    }

    // For symbol table and reloc section, dest_size is reinterpreted as per-entry size
    let mut aux_alignment: usize = 1;
    let mut aux_size: usize = 0;

    // Check if symbol table is present and load it to memory
    let mut symtables = shn_hdrs.iter().find(|entry| {
        entry.sh_type == SHT_SYMTAB
    }).and_then(|entry| {
        let reg = MapRegion::create_array_rgn(kernel_base, entry.sh_offset, entry.sh_size, entry.sh_entsize);
        let str_shn = &shn_hdrs[entry.sh_link as usize];

        assert_eq!(str_shn.sh_type, SHT_STRTAB);
        let symstr = MapRegion::create_map_rgn(kernel_base, str_shn.sh_offset, str_shn.sh_size);
        aux_alignment = aux_alignment.max(entry.sh_addralign as usize);

        let symtab_bytes = entry.sh_size as usize;
        let symstr_bytes = str_shn.sh_size as usize;
        let pad = (symtab_bytes as *const u8).align_offset(aux_alignment);
        aux_size = symtab_bytes + pad + symstr_bytes;

        Some((reg, symstr))
    });
    
    debug!("Loadable segments: {}, Dynamic segment present: {}, Symbol table present: {}, max_alignment: {}, aux_alignment: {}", loadable_segments, 
    dyn_shn.is_some(), symtables.is_some(), max_alignment, aux_alignment);

    // Need the kernel code + data regions to be in sorted order of their dest addr as upcoming logic depends on it
    map_regions_list.sort_by(|a, b| {
        a.dest_addr.cmp(&b.dest_addr)
    });

    // Layout info
    // 1st we load all the binary code + data regions
    // 2nd we load the auxiliary tables (symtab, string shn)

    let last_entry =  map_regions_list.last().unwrap();
    let main_shn_size = last_entry.dest_addr + last_entry.dest_size;
    let aux_padding = (main_shn_size as *const u8).align_offset(aux_alignment);
    let aux_shn_end = aux_padding + main_shn_size + aux_size; 
    let total_module_size = aux_shn_end; 
    
    let layout = Layout::from_size_align(total_module_size, max_alignment.max(aux_alignment)).unwrap();
    let load_base = unsafe {
        loader_alloc(layout)
    };

    debug!("Loading kernel regions at load_base: {:#X}", load_base as usize);
    // Now, map all loadable regions to appropriate locations
    for (idx, entry) in map_regions_list.iter().enumerate() {
        unsafe {
            let current_load_ptr = load_base.add(entry.dest_addr);

            // First, zero fill the memory region (Some regions have dest_size > src_size, so remaining part (dest_size - src_size) must be zeroed)
            current_load_ptr.write_bytes(0, entry.dest_size);
            
            copy_nonoverlapping(entry.src_addr as *const u8, current_load_ptr, entry.src_size);
            debug!("Loaded location:{} from {:#X} of va:{:#X} to {:#X}", idx, entry.src_addr, entry.dest_addr, current_load_ptr as usize);
        }
    }
    
    if symtables.is_some() {
        load_aux_tables(symtables.as_mut().unwrap(), load_base as usize + main_shn_size + aux_padding, aux_alignment);
    }
    
    // Now fetch info on reloc shn, dyn shn and plt shns
    let mut dyn_shn_info = None;
    if dyn_shn.is_some() {
        dyn_shn_info = Some(parse_dynamic_section(load_base, dyn_shn.as_ref().unwrap()));
        apply_relocation(load_base as usize, main_shn_size, dyn_shn_info.as_ref().unwrap());
    }

    // Fill up all output information
    let (sym_tab_out, sym_tab_str) = if let Some((sym, symstr)) = symtables {
        (Some(ArrayTable {start: sym.dest_addr, size: sym.src_size, entry_size: sym.dest_size}), 
        Some(MemoryRegion{base_address: symstr.dest_addr, size: symstr.src_size}))
    }
    else {
        (None, None)
    };
    
    let dyn_sym_tab_out = dyn_shn_info.as_ref().map(|f| {
        f.symtab.as_ref().map(|sym| {
            ArrayTable {start: load_base as usize + sym.dest_addr, size: sym.src_size, entry_size: sym.dest_size}
        })
    }).flatten();

    let dyn_str_out = dyn_shn_info.as_ref().map(|f| {
        f.strtab.as_ref().map(|sym| {
            MemoryRegion {base_address: load_base as usize + sym.dest_addr, size: sym.src_size}
        })
    }).flatten();

    let dyn_shn_out = dyn_shn.map(|sym|
        ArrayTable {start: load_base as usize + sym.dest_addr, size: sym.src_size, entry_size: size_of::<ElfDyn>()}
    );

    let reloc_shn_out = dyn_shn_info.as_ref().map(|f| {
        f.rela.as_ref().map(|reloc| {
            ArrayTable {start: load_base as usize + reloc.dest_addr, size: reloc.src_size, entry_size: reloc.dest_size }
        })
    }).flatten();

#[cfg(debug_assertions)]
    print_exported_symbols(&dyn_sym_tab_out, &dyn_str_out);

    ModuleInfo {
        entry: hdr.e_entry as usize + load_base as usize,
        base: load_base as usize,
        size: main_shn_size,
        total_size: total_module_size, 
        sym_tab: sym_tab_out,
        sym_str: sym_tab_str,
        dyn_tab: dyn_sym_tab_out,
        dyn_str: dyn_str_out,
        rlc_shn: reloc_shn_out,
        dyn_shn: dyn_shn_out
    }
}