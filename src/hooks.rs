//! Trusted scripts run within the file transaction. Recovery never replays them.
use crate::model::Installed;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    process::{Command, Stdio},
};

pub const MAX_SCRIPT: u64 = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hook {
    PreInstall,
    PostInstall,
    PreUpgrade,
    PostUpgrade,
    PreRemove,
    PostRemove,
}
impl Hook {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreInstall => "pre-install",
            Self::PostInstall => "post-install",
            Self::PreUpgrade => "pre-upgrade",
            Self::PostUpgrade => "post-upgrade",
            Self::PreRemove => "pre-remove",
            Self::PostRemove => "post-remove",
        }
    }
}
pub fn validate_script(script: &str) -> Result<()> {
    ensure!(
        script.len() as u64 <= MAX_SCRIPT && !script.contains('\0'),
        "invalid hook script (limit 64 KiB, no NUL)"
    );
    ensure!(
        !script.starts_with("#!")
            || matches!(script.lines().next(), Some("#!/bin/sh" | "#!/bin/bash")),
        "unsupported hook interpreter; use /bin/sh or /bin/bash"
    );
    Ok(())
}
pub fn authorize<'a>(records: impl Iterator<Item = &'a Installed>, trusted: bool) -> Result<()> {
    for record in records {
        ensure!(
            record.package.hooks.is_empty() || trusted,
            "{} contains lifecycle hooks; review the package and pass --trust-hooks to authorize scripts for this transaction",
            record.package.name
        );
    }
    Ok(())
}
pub fn execute(root: &Path, record: &Installed, hook: Hook, old: Option<&Installed>) -> Result<()> {
    let Some(script) = record.hook_scripts.get(&hook) else {
        return Ok(());
    };
    ensure!(
        record.package.hooks.contains(&hook),
        "undeclared stored hook"
    );
    validate_script(script)?;
    let interpreter = if script.starts_with("#!/bin/bash\n") {
        "/bin/bash"
    } else {
        "/bin/sh"
    };
    let mut command = if root == Path::new("/") {
        Command::new(interpreter)
    } else {
        crate::triggers::validate_mounts(root, &std::fs::read_to_string("/proc/self/mountinfo")?)?;
        let mut c = Command::new("/usr/bin/bwrap");
        c.args(crate::triggers::sandbox_arguments(root)?)
            .arg("--")
            .arg(interpreter);
        c
    };
    let status = command
        .args(["-c", script, hook.name(), &record.package.version.0])
        .args(old.map(|p| p.package.version.0.as_str()))
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C")
        .env("MAPLE_PACKAGE", &record.package.name)
        .env(
            "SYSTEMD_OFFLINE",
            if root == Path::new("/") { "0" } else { "1" },
        )
        .current_dir("/")
        .stdin(Stdio::null())
        .status()
        .with_context(|| {
            format!(
                "run {} for {}; offline hooks require Bubblewrap and a target shell",
                hook.name(),
                record.package.name
            )
        })?;
    ensure!(
        status.success(),
        "{} hook for {} failed: {status}; rollback covers tracked files, not script side effects",
        hook.name(),
        record.package.name
    );
    Ok(())
}
