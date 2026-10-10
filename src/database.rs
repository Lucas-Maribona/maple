use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use crate::{
    model::{Installed, validate_name},
    package::{Prepared, clean_path},
};

pub struct Database {
    pub root: PathBuf,
    directory: PathBuf,
    pub installed: BTreeMap<String, Installed>,
    // Keep the file open for as long as we hold the database lock.
    _lock: Option<File>,
}

impl Database {
    pub fn open(root: &Path, writable: bool) -> Result<Self> {
        if writable {
            fs::create_dir_all(root)?;
        }
        let root = root
            .canonicalize()
            .context("installation root must exist")?;
        check_parents(&root, "var/lib/maple/installed/record")?;
        check_parents(&root, "var/lib/maple/transactions/journal")?;
        let directory = root.join("var/lib/maple");
        let lock = if writable {
            fs::create_dir_all(directory.join("installed"))?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(directory.join("lock"))?;
            file.try_lock()
                .context("another Maple command is running")?;
            Some(file)
        } else {
            match File::open(directory.join("lock")) {
                Ok(file) => {
                    file.try_lock_shared()
                        .context("another maple command is running")?;
                    Some(file)
                }
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        };
        if writable {
            crate::transaction::recover_files(&root)?;
            crate::transaction::prune(&root)?;
        } else {
            ensure!(
                !crate::transaction::pending(&root),
                "interrupted transaction; run maple recover first"
            );
        }
        let mut installed = BTreeMap::new();
        match fs::read_dir(directory.join("installed")) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    if path.extension().is_some_and(|ext| ext == "toml") {
                        let record: Installed = toml::from_str(&fs::read_to_string(&path)?)
                            .with_context(|| format!("read installed record {}", path.display()))?;
                        record.package.validate()?;
                        ensure!(
                            record
                                .package
                                .hooks
                                .iter()
                                .copied()
                                .collect::<BTreeSet<_>>()
                                == record.hook_scripts.keys().copied().collect(),
                            "stored hook declarations do not match scripts"
                        );
                        for script in record.hook_scripts.values() {
                            crate::hooks::validate_script(script)?;
                        }
                        ensure!(
                            path.file_stem().and_then(|s| s.to_str()) == Some(&record.package.name),
                            "installed record name mismatch"
                        );
                        for name in record
                            .files
                            .iter()
                            .chain(&record.directories)
                            .chain(record.config_defaults.keys())
                        {
                            ensure!(
                                !name.is_empty() && clean_path(Path::new(name))? == *name,
                                "invalid installed path: {name}"
                            );
                        }
                        installed.insert(record.package.name.clone(), record);
                    }
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            root,
            directory,
            installed,
            _lock: lock,
        })
    }

    fn transaction_paths(&self, records: &[Installed]) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        for record in records {
            paths.extend(record.files.iter().chain(&record.directories).cloned());
            paths.extend(
                record
                    .package
                    .config_files
                    .iter()
                    .map(|p| format!("{p}.maple-new")),
            );
            if let Some(old) = self.installed.get(&record.package.name) {
                paths.extend(old.files.iter().chain(&old.directories).cloned());
            }
            paths.insert(format!(
                "var/lib/maple/installed/{}.toml",
                record.package.name
            ));
        }
        for path in paths.clone() {
            let mut parent = Path::new(&path).parent();
            while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
                paths.insert(path.to_str().unwrap().to_owned());
                parent = path.parent();
            }
        }
        paths
    }

    pub fn install_all(&mut self, packages: Vec<Prepared>) -> Result<()> {
        let incoming_size = packages.iter().try_fold(0u64, |total, p| -> Result<u64> {
            Ok(total.saturating_add(p.size()?))
        })?;
        let records: Vec<_> = packages.iter().map(|p| p.record.clone()).collect();
        let paths = self.transaction_paths(&records);
        let previous = self.installed.clone();
        let root = self.root.clone();
        let triggers = self.triggers(&records);
        let result = crate::transaction::run_with(&root, paths, triggers, incoming_size, || {
            for record in &records {
                let old = previous.get(&record.package.name);
                crate::hooks::execute(
                    &root,
                    record,
                    if old.is_some() {
                        crate::hooks::Hook::PreUpgrade
                    } else {
                        crate::hooks::Hook::PreInstall
                    },
                    old,
                )?;
            }
            let total = packages.len();
            for (index, package) in packages.into_iter().enumerate() {
                crate::output::progress(
                    index + 1,
                    total,
                    "Installing",
                    &format!(
                        "{}-{}",
                        package.record.package.name, package.record.package.version
                    ),
                );
                self.install(package)?;
            }
            for record in &records {
                let old = previous.get(&record.package.name);
                crate::hooks::execute(
                    &root,
                    record,
                    if old.is_some() {
                        crate::hooks::Hook::PostUpgrade
                    } else {
                        crate::hooks::Hook::PostInstall
                    },
                    old,
                )?;
            }
            Ok(())
        });
        if result
            .as_ref()
            .is_err_and(|e| !e.is::<crate::transaction::Committed>())
        {
            self.installed = previous;
        }
        result
    }

    pub fn remove_all(&mut self, names: &[String]) -> Result<()> {
        let records: Vec<_> = names
            .iter()
            .map(|name| self.installed[name].clone())
            .collect();
        let paths = self.transaction_paths(&records);
        let previous = self.installed.clone();
        let root = self.root.clone();
        let triggers = self.triggers(&records);
        let result = crate::transaction::run_with(&root, paths, triggers, 0, || {
            for record in &records {
                crate::hooks::execute(&root, record, crate::hooks::Hook::PreRemove, None)?;
            }
            for (index, name) in names.iter().enumerate() {
                crate::output::progress(
                    index + 1,
                    names.len(),
                    "Removing",
                    &format!("{name}-{}", self.installed[name].package.version),
                );
                self.remove(name)?;
            }
            for record in &records {
                crate::hooks::execute(&root, record, crate::hooks::Hook::PostRemove, None)?;
            }
            Ok(())
        });
        if result
            .as_ref()
            .is_err_and(|e| !e.is::<crate::transaction::Committed>())
        {
            self.installed = previous;
        }
        result
    }

    fn triggers(&self, records: &[Installed]) -> BTreeSet<crate::triggers::Trigger> {
        records
            .iter()
            .chain(
                records
                    .iter()
                    .filter_map(|r| self.installed.get(&r.package.name)),
            )
            .flat_map(|r| r.package.triggers.iter().copied())
            .collect()
    }

    pub fn preflight(&self, record: &Installed) -> Result<()> {
        let old = self.installed.get(&record.package.name);
        for path in record.files.iter().chain(&record.directories) {
            check_parents(&self.root, path)?;
            let is_directory = record.directories.contains(path);
            for other in self
                .installed
                .values()
                .filter(|p| p.package.name != record.package.name)
            {
                ensure!(
                    !other.files.contains(path)
                        && (is_directory || !other.directories.contains(path)),
                    "{path} belongs to {}",
                    other.package.name
                );
            }
            match fs::symlink_metadata(self.root.join(path)) {
                Ok(metadata) => {
                    if is_directory {
                        ensure!(
                            metadata.is_dir(),
                            "{path} already exists and is not a directory"
                        );
                    } else {
                        ensure!(
                            !metadata.is_dir(),
                            "{path} is a directory; file type changes require removal first"
                        );
                        ensure!(
                            old.is_some_and(|p| p.files.contains(path)),
                            "refusing to overwrite unowned file: {path}"
                        );
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(old) = old {
            for name in old
                .config_defaults
                .keys()
                .filter(|p| !p.ends_with(".maple-new"))
            {
                ensure!(
                    !record.files.contains(name) || record.package.config_files.contains(name),
                    "cannot stop protecting an existing config file: {name}"
                );
            }
            self.check_removal(old)?;
        }
        Ok(())
    }

    fn install(&mut self, prepared: Prepared) -> Result<()> {
        let mut record = prepared.record.clone();
        self.preflight(&record)?;
        let mut redirects = BTreeMap::new();
        let old = self.installed.get(&record.package.name);
        for name in &record.package.config_files {
            let path = self.root.join(name);
            let protected = match old {
                Some(old) if old.files.contains(name) => !unchanged(&self.root, old, name)?,
                _ => fs::symlink_metadata(&path).is_ok(),
            };
            if protected {
                let sidecar = format!("{name}.maple-new");
                check_parents(&self.root, &sidecar)?;
                ensure!(
                    !self
                        .installed
                        .values()
                        .any(|r| r.package.name != record.package.name
                            && (r.files.contains(&sidecar) || r.directories.contains(&sidecar))),
                    "config sidecar belongs to another package: {sidecar}"
                );
                match fs::symlink_metadata(self.root.join(&sidecar)) {
                    Ok(_) => ensure!(
                        old.is_some_and(|r| r.files.contains(&sidecar))
                            && unchanged(&self.root, old.unwrap(), &sidecar)?,
                        "refusing to overwrite modified or unowned config sidecar: {sidecar}"
                    ),
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                record.files.push(sidecar.clone());
                record
                    .config_defaults
                    .insert(sidecar.clone(), record.config_defaults[name].clone());
                redirects.insert(name.clone(), sidecar);
            }
        }
        prepared.extract(&self.root, &redirects)?;
        if let Some(old) = self.installed.get(&record.package.name) {
            self.remove_paths(old, Some(&record))?;
        }
        let path = self.record_path(&record.package.name)?;
        let mut file = NamedTempFile::new_in(self.directory.join("installed"))?;
        file.write_all(toml::to_string_pretty(&record)?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(path)
            .context("save installed package record")?;
        self.installed.insert(record.package.name.clone(), record);
        Ok(())
    }

    fn record_path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        Ok(self
            .directory
            .join("installed")
            .join(format!("{name}.toml")))
    }

    pub fn check_removal(&self, record: &Installed) -> Result<()> {
        for path in record.files.iter().chain(&record.directories) {
            check_parents(&self.root, path)?;
        }
        for path in &record.files {
            if let Ok(metadata) = fs::symlink_metadata(self.root.join(path)) {
                ensure!(
                    !metadata.is_dir(),
                    "tracked file became a directory: {path}"
                );
            }
        }
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<()> {
        let record = self
            .installed
            .get(name)
            .context("package is not installed")?;
        self.check_removal(record)?;
        self.remove_paths(record, None)?;
        fs::remove_file(self.record_path(name)?)?;
        self.installed.remove(name);
        Ok(())
    }

    fn remove_paths(&self, old: &Installed, replacement: Option<&Installed>) -> Result<()> {
        let mut keep: HashSet<&str> = HashSet::new();
        for record in self
            .installed
            .values()
            .filter(|r| r.package.name != old.package.name)
            .chain(replacement)
        {
            keep.extend(
                record
                    .files
                    .iter()
                    .chain(&record.directories)
                    .map(String::as_str),
            );
        }
        for path in old.files.iter().filter(|p| !keep.contains(p.as_str())) {
            if old.config_defaults.contains_key(path) && !unchanged(&self.root, old, path)? {
                continue;
            }
            match fs::remove_file(self.root.join(path)) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error).with_context(|| format!("remove {path}")),
            }
        }
        // Work backwards so children go first. Nonempty directories stay put.
        let mut directories = old.directories.clone();
        directories.sort();
        for path in directories
            .iter()
            .rev()
            .filter(|p| !keep.contains(p.as_str()))
        {
            match fs::remove_dir(self.root.join(path)) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove directory {path}"));
                }
            }
        }
        Ok(())
    }
}

/// Don't follow directory symlinks out of an alternate --root.
/// Installing a symlink is fine; writing through one is not.
pub fn check_parents(root: &Path, name: &str) -> Result<()> {
    let mut current = root.to_owned();
    if let Some(parent) = Path::new(name).parent() {
        for component in parent.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) => ensure!(
                    metadata.is_dir(),
                    "parent is not a real directory: {}",
                    current.display()
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

/// Missing files and symlinks count as administrator changes. Never follow them.
fn unchanged(root: &Path, record: &Installed, name: &str) -> Result<bool> {
    let Some(default) = record.config_defaults.get(name) else {
        return Ok(false);
    };
    match fs::symlink_metadata(root.join(name)) {
        Ok(metadata) if metadata.is_file() => Ok(fs::read(root.join(name))? == *default),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
