//! Runtime support: the embassy executor's run loop, the critical-section
//! implementation, interrupt dispatch, and the fault and panic handlers.

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::{Spawner, raw};
use static_cell::StaticCell;

use crate::{gic, println, timer, virtio_net};

// --- Executor ---
//
// embassy-executor has no "platform" for AArch64, so this drives its raw
// executor directly. Tasks only run from this loop; interrupt handlers just
// wake them.

/// Set by `__pender` when the executor has tasks to poll
static SIGNAL: AtomicBool = AtomicBool::new(false);

/// Called by the executor (from a task or an interrupt handler) when a task
/// has been woken
#[unsafe(export_name = "__pender")]
fn pender(_context: *mut ()) {
    SIGNAL.store(true, Ordering::Release);
}

/// Runs `init` as the first task, then runs tasks forever, sleeping until the
/// next interrupt whenever there's nothing to do
pub fn run(init: impl FnOnce(Spawner)) -> ! {
    static EXECUTOR: StaticCell<raw::Executor> = StaticCell::new();
    let executor: &'static raw::Executor = EXECUTOR.init(raw::Executor::new(core::ptr::null_mut()));
    init(executor.spawner());

    loop {
        // SAFETY: poll() is only ever called here, never reentrantly
        unsafe { executor.poll() };

        // Sleep, unless a task was woken while polling. IRQs are masked while
        // checking, so a wake-up between the check and the WFI can't be
        // missed: a pending interrupt ends WFI even while masked, and is then
        // taken when IRQs are unmasked.
        unsafe { asm!("msr daifset, #2", options(nostack)) };
        if !SIGNAL.swap(false, Ordering::Acquire) {
            unsafe { asm!("wfi", options(nomem, nostack)) };
        }
        unsafe { asm!("msr daifclr, #2", options(nostack)) };
    }
}

// --- Critical sections (used by embassy) ---
// Single core: masking IRQs makes a section exclusive. The previous DAIF value
// is restored on release, so sections nest and work inside interrupt handlers.

struct SingleCoreCriticalSection;
critical_section::set_impl!(SingleCoreCriticalSection);

unsafe impl critical_section::Impl for SingleCoreCriticalSection {
    unsafe fn acquire() -> u64 {
        let daif: u64;
        // No `nomem` option, so these also act as compiler memory barriers
        unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nostack)) };
        daif
    }

    unsafe fn release(daif: u64) {
        unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
    }
}

// --- Interrupts ---

/// The virtio-net device's interrupt ID, once it's been found
pub static VIRTIO_IRQ: AtomicU32 = AtomicU32::new(gic::SPURIOUS_ID);

/// Called from irq_entry (startup.S), with IRQs masked
#[unsafe(no_mangle)]
extern "C" fn handle_irq() {
    let iar = gic::acknowledge();
    let id = iar & 0x3FF;

    if id == crate::virt::IRQ_EL1_PHYS_TIMER {
        timer::on_interrupt();
    } else if id == VIRTIO_IRQ.load(Ordering::Relaxed) {
        virtio_net::on_interrupt();
    }

    if id != gic::SPURIOUS_ID {
        gic::end_of_interrupt(iar);
    }
}

/// Called from fault_entry (startup.S) for any other exception
#[unsafe(no_mangle)]
extern "C" fn handle_fault(kind: u64, esr: u64, elr: u64, far: u64) -> ! {
    const KINDS: [&str; 4] = ["Synchronous", "IRQ", "FIQ", "SError"];
    println!(
        "\n*** Unexpected exception: {} (exception class {:#x})\n    ESR_EL1 = {esr:#018x}\n    \
         ELR_EL1 = {elr:#018x}\n    FAR_EL1 = {far:#018x}",
        KINDS[kind as usize % 4],
        esr >> 26
    );
    halt()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { asm!("msr daifset, #2", options(nostack)) };
    println!("\n*** {info}");
    halt()
}

fn halt() -> ! {
    loop {
        unsafe { asm!("msr daifset, #2", "wfi", options(nomem, nostack)) };
    }
}
