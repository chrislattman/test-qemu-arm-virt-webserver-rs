#![no_std]
#![no_main]

mod gic;
mod platform;
mod timer;
mod uart;
mod virt;
mod virtio_net;

use core::fmt::Write as _;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::Write as _;
use heapless::String;
use static_cell::StaticCell;

use virtio_net::VirtioNet;

// Core Entry Point Setup
core::arch::global_asm!(include_str!("startup.S"));

/// What the `Server:` header says
const SERVER_NAME: &str = "QEMU virt (Cortex-A53)";

// QEMU's user-mode networking forwards host port 8080 to this port (see
// .cargo/config.toml)
const PORT_NUMBER: u16 = 8080;

/// Number of connections served at once. smoltcp has no listen backlog: a new
/// connection is reset unless a socket is listening for it, so each server
/// task has its own socket listening on the port.
const SERVER_TASKS: usize = 64;

/// Sockets: one per server task, plus one for the DHCP client
const SOCKETS: usize = SERVER_TASKS + 1;

const DHCP_TIMEOUT: Duration = Duration::from_secs(30);

static COUNTER: Mutex<CriticalSectionRawMutex, u32> = Mutex::new(0);

/// Called by startup.S
#[unsafe(no_mangle)]
extern "C" fn main() -> ! {
    gic::init();
    timer::init();
    platform::run(|spawner| spawner.spawn(main_task(spawner).unwrap()))
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, VirtioNet>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn main_task(spawner: Spawner) {
    let Some((device, irq)) = VirtioNet::new() else {
        return;
    };
    platform::VIRTIO_IRQ.store(irq, Ordering::Relaxed);
    gic::enable(irq);

    static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        timer::seed(),
    );
    spawner.spawn(net_task(runner).unwrap());

    println!("Waiting for DHCP...");
    if with_timeout(DHCP_TIMEOUT, stack.wait_config_up()).await.is_err() {
        println!("DHCP: no address after {} s", DHCP_TIMEOUT.as_secs());
        return;
    }
    if let Some(config) = stack.config_v4() {
        println!("IP address {}, listening on port {PORT_NUMBER}", config.address.address());
    }

    for _ in 0..SERVER_TASKS {
        spawner.spawn(server_task(stack).unwrap());
    }
}

/// Serves connections one at a time, forever. Several of these run at once.
#[embassy_executor::task(pool_size = SERVER_TASKS)]
async fn server_task(stack: Stack<'static>) -> ! {
    // Unlike FreeRTOS with lwIP, in embassy-net YOU manage the TX and RX
    // buffers. These live in the task, which embassy allocates statically.
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut client_message = [0; 1024];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // Bound how long an idle client can tie up this task
        socket.set_timeout(Some(Duration::from_secs(10)));
        if socket.accept(PORT_NUMBER).await.is_err() {
            continue;
        }

        // Errors only affect this connection; never panic, since the panic
        // handler halts the whole machine
        if serve(&mut socket, &mut client_message).await.is_ok() {
            // Graceful close: "Connection: close" asks the client to close
            // first, so wait for that (read() returns 0), discarding anything
            // else it sends, or until the socket's timeout runs out. That way
            // this side never ends up holding the connection in TIME_WAIT, and
            // dropping the socket afterwards can't leave the client's FIN to
            // be answered with a reset.
            while matches!(socket.read(&mut client_message).await, Ok(n) if n > 0) {}
        }

        // Send FIN and wait for it to be acknowledged before the socket is
        // dropped; if the peer is gone, reset the connection instead
        socket.close();
        if !matches!(with_timeout(Duration::from_secs(5), socket.flush()).await, Ok(Ok(()))) {
            socket.abort();
        }
    }
}

/// Reads one request from `socket` and answers it. Any error just ends this
/// connection; the caller closes the socket either way.
async fn serve(socket: &mut TcpSocket<'_>, client_message: &mut [u8]) -> Result<(), ()> {
    let n = socket.read(client_message).await.map_err(|_| ())?;
    if n == 0 {
        return Err(());
    }
    let client_message = str::from_utf8(&client_message[..n]).map_err(|_| ())?;
    let request_line = client_message.split("\r\n").next().unwrap_or_default();
    // Only accepting HTTP GET requests
    if !request_line.starts_with("GET") {
        return Err(());
    }
    println!("{request_line}");

    {
        let mut counter = COUNTER.lock().await;
        *counter += 1;
        println!("Handling request #{}", *counter);
    }

    let remote = socket.remote_endpoint().ok_or(())?;
    let mut content = String::<128>::new();
    write!(
        content,
        "What's up? This server was written in no_std Rust. Your IP address is {}\r\n",
        remote.addr
    )
    .map_err(|_| ())?;

    let mut server_message = String::<512>::new();
    write!(
        server_message,
        "HTTP/1.1 200 OK\r\n\
         Server: {SERVER_NAME}\r\n\
         Last-Modified: Sun, 15 Nov 2026 00:00:00 GMT\r\n\
         Accept-Ranges: bytes\r\n\
         Content-Length: {}\r\n\
         Content-Type: text/html\r\n\
         Connection: close\r\n\
         \r\n\
         {content}",
        content.len()
    )
    .map_err(|_| ())?;

    socket.write_all(server_message.as_bytes()).await.map_err(|_| ())?;
    Ok(())
}
