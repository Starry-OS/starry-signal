use std::{
    mem::{MaybeUninit, zeroed},
    ptr,
    sync::Arc,
};

use axcpu::uspace::UserContext;
use extern_trait::extern_trait;
use kspin::SpinNoIrq;
use starry_signal::{
    SignalDisposition, SignalInfo, SignalOSAction, SignalSet, Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};
use starry_vm::VmResult;

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

#[test]
fn block_ignore_send_signal() {
    let env = TestEnv::new();
    let actions = env.actions.clone();
    let sig = SignalInfo::new_user(Signo::SIGINT, 0, 1);
    assert!(env.thr.send_signal(sig.clone()));

    actions.lock()[Signo::SIGINT].disposition = SignalDisposition::Ignore;
    let proc_ignore = Arc::new(ProcessSignalManager::new(actions.clone(), 0));
    let thr_ignore = ThreadSignalManager::new(7, proc_ignore.clone());
    assert!(!thr_ignore.send_signal(sig.clone()));

    let mut set = SignalSet::default();
    set.add(Signo::SIGINT);
    env.thr.set_blocked(set);
    assert!(!env.thr.send_signal(sig.clone()));
    assert!(env.thr.pending().has(Signo::SIGINT));
    assert!(env.thr.signal_blocked(Signo::SIGINT));

    let empty = SignalSet::default();
    env.thr.set_blocked(empty);
    assert!(!env.thr.signal_blocked(Signo::SIGINT));
}

#[test]
fn handle_signal() {
    unsafe extern "C" fn test_handler(_: i32) {}
    let env = TestEnv::new();
    let actions = env.actions.clone();
    actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Handler(test_handler);
    let sig = SignalInfo::new_user(Signo::SIGTERM, 9, 9);

    let mut uctx: UserContext = unsafe { zeroed() };
    let mut stack = vec![0u8; 16 * 1024].into_boxed_slice();
    let initial_sp = stack.as_mut_ptr() as usize + stack.len();
    uctx.set_sp(initial_sp);

    let restore_blocked = env.thr.blocked();
    let action = env.actions.lock()[sig.signo()].clone();
    let result = env
        .thr
        .handle_signal(&mut uctx, restore_blocked, &sig, &action);

    assert!(matches!(result, Some(SignalOSAction::Handler)));
    assert_eq!(uctx.ip(), test_handler as *const () as usize);
    assert!(uctx.sp() < initial_sp);
    assert_eq!(uctx.arg0(), Signo::SIGTERM as usize);

    drop(stack);
}

#[test]
fn dequeue_signal() {
    let env = TestEnv::new();
    let sig1 = SignalInfo::new_user(Signo::SIGINT, 9, 9);
    let sig2 = SignalInfo::new_user(Signo::SIGTERM, 9, 9);
    let mask = SignalSet::default();
    let allowed_mask = !mask;
    assert!(env.thr.send_signal(sig1.clone()));
    assert_eq!(env.proc.send_signal(sig2), Some(7));
    assert_eq!(
        env.thr.dequeue_signal(&allowed_mask).unwrap().signo(),
        Signo::SIGINT
    );
    assert_eq!(
        env.thr.dequeue_signal(&allowed_mask).unwrap().signo(),
        Signo::SIGTERM
    );
    assert!(env.thr.dequeue_signal(&allowed_mask).is_none());
}

#[test]
fn check_signals() {
    let env = TestEnv::new();
    let mut uctx: UserContext = unsafe { zeroed() };
    uctx.set_sp(0x8000_0000);

    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 1);
    assert_eq!(env.proc.send_signal(sig.clone()), Some(7));

    let (si, _os_action) = env.thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(si.signo(), Signo::SIGTERM);

    assert!(env.thr.send_signal(sig.clone()));
    let (si, _os_action) = env.thr.check_signals(&mut uctx, None).unwrap();
    assert_eq!(si.signo(), Signo::SIGTERM);
}

#[test]
fn restore() {
    unsafe extern "C" fn test_handler(_: i32) {}
    let env = TestEnv::new();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 1);
    env.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Handler(test_handler);

    let mut stack = vec![0u8; 16 * 1024].into_boxed_slice();
    let sp = stack.as_mut_ptr() as usize + stack.len();
    let mut initial: UserContext = unsafe { zeroed() };
    initial.set_sp(sp);
    initial.set_ip(0x4000_1000usize);
    let mut uctx_user = initial.clone();

    let restore_blocked = env.thr.blocked();
    let action = env.actions.lock()[sig.signo()].clone();
    env.thr
        .handle_signal(&mut uctx_user, restore_blocked, &sig, &action);

    let new_sp = uctx_user.sp() + 8;
    uctx_user.set_sp(new_sp);
    env.thr.restore(&mut uctx_user);

    assert_eq!(uctx_user.ip(), initial.ip());
    assert_eq!(uctx_user.sp(), initial.sp());

    drop(stack);
}
