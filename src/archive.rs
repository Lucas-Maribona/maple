//! Tar handling shared by package installation and rollback.
use crate::{database::check_parents, package::clean_path};
use anyhow::{Context, Result, bail, ensure};
use filetime::FileTime;
use std::{
    collections::HashMap,
    ffi::CString,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    },
    path::Path,
};
use tar::{Builder, EntryType, Header};

/// Archive the listed paths. Don't recurse here: backups must exclude untracked files.
/// Files sharing an inode are stored as hard links.
pub fn write_paths<W: Write>(output: W, root: &Path, paths: &[String]) -> Result<W> {
    let mut builder = Builder::new(output);
    let mut links = HashMap::new();
    for name in paths {
        let path = root.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        let kind = metadata.file_type();
        let attributes = crate::attributes::read(&path)?;
        ensure!(
            attributes.keys().all(|k| !k.contains(['\n', '='])),
            "xattr name cannot be represented by SCHILY PAX: {name}"
        );
        let attributes: Vec<_> = attributes
            .iter()
            .map(|(k, v)| (format!("SCHILY.xattr.{k}"), v))
            .collect();
        builder
            .append_pax_extensions(attributes.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        let mut header = Header::new_gnu();
        header.set_metadata(&metadata);
        if kind.is_file() {
            let key = (metadata.dev(), metadata.ino());
            if let Some(target) = links.get(&key) {
                header.set_entry_type(EntryType::Link);
                header.set_size(0);
                builder.append_link(&mut header, name, target)?;
            } else {
                links.insert(key, name.clone());
                builder.append_data(&mut header, name, File::open(&path)?)?;
            }
        } else if kind.is_symlink() {
            header.set_size(0);
            header.set_entry_type(EntryType::Symlink);
            builder.append_link(&mut header, name, fs::read_link(&path)?)?;
        } else {
            header.set_size(0);
            header.set_entry_type(if kind.is_dir() {
                EntryType::Directory
            } else if kind.is_fifo() {
                EntryType::Fifo
            } else if kind.is_char_device() {
                EntryType::Char
            } else if kind.is_block_device() {
                EntryType::Block
            } else {
                bail!("cannot archive socket or unknown file type: {name}")
            });
            if kind.is_char_device() || kind.is_block_device() {
                header.set_device_major(libc::major(metadata.rdev()))?;
                header.set_device_minor(libc::minor(metadata.rdev()))?;
            }
            builder.append_data(&mut header, name, io::empty())?;
        }
    }
    Ok(builder.into_inner()?)
}

/// Extract a backup, or strip the payload/ prefix when installing a package.
pub fn extract<R: Read>(input: R, root: &Path, payload_only: bool) -> Result<()> {
    extract_mapped(
        input,
        root,
        payload_only,
        &std::collections::BTreeMap::new(),
    )
}

pub fn extract_mapped<R: Read>(
    input: R,
    root: &Path,
    payload_only: bool,
    redirects: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let mut archive = tar::Archive::new(crate::pax::Reader::new(input));
    let mut directories = Vec::new();
    let mut links = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = clean_path(&entry.path()?)?;
        let name = if payload_only {
            match name.strip_prefix("payload/") {
                Some(name) => name.to_owned(),
                None => continue,
            }
        } else {
            name
        };
        let name = redirects.get(&name).cloned().unwrap_or(name);
        ensure!(!name.is_empty(), "empty archive path");
        let attributes = crate::attributes::from_entry(&mut entry)?;
        check_parents(root, &name)?;
        let path = root.join(&name);
        let header = entry.header().clone();
        let kind = header.entry_type();
        fs::create_dir_all(path.parent().context("path has no parent")?)?;
        if kind.is_dir() {
            match fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(metadata.is_dir(), "not a directory: {name}"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
                Err(error) => return Err(error.into()),
            }
            // A read-only directory still needs to accept its children during extraction.
            let mode = fs::metadata(&path)?.permissions().mode();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode | 0o700))?;
            directories.push((path, header, attributes));
            continue;
        }
        unlink(&path)?;
        if kind.is_file() {
            let mut file = File::create(&path)?;
            io::copy(&mut entry, &mut file)?;
            restore_attributes(&path, &header)?;
            crate::attributes::restore(&path, &attributes, !payload_only)?;
            file.sync_all()?;
        } else if kind.is_symlink() {
            symlink(entry.link_name()?.context("missing symlink target")?, &path)?;
            restore_attributes(&path, &header)?;
            crate::attributes::restore(&path, &attributes, !payload_only)?;
        } else if kind.is_hard_link() {
            let target = clean_path(&entry.link_name()?.context("missing hard link target")?)?;
            let target = if payload_only {
                target
                    .strip_prefix("payload/")
                    .context("hard link outside payload")?
                    .to_owned()
            } else {
                target
            };
            links.push((name, target, attributes));
        } else if kind.is_fifo() || kind.is_character_special() || kind.is_block_special() {
            let filename = CString::new(path.as_os_str().as_bytes())?;
            let mode = header.mode()?
                | if kind.is_fifo() {
                    libc::S_IFIFO
                } else if kind.is_character_special() {
                    libc::S_IFCHR
                } else {
                    libc::S_IFBLK
                };
            let device = if kind.is_fifo() {
                0
            } else {
                libc::makedev(
                    header.device_major()?.context("missing device major")?,
                    header.device_minor()?.context("missing device minor")?,
                )
            };
            // SAFETY: filename is NUL terminated; mode and device come from a checked tar header.
            if unsafe { libc::mknod(filename.as_ptr(), mode, device) } != 0 {
                return Err(io::Error::last_os_error()).with_context(|| format!("create {name}"));
            }
            restore_attributes(&path, &header)?;
            crate::attributes::restore(&path, &attributes, !payload_only)?;
        } else {
            bail!("unsupported archive entry: {name}");
        }
    }
    // A hard link may appear before its target in the archive.
    for (name, target, attributes) in links {
        check_parents(root, &name)?;
        check_parents(root, &target)?;
        ensure!(
            fs::symlink_metadata(root.join(&target))?.is_file(),
            "hard link target is not a regular file: {target}"
        );
        fs::hard_link(root.join(target), root.join(&name))?;
        crate::attributes::restore(&root.join(name), &attributes, false)?;
    }
    // Apply directory modes last, children first.
    directories.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.components().count()));
    for (path, header, attributes) in directories {
        restore_attributes(&path, &header)?;
        crate::attributes::restore(&path, &attributes, !payload_only)?;
    }
    Ok(())
}

pub fn unlink(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("unlink {}", path.display())),
    }
}

fn restore_attributes(path: &Path, header: &Header) -> Result<()> {
    // SAFETY: geteuid takes no pointers or arguments.
    if unsafe { libc::geteuid() } == 0 {
        let filename = CString::new(path.as_os_str().as_bytes())?;
        let uid = header
            .uid()?
            .try_into()
            .context("UID exceeds Linux range")?;
        let gid = header
            .gid()?
            .try_into()
            .context("GID exceeds Linux range")?;
        // SAFETY: filename is a valid C string. lchown does not follow symlinks.
        if unsafe { libc::lchown(filename.as_ptr(), uid, gid) } != 0 {
            return Err(io::Error::last_os_error()).context("restore file ownership");
        }
    }
    if !header.entry_type().is_symlink() {
        fs::set_permissions(path, fs::Permissions::from_mode(header.mode()? & 0o7777))?;
    }
    let time = FileTime::from_unix_time(header.mtime()?.try_into()?, 0);
    filetime::set_symlink_file_times(path, time, time)?;
    Ok(())
}
