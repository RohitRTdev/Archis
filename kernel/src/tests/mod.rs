use core::{alloc::Layout, ptr::NonNull};
use std::{sync::{Arc, Mutex, OnceLock}};

use crate::mem::{self, FixedList};
use kernel_intf::list::List;
use kernel_intf::KError;
use common::test_log;

tests::init_test_logger!(aris);

#[derive(Debug)]
struct Sample {
    _a: i32,
    _b: u32
}

static TEST_LOCK: OnceLock<Arc<Mutex<bool>>> = OnceLock::new();

fn get_test_lock() -> &'static Arc<Mutex<bool>> {
    TEST_LOCK.get_or_init(|| Arc::new(Mutex::new(false)))
}


#[test]
fn list_clear_test() {
    let _guard = get_test_lock().lock().unwrap();
    mem::clear_heap();
    mem::setup_heap();

    test_log!("Starting list_clear_test");
    let mut structure: FixedList<Sample, {mem::Regions::Region0 as usize}> = List::new();

    #[derive(Debug, Clone)]
    struct TestStruct {
        _a: u32,
        _b: u32
    }

    #[derive(Debug, Clone)]
    struct Sample {
        list: FixedList<TestStruct, {mem::Regions::Region1 as usize}>,
        _a: u32,
        _b: u32
    }

    impl Sample {
        fn new(_a: u32, _b: u32) -> Self {
            let mut inst = Self {
                list: List::new(),
                _a,
                _b
            };
            
            inst.list.add_node(TestStruct{_a: _a, _b: _b}).unwrap();
            inst.list.add_node(TestStruct{_a: _a + 1, _b: _b + 1}).unwrap();
            inst
        }
    }

    impl Drop for TestStruct {
        fn drop(&mut self) {
            test_log!("Dropping {:?}", self);
        }
    }

    impl Drop for Sample {
        fn drop(&mut self) {
            test_log!("Dropping {:?}", self);
        }
    }

    structure.add_node(Sample::new(52, 12)).unwrap();
    structure.add_node(Sample::new(32, 13)).unwrap();
    structure.add_node(Sample::new(38, 1000)).unwrap();
    structure.add_node(Sample::new(12035, 2)).unwrap();


    let (_, bitmap) = mem::get_heap(mem::Regions::Region1);
    let mut passed = false;
    let total_bitmap_size = (mem::BOOT_REGIONS[1] / mem::MIN_SLOT_SIZE) >> 3;
    for byte in 0..total_bitmap_size {
        let ptr = unsafe {bitmap.add(byte)};
        if unsafe {*ptr} != 0 {
            passed = true;
            break;
        }
    }

    assert!(passed);

    test_log!("Structure={:?}", structure);
    
    {
        let _new_list = structure.clone();
        
        passed = false;
        for byte in 0..total_bitmap_size {
            let ptr = unsafe {bitmap.add(byte)};
            if unsafe {*ptr} != 0 {
                passed = true;
                break;
            }
        }

        assert!(passed);
        test_log!("Dropping new list...");
        // Drop test
    }

    test_log!("Clearing structure");
    structure.clear();
    
    for byte in 0..total_bitmap_size {
        let ptr = unsafe {bitmap.add(byte)};
        if unsafe {*ptr} != 0 {
            assert!(false, "List deallocations failed!");
        }
    }
}


#[test]
fn fixed_allocator_test() {
    // Certain tests such as this needs to be run in isolation
    let _guard = get_test_lock().lock().unwrap();
    type Allocator = mem::FixedAllocator<Sample, {mem::Regions::Region0 as usize}>;
    type Allocator1 = mem::FixedAllocator<Sample, {mem::Regions::Region1 as usize}>;
    
    let mut layout = Layout::array::<Sample>(3).unwrap();   
    
    mem::clear_heap();
    mem::setup_heap();
    test_log!("Starting fixed_allocator_test");
    let (heap, r0_bm) = mem::get_heap(mem::Regions::Region0);

    // This should allocate first 3 slots in heap from region 0
    let ptr1 = <Allocator as kernel_intf::mem::Allocator<Sample>>::alloc(layout).unwrap();
    assert_eq!(ptr1.as_ptr() as *const u8, heap);
    assert_eq!(unsafe {*r0_bm}, 0x07);
    
    let ptr2 = <Allocator as kernel_intf::mem::Allocator<Sample>>::alloc(layout).unwrap();
    assert_eq!(ptr2.as_ptr() as *const u8, unsafe {heap.add(size_of::<Sample>() * 3)});
    assert_eq!(unsafe {*r0_bm}, 0x3f);
    unsafe {
        <Allocator as kernel_intf::mem::Allocator<Sample>>::dealloc(ptr1, layout);
    }

    assert_eq!(unsafe {*r0_bm}, 0x38);

    layout = Layout::array::<Sample>(4).unwrap();
    let ptr3 = <Allocator as kernel_intf::mem::Allocator<Sample>>::alloc(layout).unwrap();
    assert_eq!(ptr3.as_ptr() as *const u8, unsafe {heap.add(size_of::<Sample>() * 6)});
    assert_eq!(unsafe {*r0_bm}, 0xf8);
    assert_eq!(unsafe {*r0_bm.add(1)}, 0x03);

    // Check allocation on different region
    let ptr1 = <Allocator1 as kernel_intf::mem::Allocator<Sample>>::alloc(layout).unwrap();
    let (heap1, r1_bm) = mem::get_heap(mem::Regions::Region1);
    assert_eq!(ptr1.as_ptr() as *const u8, heap1);
    assert_eq!(unsafe {*r1_bm}, 0x0f);
}


#[test]
fn list_alloc_test() {
    let _guard = get_test_lock().lock().unwrap();
    let mut structure: FixedList<Sample, {mem::Regions::Region0 as usize}> = List::new();
    mem::clear_heap();
    mem::setup_heap();
    test_log!("Starting list_alloc_test");
    let (_, r0_bm) = mem::get_heap(mem::Regions::Region0);

    structure.add_node(Sample{_a:52, _b: 12}).unwrap();
    structure.add_node(Sample{_a:32, _b: 13}).unwrap();
    structure.add_node(Sample{_a:38, _b: 1000}).unwrap();
    structure.add_node(Sample{_a:-12035, _b: 2}).unwrap();

    println!("Traversing linked list");
    let mut tmp_node = Vec::new();
    for node in structure.iter() {
        println!("{:?}", **node);
        if node._a == -12035 || node._a == 52 {
            tmp_node.push(NonNull::from(node));
        }
    }
    assert_eq!(structure.get_nodes(), 4);
    for del_node in tmp_node {
        unsafe {
           structure.remove_node(del_node);
        }
    }
    
    println!("Traversing list after removing node._a = -12035 and 52 and adding -1232");    
    assert_eq!(structure.get_nodes(), 2);
    
    structure.add_node(Sample{_a:-1232, _b: 34}).unwrap();
    for node in structure.iter_mut() {
        node._a += 2;
        println!("{:?}", **node);
    }
    assert_eq!(structure.get_nodes(), 3);
    
    assert_eq!(unsafe {*r0_bm}, 0x7);
}

#[test]
fn phy_alloc_test() {
    let _guard = get_test_lock().lock().unwrap();
    mem::clear_heap();
    mem::setup_heap();
    
    test_log!("Starting phy_alloc_test");

    mem::test_init_allocator();

    // Initially we have (10 + 2 + 6) - ()
    let layout = Layout::from_size_align(2 * common::PAGE_SIZE, 4096).unwrap();
    let addr = mem::allocate_memory(layout, 0).unwrap();

    // Now we should have (10 + 6) - (2)
    assert_eq!(addr as usize, 20 * common::PAGE_SIZE);
    
    let layout = Layout::from_size_align(5 * common::PAGE_SIZE + 32, 4096).unwrap();
    let addr = mem::allocate_memory(layout, 0).unwrap();

    // Now we should have (10) - (6 + 2)
    assert_eq!(addr as usize, 40 * common::PAGE_SIZE);

    let layout = Layout::from_size_align(common::PAGE_SIZE + 32, 4096).unwrap();
    let addr = mem::allocate_memory(layout, 0).unwrap();

    // Now we should have (8) - (2 + 6 + 2)
    assert_eq!(addr as usize, 0);
    
    let layout = Layout::from_size_align(8 * common::PAGE_SIZE + 16, 4096).unwrap();
    let addr = mem::allocate_memory(layout, 0);

    assert!(addr.is_err_and(|e| {
        e == KError::OutOfMemory
    }));

    let layout = Layout::from_size_align(4 * common::PAGE_SIZE, 8192).unwrap();
    let addr = mem::allocate_memory(layout, 0);
    
    assert!(addr.is_err_and(|e| {
        e == KError::InvalidArgument
    }));
    
    let layout_dealloc = Layout::from_size_align(4 * common::PAGE_SIZE, 4096).unwrap();
    let addr = mem::allocate_memory(layout_dealloc, 0).unwrap();

    // Now we should have (4) - (2 + 6 + 2 + 4)
    assert_eq!(addr as usize, 2 * common::PAGE_SIZE);
    mem::deallocate_memory(addr, layout_dealloc, 0).unwrap();
    mem::check_mem_nodes();
}

#[test]
fn virt_alloc_test() {
    let _guard = get_test_lock().lock().unwrap();
    mem::clear_heap();
    mem::setup_heap();
    test_log!("Starting virt_alloc_test");
    mem::virtual_allocator_test();
}