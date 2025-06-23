//! In-use guard.

use std::{
    collections::HashSet,
    io::{Error, ErrorKind, Result},
    sync::{LazyLock, Mutex},
};

static IN_USE: LazyLock<Mutex<HashSet<(usize, u8)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

pub(crate) struct InUseGuard {
    handle: usize,
    interface: u8,
}

impl InUseGuard {
    pub fn new(handle: usize, interface: u8) -> Result<Self> {
        let mut in_use = IN_USE.lock().unwrap();

        if in_use.contains(&(handle, interface)) {
            return Err(Error::new(ErrorKind::ResourceBusy, "interface is used by another UPC channel"));
        }

        in_use.insert((handle, interface));

        Ok(Self { handle, interface })
    }
}

impl Drop for InUseGuard {
    fn drop(&mut self) {
        let mut in_use = IN_USE.lock().unwrap();
        in_use.remove(&(self.handle, self.interface));
    }
}
