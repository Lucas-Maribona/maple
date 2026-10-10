//! Conservative per-filesystem estimates; allocation failures still roll back.
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

fn existing_parent(mut path: PathBuf) -> Result<PathBuf> {
    loop {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => return Ok(path),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        ensure!(path.pop(), "cannot find filesystem for space estimate");
    }
}

pub fn check(root: &Path, paths: &BTreeSet<String>, incoming: u64) -> Result<()> {
    let mut devices: BTreeMap<u64, (PathBuf, u64)> = BTreeMap::new();
    let backup_path = existing_parent(root.join("var/lib/maple/transactions"))?;
    let backup_device = fs::metadata(&backup_path)?.dev();
    let mut backup = 1024u64 * 1024;
    let mut inodes = BTreeSet::new();
    for name in paths {
        let path = root.join(name);
        crate::database::check_parents(root, name)?;
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() && inodes.insert((m.dev(), m.ino())) => {
                backup = backup.saturating_add(m.len());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        backup = backup.saturating_add(65536); // headers, attributes and allocation overhead
        let parent = existing_parent(path)?;
        devices.entry(fs::metadata(&parent)?.dev()).or_insert((
            parent,
            incoming.saturating_mul(8).saturating_add(1024 * 1024),
        ));
    }
    let entry = devices.entry(backup_device).or_insert((backup_path, 0));
    entry.1 = entry.1.saturating_add(backup.saturating_mul(2));
    for (_, (path, needed)) in devices {
        let cpath = CString::new(path.as_os_str().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: valid C string and writable statvfs storage.
        if unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful statvfs initialized the structure.
        let stat = unsafe { stat.assume_init() };
        let available = stat.f_bavail.saturating_mul(stat.f_frsize);
        ensure!(
            available >= needed,
            "insufficient disk space on {}: need approximately {needed} bytes, have {available}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_transaction_is_rejected_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let paths = ["new/file".into()].into_iter().collect();
        assert!(check(root.path(), &paths, u64::MAX).is_err());
        assert!(!root.path().join("new").exists());
        check(root.path(), &paths, 1).unwrap();
    }
}

/// Keep emergency headroom on the filesystem actually holding a temporary file.
/// This includes earlier packages still staged by the same transaction.
pub const TEMP_HEADROOM: u64 = 8 * 1024 * 1024;

pub fn temporary(file: &fs::File, additional: u64) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: live file descriptor and writable statvfs storage.
    if unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful syscall initialized the structure.
    let stat = unsafe { stat.assume_init() };
    temporary_available(stat.f_bavail.saturating_mul(stat.f_frsize), additional)
}

fn temporary_available(available: u64, additional: u64) -> Result<()> {
    let required = additional.saturating_add(TEMP_HEADROOM);
    ensure!(
        available >= required,
        "insufficient temporary disk space: need {additional} bytes plus {TEMP_HEADROOM} bytes headroom, have {available}; choose a larger TMPDIR"
    );
    Ok(())
}

/// Unknown HTTP lengths and decompression ratios require checks before every
/// write. Do not buffer outside this writer: buffered bytes are not in statvfs.
pub struct TemporaryWriter<'a>(pub &'a mut fs::File);
impl std::io::Write for TemporaryWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        temporary(self.0, bytes.len() as u64).map_err(std::io::Error::other)?;
        std::io::Write::write(self.0, bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(self.0)
    }
}

#[cfg(test)]
mod temporary_tests {
    use super::*;
    #[test]
    fn known_sizes_and_streaming_chunks_leave_headroom() {
        assert!(temporary_available(TEMP_HEADROOM + 1024, 1024).is_ok());
        assert!(temporary_available(TEMP_HEADROOM + 1023, 1024).is_err());
        assert!(temporary_available(TEMP_HEADROOM - 1, 0).is_err());
        let file = tempfile::tempfile().unwrap();
        assert!(temporary(&file, u64::MAX).is_err());
        assert_eq!(file.metadata().unwrap().len(), 0);
    }
}
