//! embassy-time driver using the EL1 physical generic timer.
//!
//! embassy-time counts in ticks of 1MHz (its default tick rate). The generic
//! timer's frequency is only known at run time (CNTFRQ_EL0: 62.5MHz on QEMU's
//! Cortex-A53), so times are converted between the two.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;

use aarch64_cpu::asm::barrier;
use aarch64_cpu::registers::{CNTFRQ_EL0, CNTP_CTL_EL0, CNTP_CVAL_EL0, CNTPCT_EL0, Read, Write};
use critical_section::{CriticalSection, Mutex};
use embassy_time_driver::{Driver, TICK_HZ};
use embassy_time_queue_utils::Queue;

use crate::gic;
use crate::virt::IRQ_EL1_PHYS_TIMER;

struct GenericTimer {
    frequency: AtomicU64,
    queue: Mutex<RefCell<Queue>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: GenericTimer = GenericTimer {
    frequency: AtomicU64::new(1),
    queue: Mutex::new(RefCell::new(Queue::new())),
});

fn counter() -> u64 {
    CNTPCT_EL0.get()
}

pub fn init() {
    DRIVER.frequency.store(CNTFRQ_EL0.get(), Ordering::Relaxed);
    gic::enable(IRQ_EL1_PHYS_TIMER);
}

/// A number to seed embassy-net's random number generator with
pub fn seed() -> u64 {
    counter()
}

impl GenericTimer {
    /// Fires the timer interrupt at embassy-time tick `at`, or never if it's
    /// u64::MAX. Returns false if `at` has already passed (the caller then
    /// handles expired timers again instead).
    fn set_alarm(&self, _cs: CriticalSection, at: u64) -> bool {
        if at == u64::MAX {
            CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::CLEAR);
            return true;
        }
        let frequency = self.frequency.load(Ordering::Relaxed) as u128;
        // Round up, so the interrupt doesn't fire just before `at`
        let cval = (at as u128 * frequency).div_ceil(TICK_HZ as u128).min(u64::MAX as u128) as u64;
        CNTP_CVAL_EL0.set(cval);
        CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET + CNTP_CTL_EL0::IMASK::CLEAR);
        barrier::isb(barrier::SY);
        at > self.now()
    }

    fn process_queue(&self, cs: CriticalSection) {
        let mut queue = self.queue.borrow(cs).borrow_mut();
        let mut next = queue.next_expiration(self.now());
        while !self.set_alarm(cs, next) {
            next = queue.next_expiration(self.now());
        }
    }
}

impl Driver for GenericTimer {
    fn now(&self) -> u64 {
        let frequency = self.frequency.load(Ordering::Relaxed) as u128;
        (counter() as u128 * TICK_HZ as u128 / frequency) as u64
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let changed = self.queue.borrow(cs).borrow_mut().schedule_wake(at, waker);
            if changed {
                self.process_queue(cs);
            }
        });
    }
}

/// The timer's interrupt handler: wakes the tasks whose timers have expired
/// and sets up the next alarm (which also clears this level-triggered interrupt)
pub fn on_interrupt() {
    critical_section::with(|cs| DRIVER.process_queue(cs));
}
