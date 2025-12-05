use kspin::SpinNoIrq;
use starry_signal::{
    SignalActionFlags, SignalDisposition, SignalInfo, Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};
use std::sync::Arc;

struct TestEnv {
    pub actions: Arc<SpinNoIrq<SignalActions>>,
    pub proc: Arc<ProcessSignalManager>,
}

impl TestEnv {
    pub fn new() -> Self {
        let actions = Arc::new(SpinNoIrq::new(SignalActions::default()));
        let proc = Arc::new(ProcessSignalManager::new(actions.clone(), 0));
        TestEnv { actions, proc }
    }
}

#[test]
fn send_wakes_sets_pending() {
    let env = TestEnv::new();
    let _thr = ThreadSignalManager::new(9, env.proc.clone());
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100);

    assert_eq!(env.proc.send_signal(sig.clone()), Some(9));
    assert!(env.proc.pending().has(Signo::SIGTERM));
}

#[test]
fn signal_ignore() {
    let env = TestEnv::new();
    env.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Ignore;
    let sig = SignalInfo::new_user(Signo::SIGTERM, 0, 100);

    assert_eq!(env.proc.send_signal(sig), None);
    assert!(!env.proc.pending().has(Signo::SIGTERM));
}

#[test]
fn can_restart() {
    let env = TestEnv::new();
    assert!(!env.proc.can_restart(Signo::SIGTERM));

    env.actions.lock()[Signo::SIGTERM]
        .flags
        .insert(SignalActionFlags::RESTART);
    assert!(env.proc.can_restart(Signo::SIGTERM));
}
