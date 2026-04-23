use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard, OnceLock},
};

static TEST_CWD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) struct TempCwdGuard {
    _lock: MutexGuard<'static, ()>,
    previous: PathBuf,
    root: PathBuf,
}

impl Drop for TempCwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn temp_cwd(prefix: &str) -> TempCwdGuard {
    let lock = TEST_CWD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("test cwd lock should not be poisoned");
    let previous = std::env::current_dir().expect("current dir should be available");
    let root = std::env::temp_dir().join(format!(
        "claw-party-test-{}-{}-{}",
        prefix,
        std::process::id(),
        rand::random::<u64>()
    ));
    fs::create_dir_all(&root).expect("test cwd should be created");
    std::env::set_current_dir(&root).expect("test cwd should be active");

    TempCwdGuard {
        _lock: lock,
        previous,
        root,
    }
}
