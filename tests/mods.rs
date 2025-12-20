#![feature(maybe_uninit_write_slice)]

use extern_trait::extern_trait;
use starry_vm::{VmError, VmIo, VmResult};
use std::{
    mem::MaybeUninit,
    sync::{LazyLock, Mutex, MutexGuard},
    thread,
    time::Duration,
};

pub fn wait_until<F>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

pub static POOL: LazyLock<Mutex<Box<[u8]>>> = LazyLock::new(|| {
    let size = 0x0100_0000; // 16 MiB
    Mutex::new(vec![0; size].into_boxed_slice())
});

pub struct Vm(MutexGuard<'static, Box<[u8]>>);

#[extern_trait]
unsafe impl VmIo for Vm {
    fn new() -> Self {
        let pool = POOL.lock().unwrap();
        Vm(pool)
    }

    fn read(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> VmResult {
        let base = self.0.as_ptr() as usize;
        let offset = start.checked_sub(base).ok_or(VmError::BadAddress)?;
        if offset.checked_add(buf.len()).ok_or(VmError::BadAddress)? > self.0.len() {
            return Err(VmError::BadAddress);
        }
        let slice = &self.0[offset..offset + buf.len()];
        buf.write_copy_of_slice(slice);
        Ok(())
    }

    fn write(&mut self, start: usize, buf: &[u8]) -> VmResult {
        let base = self.0.as_ptr() as usize;
        let offset = start.checked_sub(base).ok_or(VmError::BadAddress)?;
        if offset.checked_add(buf.len()).ok_or(VmError::BadAddress)? > self.0.len() {
            return Err(VmError::BadAddress);
        }
        let slice = &mut self.0[offset..offset + buf.len()];
        slice.copy_from_slice(buf);
        Ok(())
    }
}
