//! USB packet channel (UPC) command-line tool.
//!
//! Install with:
//! - Host side: `cargo install upc --features cli,host`
//! - Device side: `cargo install upc --features cli,device`
//! - Both: `cargo install upc --features cli,host,device`

#[cfg(not(any(feature = "host", feature = "device")))]
compile_error!("The `cli` feature requires at least one of `host` or `device` features to be enabled.");

use std::{
    io::{self, Write},
    process::ExitCode,
};

use bytes::Bytes;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use upc::Class;

/// USB packet channel (UPC) command-line tool.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List USB devices and probe UPC interfaces for info.
    #[cfg(feature = "host")]
    Probe {
        #[command(flatten)]
        filter: DeviceFilter,
    },

    /// Connect to a UPC device and forward data to stdin/stdout.
    #[cfg(feature = "host")]
    Connect {
        #[command(flatten)]
        filter: DeviceFilter,

        /// Topic to send to the device when connecting.
        #[arg(long, default_value = "")]
        topic: String,

        /// Maximum packet size in bytes.
        #[arg(long, default_value_t = 65536)]
        max_packet: usize,

        /// Stdin framing mode.
        #[arg(long, value_enum, default_value_t = Framing::Line)]
        framing: Framing,
    },

    /// Create a USB gadget with a UPC interface and wait for a connection.
    #[cfg(feature = "device")]
    Device {
        /// USB vendor ID (hex, e.g. 1984).
        #[arg(long, value_parser = parse_hex_u16)]
        vid: u16,

        /// USB product ID (hex, e.g. 0902).
        #[arg(long, value_parser = parse_hex_u16)]
        pid: u16,

        /// Manufacturer string.
        #[arg(long, default_value = "")]
        manufacturer: String,

        /// Product string.
        #[arg(long, default_value = "")]
        product: String,

        /// Serial number string.
        #[arg(long, default_value = "")]
        serial: String,

        /// Interface subclass (hex, e.g. 01).
        #[arg(long, value_parser = parse_hex_u8, default_value = "01")]
        subclass: u8,

        /// Interface protocol (hex, e.g. 00).
        #[arg(long, value_parser = parse_hex_u8, default_value = "00")]
        protocol: u8,

        /// Interface name.
        #[arg(long)]
        name: Option<String>,

        /// Device interface GUID for Microsoft OS.
        #[arg(long)]
        guid: Option<String>,

        /// Info string to provide to host.
        #[arg(long, default_value = "")]
        info: String,

        /// Maximum packet size in bytes.
        #[arg(long, default_value_t = 65536)]
        max_packet: usize,

        /// Stdin framing mode.
        #[arg(long, value_enum, default_value_t = Framing::Line)]
        framing: Framing,
    },
}

#[cfg(feature = "host")]
#[derive(Parser, Clone)]
struct DeviceFilter {
    /// Filter by USB vendor ID (hex, e.g. 1984).
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// Filter by USB product ID (hex, e.g. 0902).
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// Only probe a specific interface number.
    #[arg(long, short)]
    interface: Option<u8>,

    /// Filter by interface class (hex, e.g. ff).
    #[arg(long, value_parser = parse_hex_u8)]
    class: Option<u8>,

    /// Filter by interface subclass (hex, e.g. 01).
    #[arg(long, value_parser = parse_hex_u8)]
    subclass: Option<u8>,

    /// Filter by interface protocol (hex, e.g. 01).
    #[arg(long, value_parser = parse_hex_u8)]
    protocol: Option<u8>,
}

/// How stdin is split into packets.
#[derive(Clone, Copy, ValueEnum)]
enum Framing {
    /// Each line (delimited by newline) becomes one packet.
    Line,
    /// Each read() call becomes one packet.
    Raw,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    u16::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("invalid hex u16: {e}"))
}

fn parse_hex_u8(s: &str) -> Result<u8, String> {
    u8::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16)
        .map_err(|e| format!("invalid hex u8: {e}"))
}

#[cfg(feature = "host")]
fn matches_filter(dev_info: &nusb::DeviceInfo, filter: &DeviceFilter) -> bool {
    if let Some(vid) = filter.vid {
        if dev_info.vendor_id() != vid {
            return false;
        }
    }
    if let Some(pid) = filter.pid {
        if dev_info.product_id() != pid {
            return false;
        }
    }
    true
}

#[cfg(feature = "host")]
fn matches_iface_filter(iface: &nusb::InterfaceInfo, filter: &DeviceFilter) -> bool {
    if let Some(num) = filter.interface {
        if iface.interface_number() != num {
            return false;
        }
    }
    if let Some(class) = filter.class {
        if iface.class() != class {
            return false;
        }
    }
    if let Some(subclass) = filter.subclass {
        if iface.subclass() != subclass {
            return false;
        }
    }
    if let Some(protocol) = filter.protocol {
        if iface.protocol() != protocol {
            return false;
        }
    }
    true
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        #[cfg(feature = "host")]
        Command::Probe { filter } => cmd_probe(filter).await,
        #[cfg(feature = "host")]
        Command::Connect { filter, topic, max_packet, framing } => {
            cmd_connect(filter, topic, max_packet, framing).await
        }
        #[cfg(feature = "device")]
        Command::Device {
            vid,
            pid,
            manufacturer,
            product,
            serial,
            subclass,
            protocol,
            name,
            guid,
            info,
            max_packet,
            framing,
        } => {
            cmd_device(
                vid,
                pid,
                manufacturer,
                product,
                serial,
                subclass,
                protocol,
                name,
                guid,
                info,
                max_packet,
                framing,
            )
            .await
        }
    }
}

#[cfg(feature = "host")]
async fn cmd_probe(filter: DeviceFilter) -> ExitCode {
    use upc::host::info;

    let devices: Vec<_> = match nusb::list_devices().await {
        Ok(iter) => iter.collect(),
        Err(err) => {
            eprintln!("Cannot enumerate USB devices: {err}");
            return ExitCode::FAILURE;
        }
    };

    if devices.is_empty() {
        println!("No USB devices found.");
        return ExitCode::SUCCESS;
    }

    let mut probed = false;

    for dev_info in &devices {
        if !matches_filter(dev_info, &filter) {
            continue;
        }

        println!(
            "Device {:04x}:{:04x} - {} {}",
            dev_info.vendor_id(),
            dev_info.product_id(),
            dev_info.manufacturer_string().unwrap_or("?"),
            dev_info.product_string().unwrap_or("?"),
        );

        let mut dev = None;

        for iface in dev_info.interfaces() {
            if !matches_iface_filter(&iface, &filter) {
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

            if class.class == Class::VENDOR_SPECIFIC {
                let d = match &dev {
                    Some(d) => d,
                    None => match dev_info.open().await {
                        Ok(d) => dev.insert(d),
                        Err(err) => {
                            eprintln!("    Cannot open device: {err}");
                            continue;
                        }
                    },
                };

                match info(d, iface_num).await {
                    Ok(data) => {
                        probed = true;
                        match std::str::from_utf8(&data) {
                            Ok(s) => println!("    UPC info ({} bytes): {s}", data.len()),
                            Err(_) => println!("    UPC info ({} bytes): {:?}", data.len(), data),
                        }
                    }
                    Err(err) => {
                        eprintln!("    Cannot read UPC info: {err}");
                    }
                }
            }
        }
    }

    if !probed {
        println!("\nNo UPC interfaces found.");
    }

    ExitCode::SUCCESS
}

#[cfg(feature = "host")]
async fn cmd_connect(filter: DeviceFilter, topic: String, max_packet: usize, framing: Framing) -> ExitCode {
    use upc::host::connect;

    // Find the device.
    let devices: Vec<_> = match nusb::list_devices().await {
        Ok(iter) => iter.collect(),
        Err(err) => {
            eprintln!("Cannot enumerate USB devices: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Find a matching device with a UPC interface.
    let mut found = None;
    for dev_info in &devices {
        if !matches_filter(dev_info, &filter) {
            continue;
        }

        for iface in dev_info.interfaces() {
            if !matches_iface_filter(&iface, &filter) {
                continue;
            }

            // For connect, require vendor-specific class or explicit interface selection.
            if iface.class() == Class::VENDOR_SPECIFIC || filter.interface.is_some() {
                if found.is_some() {
                    eprintln!("Multiple matching UPC interfaces found. Use --vid, --pid, or -i to narrow down.");
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

    eprintln!(
        "Connecting to {:04x}:{:04x} ({}) interface {iface_num}...",
        dev_info.vendor_id(),
        dev_info.product_id(),
        dev_info.product_string().unwrap_or("?"),
    );

    let dev = match dev_info.open().await {
        Ok(dev) => dev,
        Err(err) => {
            eprintln!("Cannot open device: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (tx, mut rx) = match connect(dev, iface_num, topic.as_bytes()).await {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("Cannot connect: {err}");
            return ExitCode::FAILURE;
        }
    };

    if max_packet > 0 {
        rx.set_max_size(max_packet);
    }

    eprintln!("Connected. Forwarding data...");

    let mut stdin = tokio::io::stdin();
    let stdout = io::stdout();

    // Forward stdin -> UPC and UPC -> stdout concurrently.
    let send_task = tokio::spawn(async move {
        match framing {
            Framing::Line => {
                let mut lines = BufReader::new(stdin).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Err(err) = tx.send(Bytes::from(line)).await {
                        eprintln!("UPC send error: {err}");
                        break;
                    }
                }
            }
            Framing::Raw => {
                let mut buf = vec![0u8; max_packet];
                loop {
                    let n = match stdin.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(err) => {
                            eprintln!("stdin read error: {err}");
                            break;
                        }
                    };
                    if let Err(err) = tx.send(Bytes::copy_from_slice(&buf[..n])).await {
                        eprintln!("UPC send error: {err}");
                        break;
                    }
                }
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    let mut stdout = stdout.lock();
                    if let Err(err) = stdout.write_all(&data).and_then(|_| stdout.flush()) {
                        eprintln!("stdout write error: {err}");
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("UPC recv error: {err}");
                    break;
                }
            }
        }
    });

    // Wait for either direction to finish.
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    ExitCode::SUCCESS
}

// --- Device command ---

#[cfg(feature = "device")]
#[allow(clippy::too_many_arguments)]
async fn cmd_device(
    vid: u16, pid: u16, manufacturer: String, product: String, serial: String, subclass: u8, protocol: u8,
    name: Option<String>, guid: Option<String>, info_str: String, max_packet: usize, framing: Framing,
) -> ExitCode {
    use upc::device::{InterfaceId, UpcFunction};
    use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};

    let class = Class::vendor_specific(subclass, protocol);
    let mut iface_id = InterfaceId::new(class);
    if let Some(name) = name {
        iface_id = iface_id.with_name(name);
    }
    if let Some(guid_str) = guid {
        let guid = match uuid::Uuid::parse_str(&guid_str) {
            Ok(g) => g,
            Err(err) => {
                eprintln!("Invalid GUID: {err}");
                return ExitCode::FAILURE;
            }
        };
        iface_id = iface_id.with_guid(guid);
    }

    let (mut upc, hnd) = UpcFunction::new(iface_id);

    if !info_str.is_empty() {
        upc.set_info(info_str.into_bytes()).await;
    }

    let udc = match default_udc() {
        Ok(udc) => udc,
        Err(err) => {
            eprintln!("Cannot get UDC: {err}");
            return ExitCode::FAILURE;
        }
    };

    let gadget = Gadget::new(class.into(), Id::new(vid, pid), Strings::new(&manufacturer, &product, &serial))
        .with_config(Config::new("config").with_function(hnd))
        .with_os_descriptor(OsDescriptor::microsoft());

    let _reg = match gadget.bind(&udc) {
        Ok(reg) => reg,
        Err(err) => {
            eprintln!("Cannot bind gadget: {err}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("Gadget bound to UDC. Waiting for connection...");

    let (tx, mut rx) = match upc.accept().await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("Accept failed: {err}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!("Connected. Topic: {:?}", String::from_utf8_lossy(rx.topic()));

    if max_packet > 0 {
        rx.set_max_size(max_packet);
    }

    eprintln!("Forwarding data...");

    let mut stdin = tokio::io::stdin();
    let stdout = io::stdout();

    let send_task = tokio::spawn(async move {
        match framing {
            Framing::Line => {
                let mut lines = BufReader::new(stdin).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Err(err) = tx.send(Bytes::from(line)).await {
                        eprintln!("UPC send error: {err}");
                        break;
                    }
                }
            }
            Framing::Raw => {
                let mut buf = vec![0u8; max_packet];
                loop {
                    let n = match stdin.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(err) => {
                            eprintln!("stdin read error: {err}");
                            break;
                        }
                    };
                    if let Err(err) = tx.send(Bytes::copy_from_slice(&buf[..n])).await {
                        eprintln!("UPC send error: {err}");
                        break;
                    }
                }
            }
        }
    });

    let recv_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(data) => {
                    let mut stdout = stdout.lock();
                    if let Err(err) = stdout.write_all(&data).and_then(|_| stdout.flush()) {
                        eprintln!("stdout write error: {err}");
                        break;
                    }
                }
                Err(err) => {
                    eprintln!("UPC recv error: {err}");
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    ExitCode::SUCCESS
}
