#![feature(maybe_uninit_write_slice)]

mod common;

use std::{
    mem::zeroed,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use axcpu::uspace::UserContext;
use common::*;
use kspin::SpinNoIrq;
use starry_signal::{
    SignalDisposition, SignalInfo, SignalOSAction, SignalSet, Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};

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
    let initial_sp = {
        let pool = POOL.lock().unwrap();
        pool.as_ptr() as usize + pool.len()
    };
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

    let sp = {
        let pool = POOL.lock().unwrap();
        pool.as_ptr() as usize + pool.len()
    };
    let mut initial: UserContext = unsafe { zeroed() };
    initial.set_sp(sp);
    initial.set_ip(0x219);
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
}

#[test]
fn thread_send_signal() {
    let env = TestEnv::new();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 9, 9);

    let thr = env.thr.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        let _ = thr.send_signal(sig);
    });

    let res = wait_until(Duration::from_millis(100), || {
        env.thr.pending().has(Signo::SIGTERM) || env.proc.pending().has(Signo::SIGTERM)
    });

    assert!(res);
}

#[test]
fn thread_blocked() {
    let env = TestEnv::new();
    let sig = SignalInfo::new_user(Signo::SIGTERM, 9, 9);

    let mut blocked = SignalSet::default();
    blocked.add(Signo::SIGTERM);
    let prev = env.thr.set_blocked(blocked);
    assert!(!prev.has(Signo::SIGTERM));
    assert!(env.thr.signal_blocked(Signo::SIGTERM));

    let thr = env.thr.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        let _ = thr.send_signal(sig);
    });

    let pending_res = wait_until(Duration::from_millis(100), || {
        env.thr.pending().has(Signo::SIGTERM)
    });
    assert!(pending_res);

    env.thr.set_blocked(SignalSet::default());
    assert!(!env.thr.signal_blocked(Signo::SIGTERM));

    let uctx = Arc::new(SpinNoIrq::new(unsafe { zeroed::<UserContext>() }));
    uctx.lock().set_sp(0x8000_0000);
    let res = wait_until(Duration::from_millis(100), || {
        let mut uctx_ref = uctx.lock().clone();
        if let Some((si, _)) = env.thr.check_signals(&mut uctx_ref, None) {
            assert_eq!(si.signo(), Signo::SIGTERM);
            true
        } else {
            false
        }
    });
    assert!(res);
}

#[test]
fn thread_handler() {
    unsafe extern "C" fn test_handler(_: i32) {}

    let env = TestEnv::new();
    env.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Handler(test_handler);

    let uctx = Arc::new(SpinNoIrq::new(unsafe { zeroed::<UserContext>() }));
    let initial_sp = {
        let pool = POOL.lock().unwrap();
        pool.as_ptr() as usize + pool.len()
    };
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
    thread::spawn(move || {
        let _ = thr.send_signal(SignalInfo::new_user(Signo::SIGINT, 2, 2));
        let _ = thr.send_signal(SignalInfo::new_user(Signo::SIGTERM, 3, 3));
    });

    let pending_res = wait_until(Duration::from_millis(200), || {
        env.thr.pending().has(Signo::SIGTERM)
    });
    assert!(pending_res);

    let pending_res = wait_until(Duration::from_millis(200), || {
        env.thr.pending().has(Signo::SIGINT)
    });
    assert!(pending_res);

    let frame_sp = uctx.lock().sp() + 8;
    {
        let mut guard = uctx.lock();
        guard.set_sp(frame_sp);
        env.thr.restore(&mut guard);
    }
    assert!(!env.thr.signal_blocked(Signo::SIGTERM));

    let delivered = Arc::new(AtomicUsize::new(0));
    let delivered_result = wait_until(Duration::from_millis(200), || {
        let thr = env.thr.clone();
        let delivered_ref = delivered.clone();
        let uctx_ref = uctx.clone();
        let mut guard = uctx_ref.lock();
        let mut ctx = guard.clone();
        if let Some((sig, _)) = thr.check_signals(&mut ctx, None) {
            let bit = match sig.signo() {
                Signo::SIGINT => 0b01,
                Signo::SIGTERM => 0b10,
                _ => unreachable!(),
            };
            *guard = ctx;
            delivered_ref.fetch_or(bit, Ordering::SeqCst);
        }
        delivered_ref.load(Ordering::SeqCst) & 0b11 == 0b11
    });

    assert!(delivered_result);
}
