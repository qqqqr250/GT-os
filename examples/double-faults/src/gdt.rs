//! The global descriptor table, the task state segment and the interrupt stack
//! table.
//!
//! This is post-06's second half. To survive a double fault caused by a *stack
//! overflow*, the CPU must switch to a known-good stack before it pushes the
//! exception frame. That stack is named in the task state segment (TSS), whose
//! *interrupt stack table* (IST) holds up to seven of them, and the TSS is
//! reached through a descriptor in the global descriptor table (GDT). This
//! module owns all three.
//!
//! Unlike post-06, which appends only a code segment and reloads `cs`, this
//! module also appends a data segment and reloads the data-segment registers.
//! Without that the registers would keep pointing at the *bootloader's* GDT
//! selectors, which now name different descriptors here.

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

/// The IST slot the double-fault handler uses.
///
/// The software index runs `0..=6` and maps onto the hardware IST's `1..=7`
/// (the `x86_64` crate's `set_stack_index` adds the one). Any slot would do; 0
/// is the first.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Size of the dedicated double-fault stack.
///
/// A double-fault handler must not do anything stack-hungry, and unlike the
/// kernel stack this one has no guard page, so a few pages are plenty.
const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

/// A stack with the 16-byte alignment the ABI expects.
///
/// The array is never read or written by Rust: once the TSS names its top the
/// CPU owns it, pushing the double-fault frame itself. The `dead_code` allowance
/// covers exactly that.
#[repr(align(16))]
#[allow(dead_code)]
struct Stack([u8; DOUBLE_FAULT_STACK_SIZE]);

/// The stack the CPU switches to on a double fault.
///
/// `static mut` on purpose: an immutable `static` would land in a read-only
/// section, and a read-only stack faults. The TSS points at the *top* of this
/// array, because x86 stacks grow downwards.
static mut DOUBLE_FAULT_STACK: Stack = Stack([0; DOUBLE_FAULT_STACK_SIZE]);

/// The task state segment. Only its interrupt stack table matters here.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// The table itself, built in place by [`init`].
static GDT: Mutex<GlobalDescriptorTable> = Mutex::new(GlobalDescriptorTable::new());

/// The selectors [`init`] appended, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selectors {
    /// The kernel code segment.
    pub code: SegmentSelector,
    /// The kernel data segment.
    pub data: SegmentSelector,
    /// The TSS segment.
    pub tss: SegmentSelector,
}

/// The selectors [`init`] produced, or `None` before it runs.
static SELECTORS: Mutex<Option<Selectors>> = Mutex::new(None);

/// Builds the TSS and the GDT, then loads both.
///
/// The TSS gets a double-fault stack in its IST; the ring-0 privilege stack is
/// left alone because this crate never enters ring 3.
///
/// Must run before [`crate::interrupts::init`], which points the double-fault
/// IDT entry at [`DOUBLE_FAULT_IST_INDEX`]: until the TSS is loaded the CPU has
/// no stack for that slot.
#[must_use]
pub fn init() -> Selectors {
    // SAFETY: single-threaded boot, before any exception can fire, and both the
    // TSS and the stack are statics, so the addresses written stay valid.
    let stack_top = unsafe {
        let stack = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK).cast::<u8>();
        stack.add(DOUBLE_FAULT_STACK_SIZE) as u64
    };
    // SAFETY: as above; the bootloader's own stack is left untouched.
    unsafe {
        let tss = core::ptr::addr_of_mut!(TSS);
        (*tss).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)] =
            VirtAddr::new(stack_top);
    }

    let mut gdt = GDT.lock();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let data = gdt.append(Descriptor::kernel_data_segment());
    // SAFETY: the TSS lives in a static, so the `&'static` reference it needs is
    // valid for the whole program.
    let tss = gdt.append(Descriptor::tss_segment(unsafe {
        &*core::ptr::addr_of!(TSS)
    }));
    // SAFETY: `GDT` lives in a static and is never moved, so the address the CPU
    // caches stays valid; `load` would need a `&'static` the guard cannot give.
    unsafe { gdt.load_unsafe() };
    drop(gdt);

    // SAFETY: the selectors name the descriptors appended just above and the TSS
    // is a static. Reloading the data-segment registers avoids leaving them
    // pointing at stale selectors from the bootloader's GDT.
    unsafe {
        CS::set_reg(code);
        SS::set_reg(data);
        DS::set_reg(data);
        ES::set_reg(data);
        FS::set_reg(data);
        GS::set_reg(data);
        load_tss(tss);
    }

    let selectors = Selectors { code, data, tss };
    *SELECTORS.lock() = Some(selectors);
    selectors
}

/// The selectors [`init`] produced, or `None` before it runs.
#[must_use]
pub fn selectors() -> Option<Selectors> {
    *SELECTORS.lock()
}

/// The top of the double-fault stack the TSS names, or 0 before [`init`].
#[must_use]
pub fn double_fault_stack_top() -> u64 {
    // SAFETY: reading a `static` TSS is sound; boot is single-threaded and the
    // table is only written by `init`.
    unsafe {
        (*core::ptr::addr_of!(TSS)).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)]
            .as_u64()
    }
}
