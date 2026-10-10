use crate::version::{Version, VersionReq};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<crate::hooks::Hook>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provides: BTreeMap<String, Version>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, VersionReq>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conflicts: BTreeMap<String, VersionReq>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<crate::triggers::Trigger>,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, VersionReq>,
}

impl Package {
    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        self.version.validate()?;
        for version in self.provides.values().filter(|v| v.0 != "*") {
            version.validate()?;
        }
        ensure!(
            self.hooks
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == self.hooks.len(),
            "duplicate hook"
        );
        for path in &self.config_files {
            ensure!(
                !path.is_empty()
                    && crate::package::clean_path(std::path::Path::new(path))? == *path
                    && !path.ends_with(".maple-new"),
                "invalid config path: {path}"
            );
        }
        for name in self
            .dependencies
            .keys()
            .chain(self.optional_dependencies.keys())
            .chain(self.conflicts.keys())
            .chain(self.provides.keys())
        {
            validate_name(name)?;
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    let valid_start = name
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    let valid_chars = name
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-_.+".contains(&c));
    ensure!(valid_start && valid_chars, "invalid package name: {name}");
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_repository")]
    pub repository: String,
    #[serde(default = "default_retention")]
    pub rollback_retention: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    #[serde(default)]
    pub sha256: BTreeMap<String, String>,
    #[serde(default, rename = "package")]
    pub packages: Vec<Package>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Installed {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hook_scripts: BTreeMap<crate::hooks::Hook, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config_defaults: BTreeMap<String, Vec<u8>>,
    pub package: Package,
    pub files: Vec<String>,
    pub directories: Vec<String>,
}

fn default_repository() -> String {
    crate::repository::DEFAULT_REPOSITORY_URL.to_owned()
}
fn default_retention() -> usize {
    1
}
