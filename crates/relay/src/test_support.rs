//! Shared helpers for unit tests in this crate. The main job is the
//! `EnvGuard`/`TempHome` pair: lots of modules read `$HOME` and `$XDG_*`
//! through `directories`, so concurrent tests would otherwise stomp on each
//! other's environment. A single process-wide mutex serializes them.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct TempHome {
    path: PathBuf,
    _guard: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl TempHome {
    pub fn new() -> Self {
        let guard = lock_env();
        let mut path = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        path.push(format!(
            "alleycat-test-{}-{}-{}",
            std::process::id(),
            stamp,
            rand_suffix()
        ));
        std::fs::create_dir_all(&path).expect("temp home");

        let keys: &[(&'static str, &str)] = &[
            ("HOME", path.to_str().unwrap()),
            ("XDG_CONFIG_HOME", ""),
            ("XDG_STATE_HOME", ""),
            ("XDG_DATA_HOME", ""),
            ("XDG_CACHE_HOME", ""),
        ];
        let mut saved = Vec::with_capacity(keys.len());
        for (k, v) in keys {
            saved.push((*k, std::env::var_os(k)));
            unsafe { std::env::set_var(k, v) };
        }
        Self {
            path,
            _guard: guard,
            saved,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Override one or more env vars for the duration of this guard. The
    /// previous values are restored on drop alongside HOME/XDG_*.
    pub fn override_env(&mut self, keys: &[(&'static str, &str)]) {
        for (k, v) in keys {
            self.saved.push((*k, std::env::var_os(k)));
            unsafe { std::env::set_var(k, v) };
        }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn rand_suffix() -> u32 {
    use rand::RngCore;
    rand::rngs::OsRng.next_u32()
}
