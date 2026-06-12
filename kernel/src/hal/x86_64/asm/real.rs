unsafe extern "C" {
    pub fn init_address_space(pml4_phys: u64, stack_address: u64, branch_addr: u64);
    pub fn setup_table(gdt_address: u64, idt_address: u64);
    pub fn jump_to_user_code(user_start_addr: u64, init_rflags: u64, user_stack_base: u64);
    pub fn switch_context_force(context: u64);
}
