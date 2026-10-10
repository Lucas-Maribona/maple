//! Linux extended attributes. ACLs and capabilities are kernel xattrs too.
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, ffi::CString, io, os::unix::ffi::OsStrExt, path::Path};
pub type Attributes = BTreeMap<String, Vec<u8>>;

pub fn read(path: &Path) -> Result<Attributes> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: valid C string, null buffer requests the required size.
    let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(Attributes::new());
        }
        return Err(error).context("list extended attributes");
    }
    let mut names = vec![0u8; size as usize];
    // SAFETY: buffer is writable for its declared length.
    let size = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    ensure!(
        size >= 0,
        "list extended attributes: {}",
        io::Error::last_os_error()
    );
    names.truncate(size as usize);
    let mut attributes = Attributes::new();
    for name in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let key = std::str::from_utf8(name)
            .context("xattr name must be UTF-8")?
            .to_owned();
        let name = CString::new(name)?;
        // SAFETY: valid C strings, null buffer requests size.
        let size =
            unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
        ensure!(
            size >= 0,
            "read extended attribute {key}: {}",
            io::Error::last_os_error()
        );
        let mut value = vec![0u8; size as usize];
        // SAFETY: buffer is writable for its declared length.
        let size = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        ensure!(
            size >= 0,
            "read extended attribute {key}: {}",
            io::Error::last_os_error()
        );
        value.truncate(size as usize);
        attributes.insert(key, value);
    }
    Ok(attributes)
}

pub fn restore(path: &Path, attributes: &Attributes, exact: bool) -> Result<()> {
    let current = if exact {
        read(path)?
    } else {
        Attributes::new()
    };
    let filename = CString::new(path.as_os_str().as_bytes())?;
    for key in current.keys().filter(|key| !attributes.contains_key(*key)) {
        let name = CString::new(key.as_bytes())?;
        // SAFETY: valid C strings; lremovexattr never follows the final symlink.
        if unsafe { libc::lremovexattr(filename.as_ptr(), name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error()).with_context(|| format!("remove xattr {key}"));
        }
    }
    for (key, value) in attributes {
        let name = CString::new(key.as_bytes())?;
        // SAFETY: valid C strings and readable buffer. Applied after chown/chmod,
        // which would otherwise clear capabilities or change the ACL mask.
        if unsafe {
            libc::lsetxattr(
                filename.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("restore xattr {key} on {}", path.display()));
        }
    }
    Ok(())
}

pub fn from_entry<R: io::Read>(entry: &mut tar::Entry<'_, R>) -> Result<Attributes> {
    let mut result = Attributes::new();
    if let Some(extensions) = entry.pax_extensions()? {
        for extension in extensions {
            let extension = extension?;
            let key = extension.key()?;
            if let Some(name) = key.strip_prefix("MAPLE.xattr.hex.") {
                ensure!(
                    !name.is_empty() && !name.contains('\0'),
                    "invalid xattr name"
                );
                let value = extension.value_bytes();
                ensure!(value.len().is_multiple_of(2), "invalid hex xattr");
                let bytes: Result<Vec<u8>> = value
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
                    .collect();
                result.insert(name.to_owned(), bytes?);
            } else if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                ensure!(
                    !name.is_empty() && !name.contains('\0'),
                    "invalid xattr name"
                );
                result.insert(name.to_owned(), extension.value_bytes().to_vec());
            } else if key == "SCHILY.acl.access" || key == "SCHILY.acl.default" {
                let name = if key.ends_with("access") {
                    "system.posix_acl_access"
                } else {
                    "system.posix_acl_default"
                };
                result
                    .entry(name.to_owned())
                    .or_insert(acl(extension.value()?)?);
            } else if let Some(name) = key.strip_prefix("LIBARCHIVE.xattr.") {
                use base64::Engine;
                ensure!(
                    !name.is_empty() && !name.contains(['%', '\0', '\n', '=']),
                    "unsupported encoded xattr name: {name}"
                );
                let value = base64::engine::general_purpose::STANDARD
                    .decode(extension.value_bytes())
                    .or_else(|_| {
                        base64::engine::general_purpose::STANDARD_NO_PAD
                            .decode(extension.value_bytes())
                    })
                    .context("invalid base64 xattr")?;
                if let Some(old) = result.insert(name.into(), value.clone()) {
                    ensure!(old == value, "inconsistent duplicate xattr: {name}");
                }
            }
        }
    }
    Ok(result)
}

// Linux POSIX ACL xattr format, version 2, little endian. Numeric qualifiers
// avoid resolving names against the host when managing an alternate root.
fn acl(text: &str) -> Result<Vec<u8>> {
    let mut bytes = 2u32.to_le_bytes().to_vec();
    for entry in text.split(',').filter(|entry| !entry.trim().is_empty()) {
        let fields: Vec<_> = entry.trim().split(':').collect();
        ensure!((3..=4).contains(&fields.len()), "invalid POSIX ACL entry");
        let named = !fields[1].is_empty();
        let tag: u16 = match (fields[0], named) {
            ("user", false) => 1,
            ("user", true) => 2,
            ("group", false) => 4,
            ("group", true) => 8,
            ("mask", false) => 16,
            ("other", false) => 32,
            _ => anyhow::bail!("invalid POSIX ACL tag"),
        };
        let id = if named {
            fields
                .get(3)
                .unwrap_or(&fields[1])
                .parse::<u32>()
                .context("ACL qualifiers must be numeric (use tar --numeric-owner)")?
        } else {
            u32::MAX
        };
        let permissions = fields[2].as_bytes();
        ensure!(
            permissions.len() == 3
                && matches!(permissions[0], b'r' | b'-')
                && matches!(permissions[1], b'w' | b'-')
                && matches!(permissions[2], b'x' | b'-'),
            "invalid ACL permissions"
        );
        let mode: u16 = u16::from(permissions[0] == b'r') * 4
            + u16::from(permissions[1] == b'w') * 2
            + u16::from(permissions[2] == b'x');
        bytes.extend(tag.to_le_bytes());
        bytes.extend(mode.to_le_bytes());
        bytes.extend(id.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_xattrs_acls_and_exact_directory_restore() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("dir");
        std::fs::create_dir(&path).unwrap();
        let mut expected = Attributes::new();
        expected.insert("user.binary".into(), vec![0, 1, 10, 255]);
        let access = acl("user::rwx,user:12345:r--,group::r-x,mask::r-x,other::---").unwrap();
        expected.insert("system.posix_acl_access".into(), access.clone());
        expected.insert("system.posix_acl_default".into(), access);
        restore(&path, &expected, true).unwrap();
        assert_eq!(read(&path).unwrap(), expected);
        let mut archive = Vec::new();
        crate::archive::write_paths(&mut archive, root.path(), &["dir".into()]).unwrap();
        let mut changed = Attributes::new();
        changed.insert("user.extra".into(), b"remove on restore".to_vec());
        restore(&path, &changed, true).unwrap();
        crate::archive::extract(archive.as_slice(), root.path(), false).unwrap();
        assert_eq!(read(&path).unwrap(), expected);
        assert!(acl("user:root:rwx").is_err());
        assert!(acl("user::bad").is_err());
    }

    #[test]
    fn capability_pax_bytes_survive_without_needing_privileges_to_parse() {
        let capability = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut builder = tar::Builder::new(Vec::new());
        builder
            .append_pax_extensions([("SCHILY.xattr.security.capability", capability.as_slice())])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "file", b"x".as_slice())
            .unwrap();
        let data = builder.into_inner().unwrap();
        let mut archive = tar::Archive::new(data.as_slice());
        let mut entry = archive.entries().unwrap().next().unwrap().unwrap();
        assert_eq!(
            from_entry(&mut entry).unwrap()["security.capability"],
            capability
        );
    }
}
