use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::{self, Read},
    path::{Component, Path},
};

use anyhow::{Context, Result, bail, ensure};
use tempfile::NamedTempFile;
use xz2::read::XzDecoder;

use crate::model::{Installed, Package};

/// A checked package, with its tar file kept around for installation.
/// Decompress once rather than doing it again when we extract.
pub struct Prepared {
    pub record: Installed,
    archive: NamedTempFile,
}

/// Normalize archive paths and reject anything that could escape the install root.
pub fn clean_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::Normal(value) => {
                parts.push(value.to_str().context("package paths must be UTF-8")?)
            }
            _ => bail!(
                "package path must be relative and contain no '..': {}",
                path.display()
            ),
        }
    }
    Ok(parts.join("/"))
}

impl Prepared {
    pub fn open(path: &Path) -> Result<Self> {
        let mut archive = NamedTempFile::new()?;
        let input = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // A preliminary estimate, followed by checks before each expanded write.
        crate::space::temporary(archive.as_file(), input.metadata()?.len().saturating_mul(4))
            .context("estimate temporary decompression space")?;
        let mut decoder = XzDecoder::new(input);
        io::copy(
            &mut decoder,
            &mut crate::space::TemporaryWriter(archive.as_file_mut()),
        )
        .context("read tar.xz package")?;
        let mut reader = tar::Archive::new(crate::pax::Reader::new(File::open(archive.path())?));
        let mut metadata = None;
        let mut hook_scripts = BTreeMap::new();
        let mut files = BTreeSet::new();
        let mut directories = BTreeSet::new();
        let mut seen = HashSet::new();
        let mut hard_links = Vec::new();
        let mut regular_files = HashSet::new();
        let mut contents = BTreeMap::new();
        let mut hard_targets = HashSet::new();

        for entry in reader.entries()? {
            let mut entry = entry?;
            let path = clean_path(&entry.path()?)?;
            crate::attributes::from_entry(&mut entry)?;
            let kind = entry.header().entry_type();
            if path.is_empty() || path == "payload" {
                ensure!(kind.is_dir(), "{path} must be a directory");
                continue;
            }
            ensure!(
                seen.insert(path.clone()),
                "duplicate archive member: {path}"
            );
            if path == "metadata.toml" {
                ensure!(
                    kind.is_file() && entry.size() <= 16 * 1024 * 1024,
                    "metadata.toml must be a regular file of at most 16 MiB"
                );
                let mut text = String::new();
                entry.read_to_string(&mut text)?;
                let package: Package = toml::from_str(&text).context("invalid package metadata")?;
                package.validate()?;
                metadata = Some(package);
                continue;
            }
            if path == "hooks" {
                ensure!(kind.is_dir(), "hooks must be a directory");
                continue;
            }
            if let Some(name) = path.strip_prefix("hooks/") {
                ensure!(
                    kind.is_file() && entry.size() <= crate::hooks::MAX_SCRIPT,
                    "hook must be a regular file of at most 64 KiB"
                );
                let hook: crate::hooks::Hook =
                    toml::from_str::<HookName>(&format!("hook = {name:?}"))?.hook;
                let mut script = String::new();
                entry.read_to_string(&mut script)?;
                crate::hooks::validate_script(&script)?;
                hook_scripts.insert(hook, script);
                continue;
            }
            let relative = path
                .strip_prefix("payload/")
                .context("archive must contain only metadata.toml, hooks/ and payload/")?;
            ensure!(
                relative != "var/lib/maple" && !relative.starts_with("var/lib/maple/"),
                "packages cannot overwrite maple's database"
            );
            ensure!(
                kind.is_file()
                    || kind.is_dir()
                    || kind.is_symlink()
                    || kind.is_hard_link()
                    || kind.is_fifo()
                    || kind.is_character_special()
                    || kind.is_block_special(),
                "unsupported archive entry: {path}"
            );
            // Validate numeric metadata before any transaction, even when the
            // current user cannot restore ownership or create devices.
            u32::try_from(entry.header().uid()?).context("UID exceeds Linux range")?;
            u32::try_from(entry.header().gid()?).context("GID exceeds Linux range")?;
            i64::try_from(entry.header().mtime()?).context("mtime exceeds supported range")?;
            entry.header().mode()?;
            if kind.is_character_special() || kind.is_block_special() {
                entry
                    .header()
                    .device_major()?
                    .context("missing device major")?;
                entry
                    .header()
                    .device_minor()?
                    .context("missing device minor")?;
            }
            if kind.is_dir() {
                directories.insert(relative.to_owned());
            } else {
                files.insert(relative.to_owned());
            }
            if kind.is_file() {
                regular_files.insert(relative.to_owned());
                // Read config defaults in a second pass once metadata is known.
            }
            if kind.is_hard_link() {
                let target = entry.link_name()?.context("hard link has no target")?;
                let target = clean_path(&target)?;
                let target = target
                    .strip_prefix("payload/")
                    .context("hard links must target a file inside payload/")?;
                hard_targets.insert(target.to_owned());
                hard_links.push(target.to_owned());
            }
            // Some archives omit directory entries. Track those parents as well.
            let mut parent = Path::new(relative).parent();
            while let Some(path) = parent.filter(|p| !p.as_os_str().is_empty()) {
                directories.insert(path.to_str().unwrap().to_owned());
                parent = path.parent();
            }
        }
        for directory in &directories {
            ensure!(
                !files.contains(directory),
                "payload uses {directory} as both a file and a parent directory"
            );
        }
        for target in hard_links {
            ensure!(
                regular_files.contains(&target),
                "hard link target must be a regular payload file: {target}"
            );
        }
        let package = metadata.context("package is missing metadata.toml")?;
        ensure!(
            package.hooks.iter().copied().collect::<BTreeSet<_>>()
                == hook_scripts.keys().copied().collect(),
            "declared hooks must exactly match archive scripts"
        );
        for name in &package.config_files {
            ensure!(
                regular_files.contains(name) && !hard_targets.contains(name),
                "config must be a regular file without hard links: {name}"
            );
            let sidecar = format!("{name}.maple-new");
            ensure!(
                !files.contains(&sidecar) && !directories.contains(&sidecar),
                "config sidecar collides with payload: {sidecar}"
            );
        }
        if !package.config_files.is_empty() {
            let mut reader =
                tar::Archive::new(crate::pax::Reader::new(File::open(archive.path())?));
            for entry in reader.entries()? {
                let mut entry = entry?;
                let path = clean_path(&entry.path()?)?;
                if let Some(name) = path
                    .strip_prefix("payload/")
                    .filter(|name| package.config_files.iter().any(|p| p == name))
                {
                    let mut bytes = Vec::new();
                    entry.read_to_end(&mut bytes)?;
                    contents.insert(name.to_owned(), bytes);
                }
            }
        }
        Ok(Self {
            record: Installed {
                hook_scripts,
                config_defaults: contents,
                package,
                files: files.into_iter().collect(),
                directories: directories.into_iter().collect(),
            },
            archive,
        })
    }

    pub fn size(&self) -> Result<u64> {
        Ok(self.archive.as_file().metadata()?.len())
    }

    pub fn extract(&self, root: &Path, redirects: &BTreeMap<String, String>) -> Result<()> {
        crate::archive::extract_mapped(File::open(self.archive.path())?, root, true, redirects)
    }
}

#[derive(serde::Deserialize)]
struct HookName {
    hook: crate::hooks::Hook,
}
