use rand::prelude::*;
use rand_xoshiro::Xoshiro128StarStar;
use std::{
    collections::VecDeque,
    sync::Once,
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use upc::Class;

const VID: u16 = 4;
const PID: u16 = 5;

const CLASS: Class = Class::vendor_specific(22, 3);
const NAME: &str = "USB-PACKET-TEST";
const TOPIC: &[u8] = b"TEST TOPIC";
const INFO: &[u8] = b"TEST INFO";

const HOST_SEED: u64 = 12523;
const DEVICE_SEED: u64 = 23152;
const TEST_PACKETS: usize = 1_000;
const TEST_PACKET_MAX_SIZE: usize = 1_000_000;

const DELAY: bool = false;

fn init_log() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
        tracing_log::LogTracer::init().unwrap();
    });
}

struct TestData {
    rng: Xoshiro128StarStar,
    max_length: usize,
    pre_lengths: VecDeque<usize>,
}

impl TestData {
    pub fn new(seed: u64, max_length: usize) -> Self {
        Self {
            rng: Xoshiro128StarStar::seed_from_u64(seed),
            max_length,
            pre_lengths: [
                0,
                1,
                2,
                3,
                511,
                512,
                513,
                1023,
                1024,
                1025,
                0,
                2000,
                2048,
                0,
                4096,
                5000,
                8191,
                8192,
                8193,
                0,
                8193,
                TEST_PACKET_MAX_SIZE,
            ]
            .into(),
        }
    }

    pub fn generate(&mut self) -> Vec<u8> {
        let len = match self.pre_lengths.pop_front() {
            Some(len) => len,
            None => self.rng.gen_range(0..self.max_length),
        };
        let mut data = vec![0; len];
        self.rng.fill_bytes(&mut data);
        data
    }

    pub fn validate(&mut self, data: &[u8]) {
        let expected = self.generate();
        assert_eq!(data.len(), expected.len(), "data length mismatch");
        assert_eq!(data, &expected, "data mismatch");
    }
}

struct TestDelayer {
    rng: Xoshiro128StarStar,
}

impl TestDelayer {
    pub fn new(seed: u64) -> Self {
        Self { rng: Xoshiro128StarStar::seed_from_u64(seed) }
    }

    pub async fn delay(&mut self) {
        if !DELAY {
            return;
        }

        if self.rng.gen_ratio(1, 1000) {
            let ms = self.rng.gen_range(0..1000);
            sleep(Duration::from_millis(ms)).await;
        }
    }
}

#[cfg(feature = "host")]
#[tokio::test]
#[ignore = "device-side companion test required"]
async fn host() {
    use upc::host::{connect, find_interface, info};

    init_log();

    let dev = rusb::open_device_with_vid_pid(VID, PID).expect("device not found").device();
    println!("Using device: {dev:?}");

    println!("Finding interface...");
    let iface = find_interface(&dev, CLASS, Some(NAME)).expect("cannot find interface");
    println!("Using interface {iface}");

    println!("Getting info...");
    let info = info(&dev, iface).expect("cannot get info");
    println!("Info: {}", String::from_utf8_lossy(&info));
    assert_eq!(info, INFO, "info mismatch");

    println!("Connecting...");
    let (tx, mut rx) = connect(&dev, iface, TOPIC).await.expect("cannot connect");

    let rx_task = tokio::spawn(async move {
        let mut rx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
        let mut rx_delay = TestDelayer::new(DEVICE_SEED);

        let start = Instant::now();
        let mut total = 0;

        println!("Receiving...");
        for n in 0..TEST_PACKETS {
            let data = rx.recv().await.expect("receive failed");
            eprintln!("Recv {n}: {} bytes", data.len());
            total += data.len();
            rx_testdata.validate(&data);
            rx_delay.delay().await;
        }

        let elapsed = start.elapsed().as_secs_f32();
        println!("Received {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

        println!("Waiting for receiver close");
        rx.recv().await.expect_err("receiver not closed");
        println!("Receiver closed");
    });

    let mut tx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
    let mut tx_delay = TestDelayer::new(HOST_SEED);

    let start = Instant::now();
    let mut total = 0;

    println!("Sending");
    for n in 0..TEST_PACKETS {
        let data = tx_testdata.generate();
        let len = data.len();
        tx.send(data).await.expect("send failed");
        total += len;
        tx_delay.delay().await;

        eprintln!("Send {n}: {len} bytes");
    }

    let elapsed = start.elapsed().as_secs_f32();
    println!("Sent {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

    sleep(Duration::from_secs(1)).await;

    println!("Disconnecting...");
    drop(tx);
    rx_task.await.unwrap();
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}

#[cfg(feature = "device")]
#[tokio::test]
#[ignore = "host-side companion test required"]
async fn device() {
    use upc::device::{InterfaceId, UpcFunction};
    use usb_gadget::{default_udc, Config, Gadget, Id, OsDescriptor, Strings};
    use uuid::uuid;

    const DEVICE_CLASS: Class = Class::vendor_specific(0xff, 0);

    init_log();
    usb_gadget::remove_all().expect("cannot remove all USB gadgets");
    sleep(Duration::from_secs(1)).await;

    println!("Creating UPC function...");
    let (mut upc, hnd) = UpcFunction::new(
        InterfaceId::new(CLASS).with_name(NAME).with_guid(uuid!("3bf77270-42d2-42c6-a475-490227a9cc89")),
    );
    upc.set_info(INFO.to_vec()).await;

    println!("Registering gadget...");
    let udc = default_udc().expect("cannot get UDC");
    let mut gadget = Gadget::new(DEVICE_CLASS.into(), Id::new(VID, PID), Strings::new("usb-packet", "test", "0"))
        .with_config(Config::new("config").with_function(hnd))
        .with_os_descriptor(OsDescriptor::microsoft());
    gadget.device_release = 0x0110;
    let reg = gadget.bind(&udc).expect("cannot bind to UDC");
    assert!(reg.is_attached());

    println!("Waiting for connection...");
    let (tx, mut rx) = upc.accept().await.expect("accept failed");
    assert_eq!(rx.topic(), TOPIC, "wrong topic");

    let rx_task = tokio::spawn(async move {
        let mut rx_testdata = TestData::new(HOST_SEED, TEST_PACKET_MAX_SIZE);
        let mut rx_delay = TestDelayer::new(HOST_SEED);

        let start = Instant::now();
        let mut total = 0;

        println!("Receiving...");
        for n in 0..TEST_PACKETS {
            let data = rx.recv().await.expect("receive failed");
            eprintln!("Recv {n}: {} bytes", data.len());
            total += data.len();
            rx_testdata.validate(&data);
            rx_delay.delay().await;
        }

        let elapsed = start.elapsed().as_secs_f32();
        println!("Received {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

        println!("Waiting for receiver close");
        rx.recv().await.expect_err("receiver not closed");
        println!("Receiver closed");
    });

    let mut tx_testdata = TestData::new(DEVICE_SEED, TEST_PACKET_MAX_SIZE);
    let mut tx_delay = TestDelayer::new(DEVICE_SEED);

    let start = Instant::now();
    let mut total = 0;

    println!("Sending");
    for n in 0..TEST_PACKETS {
        let data = tx_testdata.generate();
        let len = data.len();
        tx.send(data).await.expect("send failed");
        total += len;
        tx_delay.delay().await;

        eprintln!("Send {n}: {len} bytes");
    }

    let elapsed = start.elapsed().as_secs_f32();
    println!("Sent {total} bytes in {elapsed:.2} seconds: {} MB/s", total as f32 / elapsed / 1_048_576.);

    sleep(Duration::from_secs(1)).await;

    println!("Waiting for receiver...");
    rx_task.await.unwrap();
    drop(tx);
    println!("Disconnected");

    sleep(Duration::from_secs(1)).await;
}
