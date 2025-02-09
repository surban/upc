use std::fmt;
use tokio::sync::oneshot;
use wasm_bindgen::{prelude::*, JsCast};
use web_sys::Event;

/// Log to console.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        web_sys::console::log_1(&::wasm_bindgen::JsValue::from_str(&format!($($arg)*)));
    }
}

#[track_caller]
pub fn log_and_panic(msg: &str) -> ! {
    web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(msg));
    panic!("{msg}")
}

#[macro_export]
macro_rules! panic_log {
    ($($arg:tt)*) => {
        $crate::util::log_and_panic(&format!($($arg)*))
    }
}

pub trait ResultExt<T> {
    #[track_caller]
    fn expect_log(self, msg: &str) -> T;
}

impl<T, E> ResultExt<T> for Result<T, E>
where
    E: fmt::Display,
{
    #[track_caller]
    fn expect_log(self, msg: &str) -> T {
        match self {
            Ok(v) => v,
            Err(err) => panic_log!("{msg}: {err}"),
        }
    }
}

impl<T> ResultExt<T> for Option<T> {
    #[track_caller]
    fn expect_log(self, msg: &str) -> T {
        match self {
            Some(v) => v,
            None => panic_log!("{msg}"),
        }
    }
}

/// Wait for a click event on the page until user interaction is enabled.
pub async fn wait_for_interaction(msg: &str) {
    log!("Waiting for user interaction on page");

    let window = web_sys::window().unwrap();
    let document = window.document().unwrap();
    let body = document.body().unwrap();

    let message = document.create_element("div").unwrap();
    message.set_inner_html(msg);
    body.append_child(&message).unwrap();

    let click_future = async {
        let (sender, receiver) = oneshot::channel::<()>();
        let closure = Closure::once(move |_event: Event| {
            let _ = sender.send(());
        });
        body.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref()).unwrap();
        receiver.await.unwrap();
        body.remove_event_listener_with_callback("click", closure.as_ref().unchecked_ref()).unwrap();
    };

    click_future.await;

    body.remove_child(&message).unwrap();
}
