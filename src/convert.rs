//! Arch package conversion without extracting files or executing package code.
use crate::{
    hooks::Hook,
    model::Package,
    version::{Version, VersionReq},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read},
    path::Path,
};

pub fn convert(input: &Path, output: &Path) -> Result<Package> {
    let mut staging = tempfile::NamedTempFile::new()?;
    let file = File::open(input)?;
    let mut decoder = zstd::stream::read::Decoder::new(file)?;
    io::copy(
        &mut decoder,
        &mut crate::space::TemporaryWriter(staging.as_file_mut()),
    )?;
    let mut archive = tar::Archive::new(crate::pax::Reader::new(File::open(staging.path())?));
    let mut info = None;
    let mut install = None;
    let mut seen = BTreeSet::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = crate::package::clean_path(&entry.path()?)?;
        ensure!(
            seen.insert(name.clone()),
            "duplicate Arch archive member: {name}"
        );
        if name.starts_with('.') && !name.is_empty() {
            ensure!(
                entry.header().entry_type().is_file(),
                "Arch metadata must be a regular file: {name}"
            );
            ensure!(
                entry.size() <= 16 * 1024 * 1024,
                "Arch metadata too large: {name}"
            );
            match name.as_str() {
                ".PKGINFO" => {
                    let mut s = String::new();
                    entry.read_to_string(&mut s)?;
                    info = Some(s);
                }
                ".INSTALL" => {
                    ensure!(
                        entry.size() <= crate::hooks::MAX_SCRIPT / 2,
                        "Arch install script too large"
                    );
                    let mut s = String::new();
                    entry.read_to_string(&mut s)?;
                    install = Some(s);
                }
                ".BUILDINFO" | ".MTREE" => eprintln!(
                    "notice: {name} is build provenance; omitted (output archive is validated independently)"
                ),
                _ => bail!("unsupported Arch metadata member: {name}"),
            }
        }
        ensure!(
            !name.starts_with("usr/share/libalpm/hooks/")
                && !name.starts_with("etc/pacman.d/hooks/"),
            "unsupported ALPM transaction hook: {name}; translate it to a Maple trigger manually"
        );
    }
    let mut package = metadata(&info.context("Arch package is missing .PKGINFO")?)?;
    let hooks = [
        Hook::PreInstall,
        Hook::PostInstall,
        Hook::PreUpgrade,
        Hook::PostUpgrade,
        Hook::PreRemove,
        Hook::PostRemove,
    ];
    if install.is_some() {
        package.hooks = hooks.to_vec();
        package
            .dependencies
            .entry("bash".into())
            .or_insert_with(VersionReq::any);
        eprintln!(
            "notice: .INSTALL preserved as six Bash lifecycle adapters; top-level code runs at every phase, even when its function is absent; review assumptions before --trust-hooks"
        );
    }
    package.validate()?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut result = tempfile::NamedTempFile::new_in(parent)?;
    {
        let encoder =
            xz2::write::XzEncoder::new(crate::space::TemporaryWriter(result.as_file_mut()), 1);
        let mut builder = tar::Builder::new(encoder);
        append(
            &mut builder,
            "metadata.toml",
            toml::to_string(&package)?.as_bytes(),
        )?;
        if let Some(script) = install {
            ensure!(!script.contains('\0'), "NUL in Arch install script");
            for hook in hooks {
                let function = hook.name().replace('-', "_");
                let adapter = format!(
                    "#!/bin/bash\n{script}\nif declare -F {function} >/dev/null; then\n  {function} \"$@\"\nfi\n"
                );
                append(
                    &mut builder,
                    &format!("hooks/{}", hook.name()),
                    adapter.as_bytes(),
                )?;
            }
        }
        let mut archive = tar::Archive::new(crate::pax::Reader::new(File::open(staging.path())?));
        for entry in archive.entries()? {
            let mut entry = entry?;
            let name = crate::package::clean_path(&entry.path()?)?;
            if name.is_empty() || name.starts_with('.') {
                continue;
            }
            let attributes = crate::attributes::from_entry(&mut entry)?;
            let mut mtime = None;
            if let Some(extensions) = entry.pax_extensions()? {
                for ext in extensions {
                    let ext = ext?;
                    let key = ext.key()?;
                    ensure!(
                        matches!(
                            key,
                            "path"
                                | "linkpath"
                                | "size"
                                | "uid"
                                | "gid"
                                | "uname"
                                | "gname"
                                | "mtime"
                                | "atime"
                                | "ctime"
                                | "SCHILY.acl.access"
                                | "SCHILY.acl.default"
                        ) || key.starts_with("SCHILY.xattr.")
                            || key.starts_with("MAPLE.xattr.hex.")
                            || key.starts_with("LIBARCHIVE.xattr."),
                        "unsupported PAX feature {key} on {name}"
                    );
                    if key == "mtime" {
                        let value = ext.value()?;
                        let (seconds, fraction) = value
                            .split_once('.')
                            .map_or((value, None), |(s, f)| (s, Some(f)));
                        mtime = Some(seconds.parse::<u64>().context("unsupported PAX mtime")?);
                        if let Some(fraction) = fraction {
                            ensure!(
                                !fraction.is_empty()
                                    && fraction.bytes().all(|b| b.is_ascii_digit()),
                                "invalid fractional mtime"
                            );
                            eprintln!("notice: {name}: fractional mtime truncated to seconds");
                        }
                    }
                    if matches!(key, "atime" | "ctime") {
                        eprintln!(
                            "notice: {name}: {key} not represented in Maple (timestamps have one-second precision)"
                        );
                    }
                }
            }
            let attrs: Vec<_> = attributes
                .iter()
                .map(|(k, v)| (format!("SCHILY.xattr.{k}"), v))
                .collect();
            builder.append_pax_extensions(attrs.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
            let mut header = entry.header().clone();
            header.set_size(entry.size());
            if let Some(mtime) = mtime {
                header.set_mtime(mtime);
            }
            let path = format!("payload/{name}");
            if header.entry_type().is_hard_link() {
                let target = crate::package::clean_path(
                    &entry.link_name()?.context("missing hard link target")?,
                )?;
                builder.append_link(&mut header, &path, format!("payload/{target}"))?;
            } else if header.entry_type().is_symlink() {
                builder.append_link(
                    &mut header,
                    &path,
                    entry.link_name()?.context("missing symlink target")?,
                )?;
            } else {
                builder.append_data(&mut header, &path, &mut entry)?;
            }
        }
        builder.into_inner()?.finish()?;
    }
    // Reuse the installer's complete structural and metadata validation before
    // publishing. No target filesystem or package scripts are touched here.
    crate::package::Prepared::open(result.path())
        .context("converted archive failed Maple validation")?;
    result.as_file().sync_all()?;
    result
        .persist_noclobber(output)
        .context("write output (must not already exist)")?;
    Ok(package)
}
fn append<W: io::Write>(builder: &mut tar::Builder<W>, path: &str, bytes: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, path, bytes)?;
    Ok(())
}
fn relation(text: &str) -> Result<(String, VersionReq)> {
    let i = text.find(['<', '>', '=']).unwrap_or(text.len());
    let name = &text[..i];
    crate::model::validate_name(name)?;
    let req = if i == text.len() {
        VersionReq::any()
    } else {
        VersionReq::try_from(text[i..].to_owned())?
    };
    Ok((name.into(), req))
}
fn add_relation(table: &mut BTreeMap<String, VersionReq>, text: &str) -> Result<()> {
    let (name, req) = relation(text)?;
    ensure!(
        table.insert(name.clone(), req).is_none(),
        "multiple constraints for {name} are unsupported; combine them manually"
    );
    Ok(())
}
fn metadata(text: &str) -> Result<Package> {
    let mut fields: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (k, v) = line
            .split_once(" = ")
            .context("invalid .PKGINFO assignment")?;
        fields.entry(k).or_default().push(v);
    }
    let one = |key| -> Result<&str> {
        let v = fields
            .get(key)
            .with_context(|| format!("missing .PKGINFO {key}"))?;
        ensure!(v.len() == 1, "duplicate .PKGINFO {key}");
        Ok(v[0])
    };
    let mut p = Package {
        name: one("pkgname")?.into(),
        version: Version(one("pkgver")?.into()),
        description: fields.get("pkgdesc").map_or("", |v| v[0]).into(),
        provides: BTreeMap::new(),
        dependencies: BTreeMap::new(),
        optional_dependencies: BTreeMap::new(),
        conflicts: BTreeMap::new(),
        config_files: Vec::new(),
        triggers: Vec::new(),
        hooks: Vec::new(),
    };
    for (key, values) in fields {
        for v in values {
            match key {
                "pkgname" | "pkgver" | "pkgdesc" => {}
                "depend" => add_relation(&mut p.dependencies, v)?,
                "optdepend" => add_relation(
                    &mut p.optional_dependencies,
                    v.split_once(": ").map_or(v, |(r, _)| r),
                )?,
                "conflict" => add_relation(&mut p.conflicts, v)?,
                "provides" => {
                    let (name, version) = v.split_once('=').unwrap_or((v, "*"));
                    crate::model::validate_name(name)?;
                    ensure!(
                        p.provides
                            .insert(name.into(), Version(version.into()))
                            .is_none(),
                        "duplicate provide: {name}"
                    );
                }
                "backup" => p.config_files.push(v.into()),
                "replaces" => bail!(
                    "unsupported Arch replaces = {v}; Maple does not automatically remove packages"
                ),
                "pkgbase" | "url" | "builddate" | "packager" | "size" | "arch" | "license"
                | "group" | "makedepend" | "checkdepend" | "xdata" => eprintln!(
                    "notice: informational .PKGINFO {key} = {v} is not represented in Maple; use an architecture-specific repository"
                ),
                _ => bail!("unsupported .PKGINFO field: {key}"),
            }
        }
    }
    p.validate()?;
    Ok(p)
}
