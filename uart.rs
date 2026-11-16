//! Polled PL011 console output, and the `print!`/`println!` macros.
//!
//! Only the executor thread prints (interrupt handlers never do, apart from
//! the fault and panic handlers, which stop everything), so no locking is
//! needed.

use core::fmt;

use crate::virt::UART0_BASE;

const UART0_DR: *mut u32 = UART0_BASE as *mut u32;
const UART0_FR: *const u32 = (UART0_BASE + 0x18) as *const u32;
const UART_FR_TXFF: u32 = 1 << 5;

pub fn putc(c: u8) {
    // Wait until UART TX FIFO has space. QEMU leaves the PL011 enabled at reset.
    unsafe {
        while UART0_FR.read_volatile() & UART_FR_TXFF != 0 {}
        UART0_DR.write_volatile(c as u32);
    }
}

pub struct Uart;

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.bytes() {
            if c == b'\n' {
                putc(b'\r'); // Terminals want "\r\n"
            }
            putc(c);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::uart::Uart, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = writeln!($crate::uart::Uart, $($arg)*);
    }};
}
