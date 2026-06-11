#[repr(C)]
#[derive(Debug)]
pub struct Elf64Ehdr {
    pub e_ident: [u8; 16],     // Magic number and other info
    pub e_type: u16,           // Object file type
    pub e_machine: u16,        // Architecture
    pub e_version: u32,        // Object file version
    pub e_entry: u64,          // Entry point virtual address
    pub e_phoff: u64,          // Program header table file offset
    pub e_shoff: u64,          // Section header table file offset
    pub e_flags: u32,          // Processor-specific flags
    pub e_ehsize: u16,         // ELF header size in bytes
    pub e_phentsize: u16,      // Program header table entry size
    pub e_phnum: u16,          // Program header table entry count
    pub e_shentsize: u16,      // Section header table entry size
    pub e_shnum: u16,          // Section header table entry count
    pub e_shstrndx: u16        // Section header string table index
}

#[repr(C)]
pub struct Elf64Phdr {
    pub p_type: u32,     
    pub p_flags: u32,    
    pub p_offset: u64,   
    pub p_vaddr: u64,    
    pub p_paddr: u64,    
    pub p_filesz: u64,   
    pub p_memsz: u64,    
    pub p_align: u64  
}

#[repr(C)]
pub struct Elf64Shdr {
    pub sh_name:      u32,
    pub sh_type:      u32,
    pub sh_flags:     u64,
    pub sh_addr:      u64,
    pub sh_offset:    u64,
    pub sh_size:      u64,
    pub sh_link:      u32,
    pub sh_info:      u32,
    pub sh_addralign: u64,
    pub sh_entsize:   u64
}

#[repr(C)]
pub struct Elf64Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64
}

#[derive(Debug)]
#[repr(C)]
pub struct ElfDyn {
    pub tag: i64,
    pub val: u64
}

#[derive(Debug)]
#[repr(C)]
pub struct Elf64Sym {
    pub st_name: u32,    
    pub st_info: u8,     
    pub st_other: u8,    
    pub st_shndx: u16,   
    pub st_value: u64,   
    pub st_size: u64,    
}

pub const ELFCLASS64: u8 = 2;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;

// Program header segment permission flags (p_flags)
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 1 << 1;
pub const PF_R: u32 = 1 << 2;

pub const SHT_SYMTAB: u32 = 2;
pub const SHT_STRTAB: u32 = 3;
pub const SHT_RELA: u32 = 4;
pub const SHT_DYNAMIC: u32 = 6;
pub const SHT_DYNSYM: u32 = 11;

pub const SHN_UNDEF: u16 = 0;

pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;

pub const R_X86_64_64: u32 = 1; 
pub const R_X86_64_RELATIVE: u32 = 8;  
pub const R_GLOB_DAT: u32 = 6;
pub const R_JUMP_SLOT: u32 = 7;

pub const DT_NULL:      i64 = 0;
pub const DT_NEEDED:    i64 = 1;
pub const DT_PLTRELSZ:  i64 = 2;
pub const DT_HASH:      i64 = 4;
pub const DT_STRTAB:    i64 = 5;
pub const DT_SYMTAB:    i64 = 6;
pub const DT_RELA:      i64 = 7;
pub const DT_RELASZ:    i64 = 8;
pub const DT_RELAENT:   i64 = 9;
pub const DT_STRSZ:     i64 = 10;
pub const DT_JMPREL:    i64 = 23;
pub const DT_RELACOUNT: i64 = 0x6ffffff9;

// ELF Magic numbers
pub const ELFMAG: u32 = u32::from_le_bytes([0x7F, b'E', b'L', b'F']);