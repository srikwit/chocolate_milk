//! Main Rust entry point for the chocolate milk bootloader

#![feature(panic_info_message, rustc_private, alloc_error_handler)]
#![no_std]
#![no_main]

extern crate compiler_builtins;
extern crate core_reqs;

#[allow(unused_imports)]
#[macro_use] extern crate alloc;

#[macro_use] mod print;
mod realmode;
mod mm;
mod panic;
mod pxe;
mod intrins;

use core::sync::atomic::{AtomicU64, Ordering};
use serial::SerialPort;
use boot_args::{BootArgs, KERNEL_PHYS_WINDOW_SIZE, KERNEL_STACKS_BASE};
use boot_args::{KERNEL_PHYS_WINDOW_BASE, KERNEL_STACK_SIZE, KERNEL_STACK_PAD};
use pe_parser::PeParser;
use lockcell::LockCell;
use page_table::{VirtAddr, PageType, PageTable, PAGE_PRESENT, PAGE_WRITE};

/// Global arguments shared between the kernel and bootloader. It is critical
/// that every structure in here is identical in shape between both 64-bit
/// and 32-bit representations.
pub static BOOT_ARGS: BootArgs = BootArgs {
    free_memory:           LockCell::new(None),
    serial:                LockCell::new(None),
    page_table:            LockCell::new(None),
    trampoline_page_table: LockCell::new(None),
    kernel_entry:          LockCell::new(None),
    stack_vaddr:           AtomicU64::new(KERNEL_STACKS_BASE),
    print_lock:            LockCell::new(()),
};

/// Rust entry point for the bootloader
///
/// * `bootloader_end` - One byte past the end of the bootloader
#[no_mangle]
extern fn entry(bootloader_end: usize) -> ! {
    // Initialize the serial driver
    {
        // Get access to the serial driver
        let mut serial = BOOT_ARGS.serial.lock();

        if serial.is_none() {
            // Driver has not yet been set up, initialize the ports
            *serial = Some(unsafe { SerialPort::new() });

            // "clear" the screen
            core::mem::drop(serial);
            for _ in 0..100 {
                print!("\n");
            }
    
            print!("Chocolate Milk bootloader starting...\n");
            print!("Bootloader end at {:#x}\n", bootloader_end);
        }
    }

    // Initialize the MMU
    mm::init();

    // Download the kernel and create the kernel page table
    let (entry_point, stack, cr3, tramp_cr3) = {
        let mut kernel_entry = BOOT_ARGS.kernel_entry.lock();
        let mut page_table   = BOOT_ARGS.page_table.lock();
        let mut tramp_table  = BOOT_ARGS.trampoline_page_table.lock();

        // If no kernel entry is set yet, download the kernel and load it
        if kernel_entry.is_none() {
            assert!(page_table.is_none() && tramp_table.is_none(),
                "Page tables set up before kernel!?");

            // Download the kernel
            let kernel = pxe::download("chocolate_milk.kern")
                .expect("Failed to download chocolate_milk.kern over TFTP");

            // Parse the PE from the kernel
            let pe = PeParser::parse(&kernel).expect("Failed to parse PE");

            // Get exclusive access to physical memory
            let mut pmem = BOOT_ARGS.free_memory.lock();
            let pmem = pmem.as_mut()
                .expect("Whoa, physical memory not initialized yet");
            let mut pmem = mm::PhysicalMemory(pmem);
            
            // Create the trampoline page table
            let mut trampoline_table = PageTable::new(&mut pmem);

            // Create the 2 different physical map windows for the trampoline
            // page table
            for paddr in (0..bootloader_end as u64).step_by(4096) {
                unsafe {
                    // Create a mapping where vaddr == paddr
                    trampoline_table.map_raw(
                        &mut pmem, VirtAddr(paddr), PageType::Page4K,
                        paddr | PAGE_WRITE | PAGE_PRESENT).unwrap();

                    // Create a mapping where
                    // vaddr == (paddr + KERNEL_PHYS_WINDOW_BASE)
                    trampoline_table.map_raw(
                        &mut pmem, VirtAddr(KERNEL_PHYS_WINDOW_BASE + paddr),
                        PageType::Page4K,
                        paddr | PAGE_WRITE | PAGE_PRESENT).unwrap();
                }
            }

            // Create a new page table
            let mut table = PageTable::new(&mut pmem);

            // Create a linear map of physical memory
            for paddr in (0..KERNEL_PHYS_WINDOW_SIZE).step_by(4096) {
                unsafe {
                    table.map_raw(&mut pmem,
                        VirtAddr(KERNEL_PHYS_WINDOW_BASE + paddr),
                        PageType::Page4K,
                        paddr | PAGE_WRITE | PAGE_PRESENT).unwrap();
                }
            }

            // Load all the sections from the PE into the new page table
            pe.sections(|vaddr, vsize, raw, read, write, execute| {
                // Create a new virtual mapping for the PE range and initialize
                // it to the raw bytes from the PE file, otherwise to zero for
                // all bytes that were not initialized in the file.
                table.map_init(&mut pmem, VirtAddr(vaddr),
                    PageType::Page4K,
                    vsize as u64, read, write, execute,
                    Some(|off| {
                        raw.get(off as usize).copied().unwrap_or(0)
                    }));

                print!("Created map at {:#018x} for {:#018x} bytes | \
                       perms {}{}{}\n",
                       vaddr, vsize,
                       if read    { "R" } else { "-" },
                       if write   { "W" } else { "-" },
                       if execute { "X" } else { "-" });

                Some(())
            }).unwrap();

            print!("Entry point is {:#x}\n", pe.entry_point);

            // Set up the entry point and page table
            *kernel_entry = Some(pe.entry_point);
            *tramp_table  = Some(trampoline_table);
            *page_table   = Some(table);
        }

        // Get exclusive access to physical memory
        let mut pmem = BOOT_ARGS.free_memory.lock();
        let pmem = pmem.as_mut()
            .expect("Whoa, physical memory not initialized yet");
        let mut pmem = mm::PhysicalMemory(pmem);

        // At this point the page table is always set up
        let page_table = page_table.as_mut().unwrap();

        // Get a unique stack address for this core
        let stack_addr = BOOT_ARGS.stack_vaddr.fetch_add(
            KERNEL_STACK_SIZE + KERNEL_STACK_PAD, Ordering::SeqCst);
        
        // Map in the stack
        page_table.map(&mut pmem,
                       VirtAddr(stack_addr), PageType::Page4K,
                       KERNEL_STACK_SIZE, true, true, false).unwrap();

        (
            *kernel_entry.as_ref().unwrap(),
            stack_addr + KERNEL_STACK_SIZE,
            page_table.table().0 as u32,
            tramp_table.as_ref().unwrap().table().0 as u32,
        )
    };

    extern {
        fn enter64(entry_point: u64, stack: u64, param: u64, cr3: u32,
                   tramp_cr3: u32, phys_window_base: u64) -> !;
    }

    unsafe {
        enter64(entry_point, stack, &BOOT_ARGS as *const BootArgs as u64,
                cr3, tramp_cr3, KERNEL_PHYS_WINDOW_BASE);
    }
}

