use std::sync::atomic::{AtomicU32, Ordering};

static GLOBAL_SYSTEM_BYTES: AtomicU32 = AtomicU32::new(1);

pub fn next_system_bytes() -> u32 {
    GLOBAL_SYSTEM_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(if current == u32::MAX { 1 } else { current + 1 })
        })
        .unwrap()
}
