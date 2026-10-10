//! Save the backup before marking a transaction active. Flush the changed files
//! before renaming active.toml to last.toml; recovery depends on that ordering.
use crate::{archive, database::check_parents, output, package::clean_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

#[derive(Debug)]
pub struct Committed;
impl std::fmt::Display for Committed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("package transaction committed, but post-transaction maintenance failed; run maple recover to retry or maple rollback to undo package files")
    }
}
impl std::error::Error for Committed {}

#[derive(Deserialize, Serialize)]
struct Journal {
    #[serde(default)]
    triggers: BTreeSet<crate::triggers::Trigger>,
    #[serde(default)]
    previous: Vec<String>,
    snapshot: String,
    rollback: bool,
}

#[derive(Deserialize, Serialize)]
struct Snapshot {
    paths: Vec<String>,
    existing: Vec<String>,
}

fn transaction_dir(root: &Path) -> PathBuf {
    root.join("var/lib/maple/transactions")
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn write_toml(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("journal path has no parent")?;
    let mut file = NamedTempFile::new_in(parent)?;
    file.write_all(toml::to_string(value)?.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    sync_directory(parent)
}

fn read_journal(path: &Path) -> Result<Journal> {
    let journal: Journal = toml::from_str(&fs::read_to_string(path)?)?;
    ensure!(
        std::iter::once(&journal.snapshot)
            .chain(&journal.previous)
            .all(|s| s.starts_with("snapshot-") && !s.contains('/')),
        "invalid transaction snapshot"
    );
    Ok(journal)
}

fn begin(root: &Path, paths: BTreeSet<String>) -> Result<Journal> {
    let directory = transaction_dir(root);
    fs::create_dir_all(&directory)?;
    sync_directory(directory.parent().unwrap())?;
    ensure!(
        !directory.join("active.toml").exists(),
        "an interrupted transaction needs recovery"
    );
    let backup = tempfile::Builder::new()
        .prefix("snapshot-")
        .tempdir_in(&directory)?;
    let paths: Vec<String> = paths.into_iter().collect();
    let mut existing = Vec::new();
    for name in &paths {
        check_parents(root, name)?;
        match fs::symlink_metadata(root.join(name)) {
            Ok(_) => existing.push(name.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let file = File::create(backup.path().join("before.tar.xz"))?;
    let encoder = xz2::write::XzEncoder::new(file, 1);
    archive::write_paths(encoder, root, &existing)?
        .finish()?
        .sync_all()?;
    write_toml(
        &backup.path().join("manifest.toml"),
        &Snapshot { paths, existing },
    )?;
    let backup = backup.keep();
    sync_directory(&directory)?;
    let journal = Journal {
        triggers: BTreeSet::new(),
        previous: Vec::new(),
        snapshot: backup.file_name().unwrap().to_str().unwrap().to_owned(),
        rollback: false,
    };
    write_toml(&directory.join("active.toml"), &journal)?;
    // A fresh root needs its new database directories flushed too.
    sync_paths(
        root,
        &[
            "var/lib/maple/transactions".to_owned(),
            "var/lib/maple/installed".to_owned(),
        ],
    )?;
    Ok(journal)
}

/// Persist file data and all affected directory entries, including mount points.
fn sync_paths(root: &Path, paths: &[String]) -> Result<()> {
    let mut directories = BTreeSet::new();
    directories.insert(root.to_owned());
    for name in paths {
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => File::open(&path)?.sync_all()?,
            Ok(metadata) if metadata.is_dir() => {
                directories.insert(path.clone());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut parent = path.parent();
        while let Some(path) = parent.filter(|p| p.starts_with(root)) {
            if path.exists() {
                directories.insert(path.to_owned());
            }
            parent = path.parent();
        }
    }
    for path in directories.iter().rev() {
        sync_directory(path)?;
    }
    Ok(())
}

fn restore(root: &Path, journal: &Journal) -> Result<()> {
    let directory = transaction_dir(root);
    let backup = directory.join(&journal.snapshot);
    let snapshot: Snapshot = toml::from_str(&fs::read_to_string(backup.join("manifest.toml"))?)?;
    // We may be undoing a package with read-only directories. Make room to
    // remove their children; extracting the backup restores the old modes.
    let mut new_directories = Vec::new();
    for name in &snapshot.paths {
        ensure!(
            !name.is_empty() && clean_path(Path::new(name))? == *name,
            "invalid snapshot path"
        );
        check_parents(root, name)?;
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                let permissions = metadata.permissions();
                fs::set_permissions(
                    &path,
                    fs::Permissions::from_mode(permissions.mode() | 0o700),
                )?;
                if !snapshot.existing.contains(name) {
                    new_directories.push((path, permissions));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // Keep the backup intact in case recovery itself gets interrupted.
    for name in snapshot.paths.iter().rev() {
        ensure!(
            !name.is_empty() && clean_path(Path::new(name))? == *name,
            "invalid snapshot path"
        );
        check_parents(root, name)?;
        let path = root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                if !snapshot.existing.contains(name) {
                    match fs::remove_dir(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            Ok(_) => archive::unlink(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if backup.join("before.tar.xz").exists() {
        archive::extract(
            xz2::read::XzDecoder::new(File::open(backup.join("before.tar.xz"))?),
            root,
            false,
        )?;
    } else {
        archive::extract(File::open(backup.join("before.tar"))?, root, false)?;
    }
    // Keep directories with untracked files and put their permissions back.
    for (path, permissions) in new_directories.into_iter().rev() {
        if path.is_dir() {
            fs::set_permissions(path, permissions)?;
        }
    }
    sync_paths(root, &snapshot.paths)?;
    if journal.rollback {
        if let Some(previous) = journal.previous.first() {
            let mut previous = read_journal(&directory.join(previous).join("journal.toml"))?;
            previous.previous = journal.previous.iter().skip(1).cloned().collect();
            write_toml(&directory.join("last.toml"), &previous)?;
        } else {
            archive::unlink(&directory.join("last.toml"))?;
        }
        sync_directory(&directory)?;
    }
    queue_triggers(root, &journal.triggers)?;
    archive::unlink(&directory.join("active.toml"))?;
    sync_directory(&directory)?;
    Ok(())
}

pub fn pending(root: &Path) -> bool {
    transaction_dir(root).join("active.toml").exists()
}

pub fn recover(root: &Path) -> Result<bool> {
    let recovered = recover_files(root)?;
    finish_triggers(root).context("maintenance remains pending; run maple recover to retry")?;
    Ok(recovered)
}

pub fn recover_files(root: &Path) -> Result<bool> {
    if !pending(root) {
        enqueue_committed(root)?;
        return Ok(false);
    }
    output::step("Recovering interrupted transaction...");
    let journal = read_journal(&transaction_dir(root).join("active.toml"))?;
    restore(root, &journal)
        .context("recovery failed; backup and journal are retained for retry")?;
    enqueue_committed(root)?;
    Ok(true)
}

pub fn can_rollback(root: &Path) -> bool {
    transaction_dir(root).join("last.toml").exists()
}

pub fn rollback(root: &Path) -> Result<()> {
    let directory = transaction_dir(root);
    ensure!(can_rollback(root), "no completed transaction to roll back");
    let mut journal = read_journal(&directory.join("last.toml"))?;
    journal.rollback = true;
    write_toml(&directory.join("active.toml"), &journal)?;
    restore(root, &journal).context("rollback interrupted; run maple recover to retry")?;
    finish_triggers(root).context(
        "package rollback completed, but maintenance remains pending; run maple recover to retry",
    )
}

#[cfg(test)]
pub fn run(root: &Path, paths: BTreeSet<String>, apply: impl FnOnce() -> Result<()>) -> Result<()> {
    run_with(root, paths, BTreeSet::new(), 0, apply)
}

pub fn run_with(
    root: &Path,
    paths: BTreeSet<String>,
    triggers: BTreeSet<crate::triggers::Trigger>,
    incoming: u64,
    apply: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let retention = retention(root)?;
    crate::space::check(root, &paths, incoming)?;
    let mut previous = Vec::new();
    let directory = transaction_dir(root);
    if directory.join("last.toml").exists() {
        let old = read_journal(&directory.join("last.toml"))?;
        // Also upgrade a legacy journal to a reusable history entry.
        write_toml(&directory.join(&old.snapshot).join("journal.toml"), &old)?;
        previous.push(old.snapshot);
        previous.extend(old.previous);
    }
    previous.truncate(retention - 1);
    let mut journal = begin(root, paths.clone())?;
    journal.triggers = triggers;
    journal.previous = previous;
    write_toml(
        &directory.join(&journal.snapshot).join("journal.toml"),
        &journal,
    )?;
    write_toml(&directory.join("active.toml"), &journal)?;
    let result = apply().and_then(|()| sync_paths(root, &paths.into_iter().collect::<Vec<_>>()));
    if let Err(error) = result {
        output::warn("Operation failed; restoring previous files and package records...");
        if let Err(recovery) = restore(root, &journal) {
            anyhow::bail!(
                "{error:#}; rollback also failed: {recovery:#}. Run maple recover to retry; the backup is retained"
            );
        }
        return Err(error).context("transaction rolled back");
    }
    let directory = transaction_dir(root);
    // This rename is the commit point. Recovery sees either active (undo) or last (committed).
    if let Err(error) = fs::rename(directory.join("active.toml"), directory.join("last.toml")) {
        restore(root, &journal).context("commit failed and rollback needs recovery")?;
        return Err(error).context("commit failed; transaction rolled back");
    }
    sync_directory(&directory)
        .context("commit could not be flushed to disk")
        .map_err(|e| e.context(Committed))?;
    if let Err(error) = finish_triggers(root) {
        return Err(error.context(Committed));
    }
    if let Err(error) = prune(root) {
        output::warn(format!(
            "transaction committed, but old backup cleanup failed: {error:#}"
        ));
    }
    Ok(())
}

/// Drop old backups, keeping the active transaction and retained rollback history.
/// An unfinished backup with no journal is safe to remove.
pub fn prune(root: &Path) -> Result<()> {
    let directory = transaction_dir(root);
    if !directory.exists() {
        return Ok(());
    }
    let mut keep = BTreeSet::new();
    for name in ["active.toml", "last.toml"] {
        if directory.join(name).exists() {
            let journal = read_journal(&directory.join(name))?;
            keep.insert(journal.snapshot);
            keep.extend(journal.previous);
        }
    }
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("snapshot-") && !keep.contains(&name) && entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    sync_directory(&directory)
}

fn retention(root: &Path) -> Result<usize> {
    let path = root.join("etc/maple/config.toml");
    check_parents(root, "etc/maple/config.toml")?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file(),
            "Maple configuration must be a regular file"
        );
    }
    let count = match fs::read_to_string(path) {
        Ok(text) => toml::from_str::<crate::model::Config>(&text)?.rollback_retention,
        Err(e) if e.kind() == io::ErrorKind::NotFound => 1,
        Err(e) => return Err(e.into()),
    };
    ensure!(
        (1..=100).contains(&count),
        "rollback_retention must be between 1 and 100"
    );
    Ok(count)
}

#[derive(Default, Deserialize, Serialize)]
struct TriggerQueue {
    triggers: BTreeSet<crate::triggers::Trigger>,
}

fn queue_triggers(root: &Path, triggers: &BTreeSet<crate::triggers::Trigger>) -> Result<()> {
    let path = transaction_dir(root).join("triggers.toml");
    let mut queue = match fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<TriggerQueue>(&text)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => TriggerQueue::default(),
        Err(e) => return Err(e.into()),
    };
    queue.triggers.extend(triggers);
    if !queue.triggers.is_empty() {
        write_toml(&path, &queue)?;
    }
    Ok(())
}

fn enqueue_committed(root: &Path) -> Result<()> {
    let directory = transaction_dir(root);
    if directory.join("last.toml").exists() {
        let journal = read_journal(&directory.join("last.toml"))?;
        let marker = directory.join(&journal.snapshot).join("triggers-queued");
        if !marker.exists() {
            queue_triggers(root, &journal.triggers)?;
            File::create(&marker)?.sync_all()?;
            sync_directory(marker.parent().unwrap())?;
        }
    }
    Ok(())
}

fn finish_triggers(root: &Path) -> Result<()> {
    finish_triggers_with(root, crate::triggers::execute)
}

fn finish_triggers_with(
    root: &Path,
    mut execute: impl FnMut(&Path, &BTreeSet<crate::triggers::Trigger>) -> Result<bool>,
) -> Result<()> {
    enqueue_committed(root)?;
    let directory = transaction_dir(root);
    let path = directory.join("triggers.toml");
    let queue = match fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<TriggerQueue>(&text)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut remaining = queue.triggers.clone();
    for trigger in queue.triggers {
        let batch = [trigger].into_iter().collect();
        ensure!(
            execute(root, &batch)?,
            "{trigger:?} maintenance is incomplete; run maple recover to retry"
        );
        remaining.remove(&trigger);
        write_toml(
            &path,
            &TriggerQueue {
                triggers: remaining.clone(),
            },
        )?;
    }
    archive::unlink(&path)?;
    sync_directory(&directory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn failure_and_repeated_recovery_restore_files_records_and_links() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("var/lib/maple/installed")).unwrap();
        fs::write(root.path().join("file"), "old").unwrap();
        fs::hard_link(root.path().join("file"), root.path().join("hard")).unwrap();
        symlink("file", root.path().join("link")).unwrap();
        fs::write(
            root.path().join("var/lib/maple/installed/app.toml"),
            "old-record",
        )
        .unwrap();
        let paths: BTreeSet<_> = [
            "file",
            "hard",
            "link",
            "new",
            "var/lib/maple/installed/app.toml",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let journal = begin(root.path(), paths.clone()).unwrap();
        archive::unlink(&root.path().join("file")).unwrap();
        fs::write(root.path().join("file"), "partial").unwrap();
        fs::write(root.path().join("new"), "new").unwrap();
        fs::write(
            root.path().join("var/lib/maple/installed/app.toml"),
            "partial-record",
        )
        .unwrap();
        // Recover from disk, as a fresh process would.
        assert!(recover(root.path()).unwrap());
        assert_eq!(fs::read_to_string(root.path().join("file")).unwrap(), "old");
        assert_eq!(
            fs::metadata(root.path().join("file")).unwrap().ino(),
            fs::metadata(root.path().join("hard")).unwrap().ino()
        );
        assert_eq!(
            fs::read_link(root.path().join("link")).unwrap(),
            Path::new("file")
        );
        assert!(!root.path().join("new").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("var/lib/maple/installed/app.toml")).unwrap(),
            "old-record"
        );
        // Simulate restarting while a recovery was itself in progress.
        write_toml(&transaction_dir(root.path()).join("active.toml"), &journal).unwrap();
        assert!(recover(root.path()).unwrap());
        assert!(!recover(root.path()).unwrap());
        let error = run(root.path(), paths, || {
            fs::write(root.path().join("file"), "bad")?;
            anyhow::bail!("simulated write failure")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("transaction rolled back"));
        assert_eq!(fs::read_to_string(root.path().join("file")).unwrap(), "old");
    }
    #[test]
    fn metadata_survives_failure_recovery_and_manual_rollback() {
        let root = tempfile::tempdir().unwrap();
        drop(crate::database::Database::open(root.path(), true).unwrap());
        let path = root.path().join("file");
        fs::write(&path, "original").unwrap();
        let original = [("user.binary".into(), vec![0, 255, 10])]
            .into_iter()
            .collect();
        crate::attributes::restore(&path, &original, true).unwrap();
        let paths: BTreeSet<String> = ["file".into()].into_iter().collect();
        let journal = begin(root.path(), paths.clone()).unwrap();
        crate::attributes::restore(&path, &Default::default(), true).unwrap();
        recover(root.path()).unwrap();
        assert_eq!(crate::attributes::read(&path).unwrap(), original);
        // Recovery can itself be interrupted and repeated without losing metadata.
        write_toml(&transaction_dir(root.path()).join("active.toml"), &journal).unwrap();
        recover(root.path()).unwrap();
        assert_eq!(crate::attributes::read(&path).unwrap(), original);
        run(root.path(), paths.clone(), || {
            crate::attributes::restore(&path, &Default::default(), true)
        })
        .unwrap();
        rollback(root.path()).unwrap();
        assert_eq!(crate::attributes::read(&path).unwrap(), original);
        assert!(
            run(root.path(), paths, || {
                crate::attributes::restore(&path, &Default::default(), true)?;
                anyhow::bail!("injected failure")
            })
            .is_err()
        );
        assert_eq!(crate::attributes::read(&path).unwrap(), original);
    }

    #[test]
    fn committed_trigger_queue_is_reconstructed_after_crash() {
        let root = tempfile::tempdir().unwrap();
        drop(crate::database::Database::open(root.path(), true).unwrap());
        let mut journal = begin(root.path(), BTreeSet::new()).unwrap();
        journal.triggers.insert(crate::triggers::Trigger::Ldconfig);
        let dir = transaction_dir(root.path());
        write_toml(&dir.join("active.toml"), &journal).unwrap();
        // Crash at the commit point, before any queue or completion marker exists.
        fs::rename(dir.join("active.toml"), dir.join("last.toml")).unwrap();
        assert!(!recover_files(root.path()).unwrap());
        let queue: TriggerQueue =
            toml::from_str(&fs::read_to_string(dir.join("triggers.toml")).unwrap()).unwrap();
        assert_eq!(queue.triggers, journal.triggers);
        recover_files(root.path()).unwrap();
        let queue: TriggerQueue =
            toml::from_str(&fs::read_to_string(dir.join("triggers.toml")).unwrap()).unwrap();
        assert_eq!(queue.triggers.len(), 1);
    }

    #[test]
    fn failed_triggers_keep_queue_and_successful_retry_clears_it() {
        let root = tempfile::tempdir().unwrap();
        drop(crate::database::Database::open(root.path(), true).unwrap());
        fs::create_dir_all(transaction_dir(root.path())).unwrap();
        let triggers = [crate::triggers::Trigger::Systemd].into_iter().collect();
        queue_triggers(root.path(), &triggers).unwrap();
        assert!(
            finish_triggers_with(root.path(), |_, queued| {
                assert_eq!(queued, &triggers);
                anyhow::bail!("simulated command failure")
            })
            .is_err()
        );
        assert!(transaction_dir(root.path()).join("triggers.toml").exists());
        finish_triggers_with(root.path(), |_, queued| {
            assert_eq!(queued, &triggers);
            Ok(true)
        })
        .unwrap();
        assert!(!transaction_dir(root.path()).join("triggers.toml").exists());
    }

    #[test]
    fn legacy_uncompressed_backup_and_journal_remain_recoverable() {
        let root = tempfile::tempdir().unwrap();
        drop(crate::database::Database::open(root.path(), true).unwrap());
        fs::write(root.path().join("file"), "legacy").unwrap();
        let journal = begin(root.path(), ["file".into()].into_iter().collect()).unwrap();
        let dir = transaction_dir(root.path());
        let backup = dir.join(&journal.snapshot);
        let mut input =
            xz2::read::XzDecoder::new(File::open(backup.join("before.tar.xz")).unwrap());
        io::copy(
            &mut input,
            &mut File::create(backup.join("before.tar")).unwrap(),
        )
        .unwrap();
        fs::remove_file(backup.join("before.tar.xz")).unwrap();
        fs::write(
            dir.join("active.toml"),
            format!("snapshot = {:?}\nrollback = false\n", journal.snapshot),
        )
        .unwrap();
        fs::write(root.path().join("file"), "broken").unwrap();
        recover(root.path()).unwrap();
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "legacy"
        );
    }

    #[test]
    fn crash_writer_child() {
        let Some(root) = std::env::var_os("MAPLE_TEST_CRASH_ROOT") else {
            return;
        };
        let root = Path::new(&root);
        begin(
            root,
            ["file".to_owned(), "new".to_owned()].into_iter().collect(),
        )
        .unwrap();
        fs::write(root.join("file"), "interrupted").unwrap();
        fs::write(root.join("new"), "partial").unwrap();
        // A separate test process exits without running Rust destructors.
        std::process::exit(77);
    }

    #[test]
    fn startup_recovers_after_process_exit() {
        let root = tempfile::tempdir().unwrap();
        drop(crate::database::Database::open(root.path(), true).unwrap());
        fs::write(root.path().join("file"), "original").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "transaction::tests::crash_writer_child"])
            .env("MAPLE_TEST_CRASH_ROOT", root.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(77));
        assert!(pending(root.path()));
        assert!(crate::database::Database::open(root.path(), false).is_err());
        drop(crate::database::Database::open(root.path(), true).unwrap());
        assert!(!pending(root.path()));
        assert_eq!(
            fs::read_to_string(root.path().join("file")).unwrap(),
            "original"
        );
        assert!(!root.path().join("new").exists());
    }
}
