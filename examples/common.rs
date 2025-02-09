use rand::prelude::*;
use rand_xoshiro::Xoshiro128StarStar;
use std::{collections::VecDeque, time::Duration};

use upc::Class;

pub const VID: u16 = 4;
pub const PID: u16 = 5;

pub const CLASS: Class = Class::vendor_specific(22, 3);
pub const TOPIC: &[u8] = b"TEST TOPIC";
pub const INFO: &[u8] = b"TEST INFO";

pub const HOST_SEED: u64 = 12523;
pub const DEVICE_SEED: u64 = 23152;
pub const TEST_PACKETS: usize = 1_000;
pub const TEST_PACKET_MAX_SIZE: usize = 1_000_000;

pub const DELAY: bool = false;

#[cfg(not(feature = "web"))]
pub fn init_log() {
    use std::sync::Once;
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::registry().with(fmt::layer()).with(EnvFilter::from_default_env()).init();
        tracing_log::LogTracer::init().unwrap();
    });
}

#[cfg(not(feature = "web"))]
use tokio::time::sleep;

#[cfg(feature = "web")]
pub async fn sleep(duration: Duration) {
    use js_sys::Promise;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::window;

    let ms = duration.as_millis() as i32;
    let promise = Promise::new(&mut |resolve, _reject| {
        let window = window().unwrap();
        window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms).unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}

pub struct TestData {
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
            None => self.rng.random_range(0..self.max_length),
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

pub struct TestDelayer {
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

        if self.rng.random_ratio(1, 1000) {
            let ms = self.rng.random_range(0..1000);
            sleep(Duration::from_millis(ms)).await;
        }
    }
}
