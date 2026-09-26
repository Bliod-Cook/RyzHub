use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
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
