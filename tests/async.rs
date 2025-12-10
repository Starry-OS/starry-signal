use std::{
    future::Future,
    mem::{MaybeUninit, zeroed},
    ptr,
    sync::Arc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use axcpu::uspace::UserContext;
use extern_trait::extern_trait;
use kspin::SpinNoIrq;
use starry_signal::{
    SignalDisposition, SignalInfo, SignalOSAction, SignalSet, Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};
use starry_vm::VmResult;
use tokio::time;

async fn wait_until<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    time::timeout(timeout, async {
        loop {
            if check().await {
                break;
            }
            time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .is_ok()
}

struct TestEnv {
    actions: Arc<SpinNoIrq<SignalActions>>,
    proc: Arc<ProcessSignalManager>,
    thr: Arc<ThreadSignalManager>,
}

impl TestEnv {
    fn new() -> Self {
        let actions = Arc::new(SpinNoIrq::new(SignalActions::default()));
        let proc = Arc::new(ProcessSignalManager::new(actions.clone(), 0));
        let thr = ThreadSignalManager::new(7, proc.clone());
        Self { actions, proc, thr }
    }
}

#[derive(Clone, Copy)]
struct DummyVm;

#[extern_trait]
unsafe impl starry_vm::VmIo for DummyVm {
    fn new() -> Self {
        DummyVm
    }

    fn read(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> VmResult {
        unsafe {
            let dst = buf.as_mut_ptr() as *mut u8;
            let src = start as *const u8;
            ptr::copy_nonoverlapping(src, dst, buf.len());
        }
        Ok(())
    }

    fn write(&mut self, start: usize, buf: &[u8]) -> VmResult {
        unsafe {
            let dst = start as *mut u8;
            ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        }
        Ok(())
    }
}

#[tokio::test]
async fn async_send_signal() {
    let env = TestEnv::new();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 9, 9);

    let thr = env.thr.clone();
    tokio::spawn(async move {
        time::sleep(Duration::from_millis(10)).await;
        let _ = thr.send_signal(sig);
    });

    let res = wait_until(Duration::from_millis(100), || async {
        env.thr.pending().has(Signo::SIGTERM) || env.proc.pending().has(Signo::SIGTERM)
    })
    .await;

    assert!(res);
}

#[tokio::test]
async fn async_blocked() {
    let env = TestEnv::new();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 9, 9);

    let mut blocked = SignalSet::default();
    blocked.add(Signo::SIGTERM);
    let prev = env.thr.set_blocked(blocked);
    assert!(!prev.has(Signo::SIGTERM));
    assert!(env.thr.signal_blocked(Signo::SIGTERM));

    let thr = env.thr.clone();
    tokio::spawn(async move {
        time::sleep(Duration::from_millis(10)).await;
        let _ = thr.send_signal(sig);
    });

    let pending_res = wait_until(Duration::from_millis(100), || async {
        env.thr.pending().has(Signo::SIGTERM)
    })
    .await;
    assert!(pending_res);

    env.thr.set_blocked(SignalSet::default());
    assert!(!env.thr.signal_blocked(Signo::SIGTERM));

    let uctx = Arc::new(SpinNoIrq::new(unsafe { zeroed::<UserContext>() }));
    uctx.lock().set_sp(0x8000_0000);
    let res = wait_until(Duration::from_millis(100), || async {
        let mut uctx_ref = uctx.lock().clone();
        if let Some((si, _)) = env.thr.check_signals(&mut uctx_ref, None) {
            assert_eq!(si.signo(), Signo::SIGTERM);
            true
        } else {
            false
        }
    })
    .await;
    assert!(res);
}

#[tokio::test]
async fn async_handler() {
    unsafe extern "C" fn test_handler(_: i32) {}

    let env = TestEnv::new();
    env.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Handler(test_handler);

    let mut stack = vec![0u8; 16 * 1024].into_boxed_slice();
    let initial_sp = stack.as_mut_ptr() as usize + stack.len();
    let uctx = Arc::new(SpinNoIrq::new(unsafe { zeroed::<UserContext>() }));
    uctx.lock().set_sp(initial_sp);

    let first = SignalInfo::new_user(Signo::SIGTERM, 9, 9);
    assert!(env.thr.send_signal(first.clone()));
    let (si, action) = {
        let mut guard = uctx.lock();
        env.thr.check_signals(&mut guard, None).unwrap()
    };
    assert_eq!(si.signo(), Signo::SIGTERM);
    assert!(matches!(action, SignalOSAction::Handler));
    assert!(env.thr.signal_blocked(Signo::SIGTERM));

    let thr = env.thr.clone();
    tokio::spawn(async move {
        let _ = thr.send_signal(SignalInfo::new_user(Signo::SIGINT, 2, 2));
        let _ = thr.send_signal(SignalInfo::new_user(Signo::SIGTERM, 3, 3));
    });

    let pending_res = wait_until(Duration::from_millis(200), || async {
        env.thr.pending().has(Signo::SIGTERM)
    })
    .await;
    assert!(pending_res);

    let pending_res = wait_until(Duration::from_millis(200), || async {
        env.thr.pending().has(Signo::SIGINT)
    })
    .await;
    assert!(pending_res);

    let frame_sp = uctx.lock().sp() + 8;
    {
        let mut guard = uctx.lock();
        guard.set_sp(frame_sp);
        env.thr.restore(&mut guard);
    }
    assert!(!env.thr.signal_blocked(Signo::SIGTERM));

    let delivered = Arc::new(AtomicUsize::new(0));
    let delivered_result = {
        wait_until(Duration::from_millis(200), move || {
            let thr = env.thr.clone();
            let delivered_ref = delivered.clone();
            let uctx_ref = uctx.clone();
            async move {
                if let Some((sig, _)) = thr.check_signals(&mut *uctx_ref.lock(), None) {
                    assert!(matches!(sig.signo(), Signo::SIGINT | Signo::SIGTERM));
                    delivered_ref.fetch_add(1, Ordering::SeqCst);
                }
                delivered_ref.load(Ordering::SeqCst) >= 2
            }
        })
        .await
    };

    assert!(delivered_result);
    drop(stack);
}
