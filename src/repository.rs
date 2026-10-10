use std::{collections::HashSet, fs, io, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client};
use tempfile::NamedTempFile;

use crate::model::{Config, Package, Repository};

// Archives live beside this index, under packages/.
pub const DEFAULT_REPOSITORY_URL: &str =
    "https://raw.githubusercontent.com/Lucas-Maribona/mcxpkgs/main/repository.toml";

pub struct Remote {
    client: Client,
    pub index: Repository,
    base: Url,
}

impl Remote {
    pub fn load(config_path: &Path, override_url: Option<&str>) -> Result<Self> {
        let repository = match override_url {
            Some(url) => url.to_owned(),
            None => match fs::read_to_string(config_path) {
                Ok(text) => toml::from_str::<Config>(&text)?.repository,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    DEFAULT_REPOSITORY_URL.to_owned()
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", config_path.display()));
                }
            },
        };
        let base = Url::parse(&repository).context("invalid repository URL")?;
        ensure!(
            matches!(base.scheme(), "http" | "https"),
            "repository URL must use HTTP or HTTPS"
        );
        let client = Client::builder()
            .user_agent(concat!("maple/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(1800))
            .build()?;
        let response = client.get(base.clone()).send()?.error_for_status()?;
        // Use the final URL so redirects also work for relative package paths.
        let base = response.url().clone();
        let index: Repository =
            toml::from_str(&response.text()?).context("invalid repository TOML")?;
        let mut names = HashSet::new();
        for (key, hash) in &index.sha256 {
            ensure!(
                hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid SHA-256 for {key}"
            );
            ensure!(
                index
                    .packages
                    .iter()
                    .any(|p| format!("{}/{}", p.name, p.version) == *key),
                "checksum has no repository entry: {key}"
            );
        }
        for entry in &index.packages {
            entry.validate()?;
            ensure!(
                names.insert((&entry.name, &entry.version.0)),
                "duplicate repository package version: {}",
                entry.name
            );
        }
        Ok(Self {
            client,
            index,
            base,
        })
    }

    pub fn download(&self, package: &Package, position: usize) -> Result<NamedTempFile> {
        let mut url = self.base.join("packages/")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid repository base URL"))?
            .pop_if_empty()
            .push(&package.name)
            .push(&format!("{}.maple", package.version));
        let file = crate::download::download(
            &self.client,
            &url,
            &format!("[{position}] {}-{}", package.name, package.version),
        )
        .with_context(|| format!("download {url}"))?;
        if let Some(expected) = self
            .index
            .sha256
            .get(&format!("{}/{}", package.name, package.version))
        {
            ensure!(
                sha256(file.path())?.eq_ignore_ascii_case(expected),
                "SHA-256 mismatch for {}",
                package.name
            );
        }
        Ok(file)
    }
}

pub fn sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
