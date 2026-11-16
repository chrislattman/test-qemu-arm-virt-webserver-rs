//! virtio-net driver for the legacy (version 1) virtio-mmio interface, as an
//! embassy-net driver. The same file is used by the Arm and RISC-V versions;
//! only `virt.rs` (addresses) and the memory barrier differ.
//!
//! References:
//! - Virtio spec v1.2, sections 2.7 (split virtqueues), 4.2.4 (legacy MMIO
//!   interface) and 5.1 (network device):
//!   <https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html>
//! - QEMU: hw/virtio/virtio-mmio.c, hw/net/virtio-net.c
//!
//! QEMU's virtio-mmio transport is legacy unless started with
//! `-global virtio-mmio.force-legacy=false`.
//!
//! The rings and buffers live in one static block. Addresses are physical
//! (the Arm MMU identity-maps RAM; RISC-V machine mode has no translation), and
//! QEMU's DMA is cache-coherent, so no cache maintenance is needed.
//!
//! Unlike the C version, there's no copying in or out of the virtio buffers:
//! embassy-net's tokens hand smoltcp the buffer itself.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::Context;

use embassy_net_driver::{Capabilities, HardwareAddress, LinkState};
use embassy_sync::waitqueue::AtomicWaker;
use static_cell::ConstStaticCell;

use crate::println;
use crate::virt::{VIRTIO_MMIO_BASE, VIRTIO_MMIO_COUNT, VIRTIO_MMIO_STRIDE, irq_virtio_mmio};

// --- virtio-mmio legacy registers ---
const MAGIC_VALUE: usize = 0x000; // "virt"
const VERSION: usize = 0x004; // 1 = legacy
const DEVICE_ID: usize = 0x008; // 0 = empty slot
const HOST_FEATURES: usize = 0x010;
const HOST_FEATURES_SEL: usize = 0x014;
const GUEST_FEATURES: usize = 0x020;
const GUEST_FEATURES_SEL: usize = 0x024;
const GUEST_PAGE_SIZE: usize = 0x028;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_ALIGN: usize = 0x03c;
const QUEUE_PFN: usize = 0x040;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const CONFIG: usize = 0x100; // Device-specific configuration

const MAGIC: u32 = 0x7472_6976;
const DEVICE_ID_NET: u32 = 1;

// Device status bits
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;

// virtio-net feature bits and configuration layout
const NET_F_MAC: u32 = 5; // config.mac is valid
const NET_CONFIG_MAC: usize = 0x00; // 6 bytes

// --- Split virtqueues ---
const QUEUE_SIZE: usize = 16; // Entries per queue (power of 2, <= QueueNumMax)
const PAGE_SIZE: usize = 4096; // Legacy: QueuePFN is in units of this, and the used ring is aligned to it

const RX_QUEUE: u32 = 0;
const TX_QUEUE: u32 = 1;

const DESC_F_WRITE: u16 = 2; // Device writes (vs reads) the buffer

// Every packet starts with a virtio_net_hdr, which is 10 bytes since
// VIRTIO_NET_F_MRG_RXBUF isn't negotiated. All zeros on transmit means no
// checksum offload or segmentation.
const NET_HDR_SIZE: usize = 10;
const ETH_MAX_FRAME: usize = 1514; // Without FCS, which virtio doesn't use
const BUF_SIZE: usize = NET_HDR_SIZE + ETH_MAX_FRAME;

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct Avail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C)]
struct UsedElem {
    id: u32,
    len: u32,
}

#[repr(C, align(4096))] // Legacy: the used ring starts on the next page
struct Used {
    flags: u16,
    idx: u16,
    ring: [UsedElem; QUEUE_SIZE],
    avail_event: u16,
}

/// The legacy layout: descriptor table, then the available ring straight
/// after it, then the used ring at the next page boundary
#[repr(C, align(4096))]
struct Virtq {
    desc: [Desc; QUEUE_SIZE],
    avail: Avail,
    used: Used,
}

/// Memory shared with the device
#[repr(C)]
struct DmaMemory {
    rxq: Virtq,
    txq: Virtq,
    rx_bufs: [[u8; BUF_SIZE]; QUEUE_SIZE],
    tx_bufs: [[u8; BUF_SIZE]; QUEUE_SIZE],
}

// SAFETY: all zeros is a valid DmaMemory (it's all integers). ConstStaticCell
// places it straight in .bss, rather than building it on the stack first.
static DMA: ConstStaticCell<DmaMemory> = ConstStaticCell::new(unsafe { core::mem::zeroed() });

/// The interrupt handler's view of the device: its address and the waker of
/// whoever (embassy-net's runner) is waiting for it
static BASE: AtomicUsize = AtomicUsize::new(0);
static WAKER: AtomicWaker = AtomicWaker::new();

fn reg_read(base: usize, offset: usize) -> u32 {
    unsafe { ((base + offset) as *const u32).read_volatile() }
}

fn reg_write(base: usize, offset: usize, value: u32) {
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

/// Orders ring writes before the device can see them (or an MMIO notify), and
/// used ring index reads before the entries they cover. Also a compiler
/// barrier, since the asm may touch memory.
fn mb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags));
    }
}

/// The device's index into the used ring, which it updates behind our back
fn used_idx(q: &Virtq) -> u16 {
    unsafe { addr_of!(q.used.idx).read_volatile() }
}

/// Adds descriptor `id` to the available ring
fn make_available(q: &mut Virtq, id: u16) {
    let idx = q.avail.idx;
    q.avail.ring[idx as usize % QUEUE_SIZE] = id;
    mb();
    unsafe { addr_of_mut!(q.avail.idx).write_volatile(idx.wrapping_add(1)) };
}

/// Reads used ring entry number `n` (id, length)
fn used_elem(q: &Virtq, n: u16) -> (u16, usize) {
    mb();
    let e = &q.used.ring[n as usize % QUEUE_SIZE];
    unsafe {
        (
            addr_of!(e.id).read_volatile() as u16,
            addr_of!(e.len).read_volatile() as usize,
        )
    }
}

struct RxQueue {
    base: usize,
    q: &'static mut Virtq,
    bufs: &'static mut [[u8; BUF_SIZE]; QUEUE_SIZE],
    last_used: u16, // Next used ring entry to look at
}

impl RxQueue {
    /// Hands buffer `id` (back) to the device
    fn post(&mut self, id: u16) {
        let d = &mut self.q.desc[id as usize];
        d.addr = self.bufs[id as usize].as_ptr() as u64;
        d.len = BUF_SIZE as u32;
        d.flags = DESC_F_WRITE;
        make_available(self.q, id);
    }

    fn has_packet(&self) -> bool {
        used_idx(self.q) != self.last_used
    }
}

struct TxQueue {
    base: usize,
    q: &'static mut Virtq,
    bufs: &'static mut [[u8; BUF_SIZE]; QUEUE_SIZE],
    last_used: u16,
    free: [u16; QUEUE_SIZE], // Stack of free descriptor IDs
    free_count: usize,
}

impl TxQueue {
    /// Collects descriptors the device has finished with
    fn reclaim(&mut self) {
        while used_idx(self.q) != self.last_used {
            let (id, _) = used_elem(self.q, self.last_used);
            self.free[self.free_count] = id;
            self.free_count += 1;
            self.last_used = self.last_used.wrapping_add(1);
        }
    }
}

pub struct VirtioNet {
    mac: [u8; 6],
    rx: RxQueue,
    tx: TxQueue,
}

impl VirtioNet {
    /// Finds the first virtio-net device on the virtio-mmio transports and
    /// sets it up. Returns the driver and the device's interrupt ID, which the
    /// caller must route to [`on_interrupt`]. Can only succeed once.
    pub fn new() -> Option<(Self, u32)> {
        let slot = (0..VIRTIO_MMIO_COUNT).find(|&slot| {
            let base = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
            if reg_read(base, MAGIC_VALUE) != MAGIC || reg_read(base, DEVICE_ID) != DEVICE_ID_NET {
                return false;
            }
            let version = reg_read(base, VERSION);
            if version != 1 {
                println!(
                    "virtio-net: slot {slot} is virtio-mmio version {version}; this driver only \
                     supports the legacy interface (version 1)"
                );
                return false;
            }
            true
        });
        let Some(slot) = slot else {
            println!("virtio-net: no device found (is -device virtio-net-device on the QEMU command line?)");
            return None;
        };
        let base = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        let dma = DMA.try_take()?;

        // Initialization sequence for legacy devices (spec 3.1.2)
        reg_write(base, STATUS, 0); // Reset
        reg_write(base, STATUS, STATUS_ACKNOWLEDGE);
        reg_write(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

        reg_write(base, HOST_FEATURES_SEL, 0);
        let features = reg_read(base, HOST_FEATURES) & (1 << NET_F_MAC);
        reg_write(base, GUEST_FEATURES_SEL, 0);
        reg_write(base, GUEST_FEATURES, features);

        reg_write(base, GUEST_PAGE_SIZE, PAGE_SIZE as u32);
        for (index, q) in [(RX_QUEUE, &dma.rxq), (TX_QUEUE, &dma.txq)] {
            reg_write(base, QUEUE_SEL, index);
            assert_eq!(reg_read(base, QUEUE_PFN), 0, "virtqueue already in use");
            assert!(reg_read(base, QUEUE_NUM_MAX) as usize >= QUEUE_SIZE);
            reg_write(base, QUEUE_NUM, QUEUE_SIZE as u32);
            reg_write(base, QUEUE_ALIGN, PAGE_SIZE as u32);
            reg_write(base, QUEUE_PFN, (q as *const Virtq as usize / PAGE_SIZE) as u32);
        }

        let mac = if features & (1 << NET_F_MAC) != 0 {
            core::array::from_fn(|i| unsafe {
                ((base + CONFIG + NET_CONFIG_MAC + i) as *const u8).read_volatile()
            })
        } else {
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56] // QEMU's default MAC address
        };

        let mut rx = RxQueue { base, q: &mut dma.rxq, bufs: &mut dma.rx_bufs, last_used: 0 };
        let tx = TxQueue {
            base,
            q: &mut dma.txq,
            bufs: &mut dma.tx_bufs,
            last_used: 0,
            free: core::array::from_fn(|i| i as u16),
            free_count: QUEUE_SIZE,
        };
        // Give the device every RX buffer, and keep every TX buffer for ourselves
        for id in 0..QUEUE_SIZE as u16 {
            rx.post(id);
        }

        BASE.store(base, Ordering::Release);
        reg_write(base, STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
        mb();
        reg_write(base, QUEUE_NOTIFY, RX_QUEUE);

        let irq = irq_virtio_mmio(slot);
        println!(
            "virtio-net: slot {slot}, IRQ {irq}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        Some((Self { mac, rx, tx }, irq))
    }
}

/// The device's interrupt handler: the device has used some RX or TX buffers
pub fn on_interrupt() {
    let base = BASE.load(Ordering::Acquire);
    let status = reg_read(base, INTERRUPT_STATUS);
    reg_write(base, INTERRUPT_ACK, status);
    WAKER.wake();
}

impl embassy_net_driver::Driver for VirtioNet {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, cx: &mut Context) -> Option<(RxToken<'_>, TxToken<'_>)> {
        // Register before checking, so an interrupt in between isn't missed
        WAKER.register(cx.waker());
        self.tx.reclaim();
        // smoltcp may reply to the packet right away, so it needs a TX buffer too
        if self.rx.has_packet() && self.tx.free_count > 0 {
            Some((RxToken { rx: &mut self.rx }, TxToken { tx: &mut self.tx }))
        } else {
            None
        }
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<TxToken<'_>> {
        WAKER.register(cx.waker());
        self.tx.reclaim();
        if self.tx.free_count > 0 { Some(TxToken { tx: &mut self.tx }) } else { None }
    }

    fn link_state(&mut self, _cx: &mut Context) -> LinkState {
        LinkState::Up // QEMU's user-mode network is always connected
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default();
        caps.max_transmission_unit = ETH_MAX_FRAME; // Ethernet MTU: IP MTU (1500) + Ethernet header
        caps
    }

    fn hardware_address(&self) -> HardwareAddress {
        HardwareAddress::Ethernet(self.mac)
    }
}

pub struct RxToken<'a> {
    rx: &'a mut RxQueue,
}

impl embassy_net_driver::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, f: F) -> R {
        let rx = self.rx;
        let (id, len) = used_elem(rx.q, rx.last_used);
        rx.last_used = rx.last_used.wrapping_add(1);
        let len = len.clamp(NET_HDR_SIZE, BUF_SIZE);
        let result = f(&mut rx.bufs[id as usize][NET_HDR_SIZE..len]);
        rx.post(id);
        mb();
        reg_write(rx.base, QUEUE_NOTIFY, RX_QUEUE);
        result
    }
}

pub struct TxToken<'a> {
    tx: &'a mut TxQueue,
}

impl embassy_net_driver::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let tx = self.tx;
        // A token is only handed out when there's a free buffer
        tx.free_count -= 1;
        let id = tx.free[tx.free_count];
        let buf = &mut tx.bufs[id as usize];
        let len = len.min(ETH_MAX_FRAME);
        buf[..NET_HDR_SIZE].fill(0);
        let result = f(&mut buf[NET_HDR_SIZE..NET_HDR_SIZE + len]);

        let d = &mut tx.q.desc[id as usize];
        d.addr = buf.as_ptr() as u64;
        d.len = (NET_HDR_SIZE + len) as u32;
        d.flags = 0;
        make_available(tx.q, id);
        mb();
        reg_write(tx.base, QUEUE_NOTIFY, TX_QUEUE);
        result
    }
}
