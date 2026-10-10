//! Fixed, idempotent maintenance commands. Offline commands run only in a
//! Bubblewrap sandbox; never fall back to executing against the host.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

// Declaration order is execution order. Accounts must precede tmpfiles ownership;
// caches and persistent files must precede initramfs construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    SystemdSysusers,
    SystemdTmpfiles,
    Ldconfig,
    Initramfs,
    Systemd,
}

pub fn execute(root: &Path, triggers: &BTreeSet<Trigger>) -> Result<bool> {
    execute_with(root, triggers, |program, args| {
        let mut command = if root == Path::new("/") {
            Command::new(program)
        } else {
            validate_mounts(root, &fs::read_to_string("/proc/self/mountinfo")?)?;
            let mut command = Command::new("/usr/bin/bwrap");
            command
                .args(sandbox_arguments(root)?)
                .arg("--")
                .arg(program);
            command
        };
        let status = command.args(args).env_clear()
            .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LANG", "C")
            .env("SYSTEMD_OFFLINE", if root == Path::new("/") { "0" } else { "1" })
            .current_dir("/").stdin(std::process::Stdio::null()).status()
            .with_context(|| format!("run trigger {program}; offline maintenance requires /usr/bin/bwrap and working Linux namespaces"))?;
        ensure!(status.success(), "trigger {program} failed: {status}");
        if root != Path::new("/") {
            sync_offline_filesystems(root)?;
        }
        Ok(())
    })
}

fn execute_with(
    root: &Path,
    triggers: &BTreeSet<Trigger>,
    mut invoke: impl FnMut(&str, &[String]) -> Result<()>,
) -> Result<bool> {
    let offline = root != Path::new("/");
    for trigger in triggers {
        let (program, args): (&str, Vec<String>) = match trigger {
            Trigger::SystemdSysusers => ("/usr/bin/systemd-sysusers", vec![]),
            Trigger::SystemdTmpfiles => (
                "/usr/bin/systemd-tmpfiles",
                if offline {
                    vec!["--create".into(), "--boot".into()]
                } else {
                    vec!["--create".into()]
                },
            ),
            Trigger::Ldconfig => ("/sbin/ldconfig", vec![]),
            Trigger::Systemd if offline => {
                // No running target manager exists. Units are read at its next boot.
                crate::output::step(
                    "Offline systemd daemon-reload is unnecessary; units will be read at boot.",
                );
                continue;
            }
            Trigger::Systemd => ("/usr/bin/systemctl", vec!["daemon-reload".into()]),
            Trigger::Initramfs if offline => {
                for kernel in kernels(root)? {
                    invoke(
                        "/usr/bin/dracut",
                        &[
                            "--force".into(),
                            "--no-hostonly".into(),
                            "--no-hostonly-cmdline".into(),
                            "--kver".into(),
                            kernel,
                        ],
                    )?;
                }
                continue;
            }
            Trigger::Initramfs => (
                "/usr/bin/dracut",
                vec!["--regenerate-all".into(), "--force".into()],
            ),
        };
        invoke(program, &args).with_context(|| format!("{trigger:?} maintenance failed"))?;
    }
    Ok(true)
}

fn kernels(root: &Path) -> Result<Vec<String>> {
    crate::database::check_parents(root, "usr/lib/modules/entry")?;
    let mut kernels = Vec::new();
    for entry in fs::read_dir(root.join("usr/lib/modules"))
        .context("offline initramfs needs installed target kernels in /usr/lib/modules")?
    {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_dir(),
            "target module entries must be real directories"
        );
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("kernel version must be UTF-8"))?;
        ensure!(
            !name.starts_with('.')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b)),
            "invalid target kernel version: {name}"
        );
        kernels.push(name);
    }
    kernels.sort();
    ensure!(
        !kernels.is_empty(),
        "offline initramfs needs at least one installed target kernel"
    );
    Ok(kernels)
}

pub(crate) fn sandbox_arguments(root: &Path) -> Result<Vec<OsString>> {
    ensure!(
        root.is_absolute() && root != Path::new("/"),
        "invalid offline root"
    );
    for name in ["dev", "proc", "sys", "run", "tmp"] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "sandbox mount point must be a real directory: {name}"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let mut args: Vec<OsString> = [
        "--unshare-pid",
        "--unshare-net",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
        "--new-session",
        "--cap-drop",
        "ALL",
        "--bind",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(root.as_os_str().to_owned());
    args.push("/".into());
    for cap in [
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_SETUID",
        "CAP_SETGID",
        "CAP_SETFCAP",
        // dracut/ldconfig may chroot into the image staging directory. The
        // sandbox has already discarded the host root and its descriptors.
        "CAP_SYS_CHROOT",
    ] {
        args.extend(["--cap-add".into(), cap.into()]);
    }
    for (kind, destination) in [
        ("--dev", "/dev"),
        ("--proc", "/proc"),
        ("--tmpfs", "/sys"),
        ("--tmpfs", "/run"),
        ("--tmpfs", "/tmp"),
    ] {
        args.extend([kind.into(), destination.into()]);
    }
    args.extend(["--remount-ro".into(), "/proc".into()]);
    // Maintenance may change derived state, never Maple's journal or package DB.
    args.extend([
        "--ro-bind".into(),
        root.join("var/lib/maple").into_os_string(),
        "/var/lib/maple".into(),
        "--chdir".into(),
        "/".into(),
    ]);
    Ok(args)
}

// Refuse mounted host trees even though the runtime mounts are hidden later.
// Separate target filesystems (e.g. /boot) are allowed only when no mount of
// their device exists outside the target. This deliberately rejects bind mounts.
pub(crate) fn validate_mounts(root: &Path, mountinfo: &str) -> Result<()> {
    let mounts: Result<Vec<_>> = mountinfo
        .lines()
        .map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            ensure!(fields.len() >= 6, "invalid mountinfo record");
            Ok((fields[2], unescape_mount(fields[4])?))
        })
        .collect();
    let mounts = mounts?;
    for (device, path) in &mounts {
        if path.starts_with(root) {
            ensure!(
                !mounts
                    .iter()
                    .any(|(other, location)| other == device && !location.starts_with(root)),
                "offline root contains a filesystem also mounted on the host: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn unescape_mount(text: &str) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'\\' {
            ensure!(offset + 3 < bytes.len(), "invalid mountinfo escape");
            let value =
                u8::from_str_radix(std::str::from_utf8(&bytes[offset + 1..offset + 4])?, 8)?;
            result.push(value);
            offset += 4;
        } else {
            result.push(bytes[offset]);
            offset += 1;
        }
    }
    Ok(OsString::from_vec(result).into())
}

// Flush derived files before durably acknowledging their trigger. Mount points
// were already checked for host aliases before execution.
fn sync_offline_filesystems(root: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let mut paths = BTreeSet::from([root.to_owned()]);
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let field = line
            .split_whitespace()
            .nth(4)
            .context("invalid mountinfo")?;
        let path = unescape_mount(field)?;
        if path.starts_with(root) {
            paths.insert(path);
        }
    }
    for path in paths {
        let file = fs::File::open(&path)?;
        // SAFETY: live file descriptor for this target filesystem.
        if unsafe { libc::syncfs(file.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("flush maintenance output on {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_deduplication_and_offline_target_kernels() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("usr/lib/modules/6.12.1-mercury")).unwrap();
        let triggers = [
            Trigger::Systemd,
            Trigger::Initramfs,
            Trigger::Ldconfig,
            Trigger::SystemdTmpfiles,
            Trigger::SystemdSysusers,
            Trigger::SystemdSysusers,
        ]
        .into_iter()
        .collect();
        let mut calls = Vec::new();
        execute_with(root.path(), &triggers, |p, a| {
            calls.push((p.to_owned(), a.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            calls.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            [
                "/usr/bin/systemd-sysusers",
                "/usr/bin/systemd-tmpfiles",
                "/sbin/ldconfig",
                "/usr/bin/dracut"
            ]
        );
        assert_eq!(
            calls[3].1,
            [
                "--force",
                "--no-hostonly",
                "--no-hostonly-cmdline",
                "--kver",
                "6.12.1-mercury"
            ]
        );
        let mut attempts = 0;
        assert!(
            execute_with(root.path(), &triggers, |_, _| {
                attempts += 1;
                anyhow::bail!("failed")
            })
            .is_err()
        );
        assert_eq!(attempts, 1);
    }
    #[test]
    fn mount_aliases_and_symlink_mount_points_are_rejected() {
        let info =
            "1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 8:1 /etc /target/etc rw - ext4 /dev/a rw\n";
        assert!(validate_mounts(Path::new("/target"), info).is_err());
        assert!(validate_mounts(Path::new("/target"), &info.replace("2 1 8:1", "2 1 8:2")).is_ok());
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc", root.path().join("proc")).unwrap();
        assert!(sandbox_arguments(root.path()).is_err());
    }
}
