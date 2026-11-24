# QEMU ARM "virt" (Cortex-A53) web server in no_std Rust with embassy-net

A web server built on the bare-metal setup of the test-qemu-arm-virt-rs project, using [embassy](https://embassy.dev) for async tasks and [embassy-net](https://docs.embassy.dev/embassy-net) (built on [smoltcp](https://github.com/smoltcp-rs/smoltcp)) for networking. It's the Rust counterpart of the test-qemu-arm-virt-webserver project (FreeRTOS + lwIP in C), and its networking code follows the test-bare-metal-pico-rs-webserver project.

This README covers what's new compared with test-qemu-arm-virt-rs. See that project for the softfloat target, how QEMU loads the ELF, and the serial console options.

## Build and run

Install `qemu-system-arm` and Rust as for test-qemu-arm-virt-rs, then:

```
cargo qemu   # release build; connect with `telnet localhost 5678`
```

The console (on the telnet port) shows the address from DHCP:

```
virtio-net: slot 31, IRQ 79, MAC 52:54:00:12:34:56
Waiting for DHCP...
IP address 10.0.2.15, listening on port 8080
```

Then browse to http://localhost:8080, or run `curl http://localhost:8080`. QEMU's user-mode networking hands the guest `10.0.2.15` over DHCP and forwards host port 8080 to it (see the test-qemu-arm-virt-webserver README for the network setup and the virtio-mmio slots).

## What's new compared with test-qemu-arm-virt-rs

| File | Change |
|---|---|
| `startup.S` | Also turns on the MMU (see below). Its constants are `.set` rather than `#define`, since `global_asm!` doesn't run the C preprocessor |
| `linker.ld` | 256KB stack (was 16KB) |
| `platform.rs` | New: the executor's run loop, interrupt dispatch, fault and panic handlers |
| `timer.rs` | New: embassy-time driver on the EL1 physical generic timer |
| `gic.rs` | A minimal GICv2 driver of its own, in place of the `arm-gic` crate |
| `virtio_net.rs` | New: virtio-net driver (legacy virtio-mmio interface) implementing `embassy_net_driver::Driver` |
| `uart.rs` | PL011 output and `print!`/`println!` macros, in place of the `arm-pl011-uart` crate |
| `main.rs` | The web server, in place of the SGI interrupt demo |

### Embassy without a HAL

The Pico version gets its executor platform, time driver and critical sections from `embassy-rp`. There's no embassy HAL for QEMU's `virt` board, and embassy-executor has no platform for AArch64, so this project supplies those parts itself:

- **Executor:** `platform::run` drives embassy's `raw::Executor`. Its `__pender` sets a flag; the loop polls tasks, then sleeps with `wfi` unless the flag is set. IRQs are masked while checking the flag, so an interrupt in between can't be missed.
- **Time driver:** `timer.rs` implements `embassy_time_driver::Driver` with the generic timer. It uses `embassy-time-queue-utils` for the timer queue, and converts between embassy's 1MHz ticks and the timer's frequency (`CNTFRQ_EL0`, 62.5MHz on QEMU), which is only known at run time.
- **Critical sections:** masking interrupts is enough on a single core, so this uses the [`aarch64-cpu`](https://docs.rs/aarch64-cpu) crate's `critical-section-single-core` feature. System registers, barriers and `wfi` also go through `aarch64-cpu` rather than inline `asm!`.
- **Network driver:** `virtio_net.rs` implements `embassy_net_driver::Driver`. Its RX and TX tokens hand smoltcp the virtio buffers themselves, so unlike the C version nothing is copied. The device interrupt just wakes embassy-net's runner task.

Interrupt handlers never run tasks: they acknowledge the device and wake a waker, and tasks run in the executor's loop. So handlers don't nest, and, as in test-qemu-arm-virt-rs, `irq_entry` only saves the registers a Rust function can clobber.

### The MMU

test-qemu-arm-virt-rs runs with the MMU off. Here it's on, with an identity map (the same one as the C version), because with it off all memory is Device memory, where atomic instructions aren't guaranteed to work, and embassy relies on atomics.

### Concurrency

The C version uses one FreeRTOS task per connection, and lwIP queues new connections until `accept()` takes them. smoltcp has no such queue: a connection is reset unless a socket is already listening for it. So this server runs a fixed pool of 64 server tasks (`SERVER_TASKS`), each with its own socket and buffers, all listening on port 8080. Tasks are allocated statically by `#[embassy_executor::task(pool_size = ...)]`, so there's no heap.

Tested on QEMU with curl: release builds serve every request with 48 parallel clients (about 450 requests/s). Debug builds are slower, so sockets stay busy longer and some connections are reset at 48 parallel clients.

As in the C version, the server sends `Connection: close` and waits for the client to close first before closing its socket. Dropping an embassy-net socket removes it from smoltcp straight away, so if the client's FIN arrived afterwards it would be answered with a reset.

## Notes

- **Stack size:** everything runs on one stack (tasks keep their state in `.bss`). Release builds use about 3KB of it under load, but debug builds use about 70KB, because unoptimized code copies large futures through the stack. With a 64KB stack, debug builds overflowed into the end of `.bss`, overwrote the virtio driver's register address, and its interrupt handler then acknowledged the wrong address forever. Nothing detects an overflow, so leave plenty of room.
- **Slow connections under load:** with several clients connecting at once, about 1-2% of connections take 1 or 2 seconds, in the C version too. They're slow to connect on the host side, before the guest is involved, which matches the host kernel retrying a dropped SYN; QEMU's user-mode networking accepts forwarded connections from a small listen backlog.
