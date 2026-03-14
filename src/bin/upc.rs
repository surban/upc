//! USB packet channel (UPC) command-line tool.
//!
//! Install with:
//! - Host side: `cargo install upc --features cli,host`
//! - Device side: `cargo install upc --features cli,device`
//! - Both: `cargo install upc --features cli,host,device`

#[cfg(not(any(feature = "host", feature = "device")))]
compile_error!("The `cli` feature requires at least one of `host` or `device` features to be enabled.");

use std::{
    io::{self, IsTerminal, Write},
    pin::pin,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    sync::Notify,
    time::{sleep, Instant},
};
use upc::Class;

/// Default UPC interface protocol (hex string).
const DEFAULT_PROTOCOL: &str = "84";
/// Default Device Interface GUID for Microsoft OS (Windows).
const DEFAULT_GUID: &str = "19840902-89fc-40d8-bc10-f71079b789b5";
/// Idle timeout after stdin closes on an interactive terminal.
const IDLE_TIMEOUT: Duration = Duration::from_secs(1);

// ---- Traits for abstracting over host/device UPC types ----

/// Trait abstracting over host and device UPC senders.
trait UpcSend {
    fn upc_send(&self, data: Bytes) -> impl std::future::Future<Output = io::Result<()>> + Send;
    fn upc_closed(&self) -> impl std::future::Future<Output = ()> + Send;
    fn upc_max_size(&self) -> usize;
}

/// Trait abstracting over host and device UPC receivers.
trait UpcRecv {
    fn upc_recv(&mut self) -> impl std::future::Future<Output = io::Result<Option<Bytes>>> + Send;
}

#[cfg(feature = "host")]
impl UpcSend for upc::host::UpcSender {
    async fn upc_send(&self, data: Bytes) -> io::Result<()> {
        self.send(data).await
    }
    async fn upc_closed(&self) {
        self.closed().await
    }
    fn upc_max_size(&self) -> usize {
        self.max_size()
    }
}

#[cfg(feature = "host")]
impl UpcRecv for upc::host::UpcReceiver {
    async fn upc_recv(&mut self) -> io::Result<Option<Bytes>> {
        self.recv().await
    }
}

#[cfg(feature = "device")]
impl UpcSend for upc::device::UpcSender {
    async fn upc_send(&self, data: Bytes) -> io::Result<()> {
        self.send(data).await
    }
    async fn upc_closed(&self) {
        self.closed().await
    }
    fn upc_max_size(&self) -> usize {
        self.max_size()
    }
}

#[cfg(feature = "device")]
impl UpcRecv for upc::device::UpcReceiver {
    async fn upc_recv(&mut self) -> io::Result<Option<Bytes>> {
        self.recv().await.map(|opt| opt.map(|data| data.freeze()))
    }
}

// ---- Shared stdio forwarding ----

/// Close the process's stdin file descriptor / handle.
///
/// This signals broken pipe to any process piping into us.
fn close_stdin() {
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
        drop(unsafe { OwnedFd::from_raw_fd(io::stdin().as_raw_fd()) });
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        drop(unsafe { OwnedHandle::from_raw_handle(io::stdin().as_raw_handle()) });
    }
}

/// Close the process's stdout file descriptor / handle.
///
/// This signals EOF to any process reading our output.
fn close_stdout() {
    #[cfg(unix)]
    {
        use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
        drop(unsafe { OwnedFd::from_raw_fd(io::stdout().as_raw_fd()) });
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        drop(unsafe { OwnedHandle::from_raw_handle(io::stdout().as_raw_handle()) });
    }
}

/// Check whether an I/O error is a broken pipe (remote closed receiver).
fn is_broken_pipe(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::BrokenPipe
}

/// Forward data between stdin/stdout and a UPC channel.
async fn forward_stdio(tx: impl UpcSend, mut rx: impl UpcRecv, framing: Framing, keep: bool) -> io::Result<()> {
    let mut stdin = tokio::io::stdin();
    let stdout = io::stdout();

    let interactive = std::io::stdin().is_terminal();

    let framing = match framing {
        Framing::Auto => {
            if interactive {
                Framing::Line
            } else {
                Framing::Raw
            }
        }
        other => other,
    };

    // Notify: send_fut signals recv_fut when stdin is closed on a terminal.
    let stdin_closed = Arc::new(Notify::new());

    let stdin_closed2 = stdin_closed.clone();
    let send_fut = async move {
        let max_send = tx.upc_max_size();
        let mut closed = pin!(tx.upc_closed());
        let res: io::Result<()> = match framing {
            Framing::Line => {
                let mut lines = BufReader::new(stdin).lines();
                loop {
                    tokio::select! {
                        biased;
                        () = &mut closed => break Ok(()),
                        line = lines.next_line() => match line {
                            Ok(Some(line)) => {
                                let mut data = line.into_bytes();
                                data.push(b'\n');
                                for chunk in data.chunks(max_send) {
                                    match tx.upc_send(Bytes::copy_from_slice(chunk)).await {
                                        Ok(()) => {}
                                        Err(err) if is_broken_pipe(&err) => return Ok(()),
                                        Err(err) => return Err(err),
                                    }
                                }
                            }
                            Ok(None) => break Ok(()),
                            Err(err) => break Err(err),
                        },
                    }
                }
            }
            Framing::Raw => {
                let mut buf = vec![0u8; max_send];
                loop {
                    tokio::select! {
                        biased;
                        () = &mut closed => break Ok(()),
                        result = stdin.read(&mut buf) => match result {
                            Ok(0) => break Ok(()),
                            Err(err) => break Err(err),
                            Ok(n) => {
                                match tx.upc_send(Bytes::copy_from_slice(&buf[..n])).await {
                                    Ok(()) => {}
                                    Err(err) if is_broken_pipe(&err) => return Ok(()),
                                    Err(err) => return Err(err),
                                }
                            }
                        },
                    }
                }
            }
            Framing::Auto => unreachable!(),
        };
        if !keep {
            close_stdin();
        }
        if interactive {
            stdin_closed2.notify_one();
        }
        res
    };

    let recv_fut = async move {
        let mut idle_timeout = pin!(sleep(Duration::MAX));
        let mut stdin_done = false;

        let res: io::Result<()> = loop {
            tokio::select! {
                result = rx.upc_recv() => match result {
                    Ok(Some(data)) => {
                        if stdin_done {
                            idle_timeout.as_mut().reset(Instant::now() + IDLE_TIMEOUT);
                        }
                        let mut stdout = stdout.lock();
                        if let Err(err) = stdout.write_all(&data).and_then(|_| stdout.flush()) {
                            if is_broken_pipe(&err) {
                                break Ok(());
                            }
                            break Err(err);
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(err) if is_broken_pipe(&err) => break Ok(()),
                    Err(err) => break Err(err),
                },
                () = stdin_closed.notified(), if !stdin_done => {
                    stdin_done = true;
                    idle_timeout.as_mut().reset(Instant::now() + IDLE_TIMEOUT);
                }
                () = &mut idle_timeout => break Ok(()),
            }
        };
        if !keep {
            close_stdout();
        }
        res
    };

    tokio::try_join!(send_fut, recv_fut)?;

    sleep(Duration::from_millis(100)).await;
    Ok(())
}

// ---- Argument parsing helpers ----

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("invalid hex u16: {e}"))
}

fn parse_hex_u8(s: &str) -> Result<u8, String> {
    u8::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("invalid hex u8: {e}"))
}

/// How stdin is split into packets.
#[derive(Clone, Copy, ValueEnum)]
enum Framing {
    /// Automatic: line-wise when stdin is a terminal, raw otherwise.
    Auto,
    /// Each line (delimited by newline) becomes one packet.
    Line,
    /// Each read() call becomes one packet.
    Raw,
}

// ---- Device filter (shared between host commands) ----

#[cfg(feature = "host")]
#[derive(Parser, Clone)]
struct DeviceFilter {
    /// Filter by USB vendor ID (hex).
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// Filter by USB product ID (hex).
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// Filter by USB serial number string.
    #[arg(long)]
    serial: Option<String>,

    /// Filter by USB bus identifier.
    #[arg(long)]
    bus: Option<String>,

    /// Filter by USB device address.
    #[arg(long)]
    address: Option<u8>,

    /// Only probe a specific interface number.
    #[arg(long, short)]
    interface: Option<u8>,

    /// Filter by interface subclass (hex).
    #[arg(long, value_parser = parse_hex_u8)]
    subclass: Option<u8>,

    /// Filter by interface protocol (hex).
    #[arg(long, value_parser = parse_hex_u8, default_value = DEFAULT_PROTOCOL)]
    protocol: Option<u8>,
}

#[cfg(feature = "host")]
impl DeviceFilter {
    fn matches_device(&self, dev: &nusb::DeviceInfo) -> bool {
        if let Some(vid) = self.vid {
            if dev.vendor_id() != vid {
                return false;
            }
        }
        if let Some(pid) = self.pid {
            if dev.product_id() != pid {
                return false;
            }
        }
        if let Some(serial) = &self.serial {
            if dev.serial_number() != Some(serial.as_str()) {
                return false;
            }
        }
        if let Some(bus) = &self.bus {
            if dev.bus_id() != bus {
                return false;
            }
        }
        if let Some(address) = self.address {
            if dev.device_address() != address {
                return false;
            }
        }
        true
    }

    fn matches_interface(&self, iface: &nusb::InterfaceInfo) -> bool {
        if let Some(num) = self.interface {
            if iface.interface_number() != num {
                return false;
            }
        }
        if let Some(subclass) = self.subclass {
            if iface.subclass() != subclass {
                return false;
            }
        }
        if let Some(protocol) = self.protocol {
            if iface.protocol() != protocol {
                return false;
            }
        }
        true
    }
}

// ---- CLI structure ----

/// USB packet channel (UPC) command-line tool.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Print connection events to stderr.
    #[arg(long, short)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List USB devices and probe UPC interfaces for info.
    #[cfg(feature = "host")]
    Probe(ProbeCmd),

    /// Connect to a UPC device and forward data to stdin/stdout.
    #[cfg(feature = "host")]
    Connect(ConnectCmd),

    /// Create a USB gadget with a UPC interface and wait for a connection.
    #[cfg(feature = "device")]
    Device(DeviceCmd),
}

// ---- Probe command ----

#[cfg(feature = "host")]
#[derive(Parser)]
struct ProbeCmd {
    #[command(flatten)]
    filter: DeviceFilter,

    /// Show all USB devices, not just matched UPC interfaces.
    #[arg(long)]
    all: bool,
}

#[cfg(feature = "host")]
impl ProbeCmd {
    async fn exec(&self) -> ExitCode {
        use upc::host::info;

        let devices: Vec<_> = match nusb::list_devices().await {
            Ok(iter) => iter.collect(),
            Err(err) => {
                eprintln!("Cannot enumerate USB devices: {err}");
                return ExitCode::FAILURE;
            }
        };

        let mut found = false;

        for dev_info in &devices {
            if !self.filter.matches_device(dev_info) {
                continue;
            }

            let mut upc_ifaces = Vec::new();
            for iface in dev_info.interfaces() {
                if !self.filter.matches_interface(iface) {
                    continue;
                }
                if iface.class() == Class::VENDOR_SPECIFIC || self.filter.interface.is_some() {
                    upc_ifaces.push(iface.interface_number());
                }
            }

            if !self.all && upc_ifaces.is_empty() {
                continue;
            }

            if self.all {
                println!(
                    "Device {:04x}:{:04x} on {}:{:03} - {} {}{}",
                    dev_info.vendor_id(),
                    dev_info.product_id(),
                    dev_info.bus_id(),
                    dev_info.device_address(),
                    dev_info.manufacturer_string().unwrap_or("?"),
                    dev_info.product_string().unwrap_or("?"),
                    dev_info.serial_number().map(|s| format!(" (serial: {s})")).unwrap_or_default(),
                );

                for iface in dev_info.interfaces() {
                    if !self.filter.matches_interface(iface) {
                        continue;
                    }
                    let iface_num = iface.interface_number();
                    let class = Class::new(iface.class(), iface.subclass(), iface.protocol());
                    println!(
                        "  Interface {iface_num}: class={:02x} subclass={:02x} protocol={:02x}{}",
                        class.class,
                        class.sub_class,
                        class.protocol,
                        iface.interface_string().map(|s| format!(" \"{s}\"")).unwrap_or_default(),
                    );
                }
            }

            let mut dev = None;
            for iface_num in upc_ifaces {
                let d = match &dev {
                    Some(d) => d,
                    None => match dev_info.open().await {
                        Ok(d) => dev.insert(d),
                        Err(err) => {
                            eprintln!(
                                "Cannot open device {:04x}:{:04x}: {err}",
                                dev_info.vendor_id(),
                                dev_info.product_id()
                            );
                            break;
                        }
                    },
                };

                let info_str = match info(d, iface_num).await {
                    Ok(data) => std::str::from_utf8(&data).unwrap_or("").to_string(),
                    Err(err) => {
                        eprintln!(
                            "Cannot read UPC info on {:04x}:{:04x} interface {iface_num}: {err}",
                            dev_info.vendor_id(),
                            dev_info.product_id()
                        );
                        continue;
                    }
                };

                found = true;

                if self.all {
                    println!("    UPC info: {info_str}");
                } else {
                    println!(
                        "{:04x}:{:04x}\t{}:{:03}\t{}\t{}\t{:02x}\t{}",
                        dev_info.vendor_id(),
                        dev_info.product_id(),
                        dev_info.bus_id(),
                        dev_info.device_address(),
                        dev_info.serial_number().unwrap_or(""),
                        iface_num,
                        dev_info
                            .interfaces()
                            .find(|i| i.interface_number() == iface_num)
                            .map(|i| i.subclass())
                            .unwrap_or(0),
                        info_str,
                    );
                }
            }
        }

        if !found && self.all {
            println!("No UPC interfaces found.");
        }

        ExitCode::SUCCESS
    }
}

// ---- Connect command ----

#[cfg(feature = "host")]
#[derive(Parser)]
struct ConnectCmd {
    #[command(flatten)]
    filter: DeviceFilter,

    /// Topic to send to the device when connecting.
    #[arg(long, default_value = "")]
    topic: String,

    /// Maximum packet size in bytes.
    #[arg(long, default_value_t = 65536)]
    max_packet: usize,

    /// Stdin framing mode.
    #[arg(long, value_enum, default_value_t = Framing::Auto)]
    framing: Framing,
}

#[cfg(feature = "host")]
impl ConnectCmd {
    async fn exec(self, verbose: bool) -> ExitCode {
        use upc::host::{connect_with, UpcOptions};

        let devices: Vec<_> = match nusb::list_devices().await {
            Ok(iter) => iter.collect(),
            Err(err) => {
                eprintln!("Cannot enumerate USB devices: {err}");
                return ExitCode::FAILURE;
            }
        };

        let mut found = None;
        for dev_info in &devices {
            if !self.filter.matches_device(dev_info) {
                continue;
            }

            for iface in dev_info.interfaces() {
                if !self.filter.matches_interface(iface) {
                    continue;
                }

                if iface.class() == Class::VENDOR_SPECIFIC || self.filter.interface.is_some() {
                    if found.is_some() {
                        eprintln!("Multiple matching UPC interfaces found. Use --serial, --vid, --pid, or -i to narrow down.");
                        return ExitCode::FAILURE;
                    }
                    found = Some((dev_info.clone(), iface.interface_number()));
                }
            }
        }

        let Some((dev_info, iface_num)) = found else {
            eprintln!("No matching UPC device found.");
            return ExitCode::FAILURE;
        };

        if verbose {
            eprintln!(
                "Connecting to {:04x}:{:04x} on {}:{:03} interface {iface_num}...",
                dev_info.vendor_id(),
                dev_info.product_id(),
                dev_info.bus_id(),
                dev_info.device_address(),
            );
        }

        let dev = match dev_info.open().await {
            Ok(dev) => dev,
            Err(err) => {
                eprintln!("Cannot open device: {err}");
                return ExitCode::FAILURE;
            }
        };

        let options = UpcOptions::new().with_topic(self.topic.into_bytes()).with_max_size(self.max_packet);

        let (tx, rx) = match connect_with(dev, iface_num, options).await {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("Cannot connect: {err}");
                return ExitCode::FAILURE;
            }
        };

        if verbose {
            eprintln!("Connected.");
        }

        match forward_stdio(tx, rx, self.framing, false).await {
            Ok(()) => {
                if verbose {
                    eprintln!("Disconnected.");
                }
                std::process::exit(0)
            }
            Err(err) => {
                eprintln!("Error: {err:#}");
                std::process::exit(1);
            }
        }
    }
}

// ---- Device command ----

#[cfg(feature = "device")]
#[derive(Parser)]
struct DeviceCmd {
    /// USB vendor ID (hex).
    #[arg(long, value_parser = parse_hex_u16, default_value = "1209")]
    vid: u16,

    /// USB product ID (hex).
    #[arg(long, value_parser = parse_hex_u16, default_value = "0001")]
    pid: u16,

    /// Manufacturer string.
    #[arg(long, default_value = "UPC")]
    manufacturer: String,

    /// Product string.
    #[arg(long, default_value = "UPC device")]
    product: String,

    /// Serial number string.
    #[arg(long, default_value = "")]
    serial: String,

    /// Interface subclass (hex).
    #[arg(long, value_parser = parse_hex_u8, default_value = "00")]
    subclass: u8,

    /// Interface protocol (hex).
    #[arg(long, value_parser = parse_hex_u8, default_value = DEFAULT_PROTOCOL)]
    protocol: u8,

    /// Interface name.
    #[arg(long)]
    name: Option<String>,

    /// Device interface GUID for Microsoft OS.
    #[arg(long, default_value = DEFAULT_GUID)]
    guid: Option<String>,

    /// Info string to provide to host.
    #[arg(long, default_value = "")]
    info: String,

    /// Maximum receive packet size in bytes.
    #[arg(long)]
    max_packet: Option<u64>,

    /// Stdin framing mode.
    #[arg(long, value_enum, default_value_t = Framing::Auto)]
    framing: Framing,

    /// Keep running and accept new connections after the current one ends.
    #[arg(long, short)]
    keep: bool,
}

#[cfg(feature = "device")]
impl DeviceCmd {
    async fn exec(self, verbose: bool) -> ExitCode {
        use std::process;

        use upc::device::{InterfaceId, UpcFunction};
        use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};

        let class = Class::vendor_specific(self.subclass, self.protocol);
        let mut iface_id = InterfaceId::new(class);
        if let Some(name) = &self.name {
            iface_id = iface_id.with_name(name);
        }
        if let Some(guid_str) = &self.guid {
            let guid = match uuid::Uuid::parse_str(guid_str) {
                Ok(g) => g,
                Err(err) => {
                    eprintln!("Invalid GUID: {err}");
                    return ExitCode::FAILURE;
                }
            };
            iface_id = iface_id.with_guid(guid);
        }

        let (mut upc, hnd) = UpcFunction::new(iface_id);

        if !self.info.is_empty() {
            upc.set_info(self.info.as_bytes().to_vec()).await;
        }

        if let Some(size) = self.max_packet {
            upc.set_max_size(size).await;
        }

        let udc = match default_udc() {
            Ok(udc) => udc,
            Err(err) => {
                eprintln!("Cannot get UDC: {err}");
                return ExitCode::FAILURE;
            }
        };

        let gadget = Gadget::new(
            class.into(),
            Id::new(self.vid, self.pid),
            Strings::new(&self.manufacturer, &self.product, &self.serial),
        )
        .with_config(Config::new("config").with_function(hnd))
        .with_os_descriptor(OsDescriptor::microsoft());

        let _reg = match gadget.bind(&udc) {
            Ok(reg) => reg,
            Err(err) => {
                eprintln!("Cannot bind gadget: {err}");
                return ExitCode::FAILURE;
            }
        };

        if verbose {
            eprintln!("Waiting for connection...");
        }

        loop {
            let (tx, rx) = match upc.accept().await {
                Ok(conn) => conn,
                Err(err) => {
                    eprintln!("Accept failed: {err}");
                    return ExitCode::FAILURE;
                }
            };

            if verbose {
                eprintln!("Connection accepted.");
            }

            match forward_stdio(tx, rx, self.framing, self.keep).await {
                Ok(()) => {
                    if verbose {
                        eprintln!("Connection ended.");
                    }
                    if !self.keep {
                        process::exit(0);
                    }
                }
                Err(err) => {
                    eprintln!("Connection error: {err:#}");
                    if !self.keep {
                        process::exit(1);
                    }
                }
            }

            if verbose {
                eprintln!("Waiting for next connection...");
            }
        }
    }
}

// ---- Main ----

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    tracing_log::LogTracer::init().ok();

    let cli = Cli::parse();

    match cli.command {
        #[cfg(feature = "host")]
        Command::Probe(cmd) => cmd.exec().await,
        #[cfg(feature = "host")]
        Command::Connect(cmd) => cmd.exec(cli.verbose).await,
        #[cfg(feature = "device")]
        Command::Device(cmd) => cmd.exec(cli.verbose).await,
    }
}
