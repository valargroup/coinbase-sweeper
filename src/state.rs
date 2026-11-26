//! Small files under `general.state_dir`: the per-command lock, the halt marker, the pause
//! marker and each command's JSON state. Writes are atomic (temporary file, fsync, rename, fsync
//! of the directory), so a crash leaves either the old or the new contents.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};

use crate::config::Command;

pub struct StateDir {
    dir: PathBuf,
    command: Command,
}

/// Holds the exclusive lock for one command until dropped.
pub struct Lock {
    _file: File,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl StateDir {
    pub fn new(dir: &Path, command: Command) -> Result<StateDir> {
        fs::create_dir_all(dir)
            .with_context(|| format!("cannot create state_dir {}", dir.display()))?;
        Ok(StateDir {
            dir: dir.to_owned(),
            command,
        })
    }

    pub fn command(&self) -> Command {
        self.command
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.dir.join(format!("{}.{suffix}", self.command.name()))
    }

    /// Takes the command's exclusive lock, or fails if another instance holds it. The OS
    /// releases the lock when the process exits, however it exits.
    pub fn lock(&self) -> Result<Lock> {
        let path = self.path("lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Lock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "another `{}` instance is already running (lock {} is held)",
                self.command.name(),
                path.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("cannot lock {}", path.display()))
            }
        }
    }

    pub fn halt_path(&self) -> PathBuf {
        self.path("halt")
    }

    /// The reason the command was halted, if an earlier run halted it.
    pub fn halted(&self) -> Result<Option<String>> {
        match fs::read_to_string(self.halt_path()) {
            Ok(s) => Ok(Some(s.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("cannot read the halt marker"),
        }
    }

    /// Stops the command from sending anything until an operator removes the marker.
    pub fn halt(&self, reason: &str) -> Result<()> {
        write_atomic(&self.halt_path(), format!("{reason}\n").as_bytes())
    }

    /// Unix time until which the command is paused after a node rejection, if any.
    pub fn paused_until(&self) -> Result<Option<u64>> {
        Ok(self
            .read_json::<u64>("paused_until")?
            .filter(|t| *t > now_unix()))
    }

    pub fn pause_until(&self, until: u64) -> Result<()> {
        self.write_json("paused_until", &until)
    }

    pub fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.path(name);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("state file {} is corrupt", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).context("serialize state")?;
        write_atomic(&self.path(name), &bytes)
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f =
            File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateDir::new(dir.path(), Command::Sweep).unwrap();
        let first = s.lock().unwrap();
        let e = s.lock().err().unwrap();
        assert!(e.to_string().contains("already running"), "{e}");
        // Locks are per command.
        StateDir::new(dir.path(), Command::Refill)
            .unwrap()
            .lock()
            .unwrap();
        drop(first);
        s.lock().unwrap();
    }

    #[test]
    fn halt_and_pause_markers() {
        let dir = tempfile::tempdir().unwrap();
        let s = StateDir::new(dir.path(), Command::Refill).unwrap();
        assert_eq!(s.halted().unwrap(), None);
        s.halt("bad tx").unwrap();
        assert_eq!(s.halted().unwrap().as_deref(), Some("bad tx"));
        s.pause_until(now_unix() + 100).unwrap();
        assert!(s.paused_until().unwrap().is_some());
        s.pause_until(1).unwrap();
        assert_eq!(s.paused_until().unwrap(), None);
    }
}
