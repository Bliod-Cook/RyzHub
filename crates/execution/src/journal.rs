//! Durable local state for the host and guest agents.
//!
//! Small documents are replaced atomically ([`save`]). Frequently changing state uses a
//! [`Log`]: a snapshot plus an append-only file of numbered entries. Appending costs one write
//! and one `fdatasync`; the snapshot is rewritten only when the log grows long.
use anyhow::Context;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Write, fsync, rename, then fsync the directory: acknowledged state survives power loss.
pub fn save(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    atomic_bytes(path, &serde_json::to_vec(value)?)
}

pub fn atomic_bytes(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension("new");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// State rebuilt by applying log entries in order.
pub trait Replay: Default + Serialize + DeserializeOwned {
    type Entry: Serialize + DeserializeOwned;
    fn apply(&mut self, entry: &Self::Entry);
}

#[derive(Serialize, Deserialize)]
struct Snapshot<S> {
    /// Entries with a number at or below this are already part of `state`.
    applied: u64,
    state: S,
}

#[derive(Serialize, Deserialize)]
struct Record<E> {
    n: u64,
    entry: E,
}

/// Rewrite the snapshot after this many appended entries.
const COMPACT_AFTER: u64 = 1024;

pub struct Log<S: Replay> {
    snapshot: PathBuf,
    log: PathBuf,
    file: File,
    next: u64,
    since_snapshot: u64,
    _state: std::marker::PhantomData<S>,
}

impl<S: Replay> Log<S> {
    /// Load `<name>.json` and replay `<name>.log`. A torn final line (crash mid-append) is
    /// discarded; it was never acknowledged. The result is compacted immediately.
    pub fn open(directory: &Path, name: &str) -> anyhow::Result<(Self, S)> {
        let (state, applied) = Self::load_state(directory, name)?;
        let snapshot = directory.join(format!("{name}.json"));
        let log = directory.join(format!("{name}.log"));
        save(
            &snapshot,
            &Snapshot {
                applied,
                state: &state,
            },
        )?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log)?;
        file.sync_all()?;
        Ok((
            Self {
                snapshot,
                log,
                file,
                next: applied + 1,
                since_snapshot: 0,
                _state: std::marker::PhantomData,
            },
            state,
        ))
    }

    /// The state as of the last complete entry, without modifying any file. Safe to call on a
    /// journal another process is appending to (a torn final line is ignored).
    pub fn read(directory: &Path, name: &str) -> anyhow::Result<S> {
        Ok(Self::load_state(directory, name)?.0)
    }

    /// The snapshot with the log replayed over it, and the number of the last entry applied.
    fn load_state(directory: &Path, name: &str) -> anyhow::Result<(S, u64)> {
        let Snapshot {
            mut applied,
            mut state,
        } = match std::fs::read(directory.join(format!("{name}.json"))) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("unreadable journal snapshot")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot {
                applied: 0,
                state: S::default(),
            },
            Err(e) => return Err(e.into()),
        };
        if let Ok(file) = File::open(directory.join(format!("{name}.log"))) {
            for line in BufReader::new(file).lines() {
                let Ok(record) = serde_json::from_str::<Record<S::Entry>>(&line?) else {
                    break;
                };
                if record.n > applied {
                    state.apply(&record.entry);
                    applied = record.n;
                }
            }
        }
        Ok((state, applied))
    }

    /// Durably record `entry`. Apply it to the in-memory state after this returns.
    pub fn append(&mut self, entry: &S::Entry) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(&Record {
            n: self.next,
            entry,
        })?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.sync_data()?;
        self.next += 1;
        self.since_snapshot += 1;
        Ok(())
    }

    /// Fold the log into a new snapshot once it is long enough. `state` must reflect every
    /// appended entry.
    pub fn compact(&mut self, state: &S) -> anyhow::Result<()> {
        if self.since_snapshot < COMPACT_AFTER {
            return Ok(());
        }
        save(
            &self.snapshot,
            &Snapshot {
                applied: self.next - 1,
                state,
            },
        )?;
        self.file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.log)?;
        self.file.sync_all()?;
        self.since_snapshot = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Serialize, Deserialize)]
    struct Counter(Vec<u32>);
    impl Replay for Counter {
        type Entry = u32;
        fn apply(&mut self, entry: &u32) {
            self.0.push(*entry);
        }
    }

    #[test]
    fn replay_skips_torn_lines_and_entries_already_in_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let (mut log, mut state) = Log::<Counter>::open(dir.path(), "j").unwrap();
        for n in 1..=3 {
            log.append(&n).unwrap();
            state.apply(&n);
        }
        // Simulate a crash after the snapshot was replaced but before the log was truncated.
        save(
            &dir.path().join("j.json"),
            &Snapshot {
                applied: 3,
                state: &state,
            },
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("j.log"))
            .unwrap()
            .write_all(b"{\"n\":4,\"entry\":4}\n{\"n\":5,\"ent")
            .unwrap();
        drop(log);
        let (_, state) = Log::<Counter>::open(dir.path(), "j").unwrap();
        assert_eq!(state.0, [1, 2, 3, 4]);
    }
}
