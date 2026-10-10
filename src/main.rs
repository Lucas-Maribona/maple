use maple::{database, hooks, model, output, package, repository, resolver, transaction};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};

use database::Database;
use package::Prepared;
use repository::Remote;

#[derive(Parser)]
#[command(
    version,
    about = "A simple package manager for Mercury and its derivatives"
)]
struct Cli {
    /// Filesystem to manage; useful for building and testing a distro
    #[arg(long, global = true, default_value = "/")]
    root: PathBuf,
    /// Repository index URL; overrides config
    #[arg(long, global = true)]
    repo: Option<String>,
    /// Accept the displayed transaction without prompting
    #[arg(long, global = true)]
    noconfirm: bool,
    /// Authorize lifecycle scripts in every package in this transaction
    #[arg(long, global = true)]
    trust_hooks: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Search package names and descriptions in the repository database
    Search {
        #[arg(default_value = "")]
        query: String,
    },
    /// Install repository packages or local .maple archives
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Remove installed packages and their tracked files
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Update all installed packages, including maple itself
    Update,
    /// Undo the latest completed install, update, or removal
    Rollback,
    /// Restore an interrupted transaction (also automatic before modifying commands)
    Recover,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if error.use_stderr() {
                output::error(error.to_string());
            } else {
                for line in error.to_string().lines() {
                    output::step(line);
                }
            }
            return ExitCode::from(error.exit_code() as u8);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            output::error(format!("{error:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let writable = !matches!(cli.command, Action::Search { .. });
    let mut db = Database::open(&cli.root, writable)?;
    let config = db.root.join("etc/maple/config.toml");
    match &cli.command {
        Action::Search { query } => {
            let remote = load_remote(&config, cli.repo.as_deref())?;
            let query = query.to_lowercase();
            let mut matches = 0;
            for p in &remote.index.packages {
                if p.name.to_lowercase().contains(&query)
                    || p.description.to_lowercase().contains(&query)
                {
                    output::package(
                        &p.name,
                        &p.version,
                        &p.description,
                        db.installed.contains_key(&p.name),
                    );
                    matches += 1;
                }
            }
            if matches == 0 {
                output::step("No packages found.");
            }
        }
        Action::Install { packages } => {
            let mut local = Vec::new();
            let mut requests = Vec::new();
            let mut names = HashSet::new();
            for input in packages {
                if input.ends_with(".maple") {
                    let package = Prepared::open(Path::new(input))?;
                    ensure!(
                        names.insert(package.record.package.name.clone()),
                        "package requested more than once: {}",
                        package.record.package.name
                    );
                    local.push(package);
                } else {
                    let (name, version) = input
                        .split_once('=')
                        .map_or((input.as_str(), None), |(n, v)| (n, Some(v.to_owned())));
                    model::validate_name(name)?;
                    ensure!(
                        names.insert(name.to_owned()),
                        "package requested more than once: {name}"
                    );
                    requests.push(resolver::Request {
                        name: name.to_owned(),
                        version,
                    });
                }
            }
            let metadata: Vec<_> = local.iter().map(|p| p.record.package.clone()).collect();
            let offline = resolver::resolve(&db.installed, &metadata, &requests, &[], false);
            let prepared = if requests.is_empty() && offline.is_ok() {
                let mut by_name: std::collections::BTreeMap<_, _> = local
                    .into_iter()
                    .map(|p| (p.record.package.name.clone(), p))
                    .collect();
                offline?
                    .into_iter()
                    .map(|p| by_name.remove(&p.name).unwrap())
                    .collect()
            } else {
                let remote = load_remote(&config, cli.repo.as_deref())
                    .with_context(|| format!("{}", offline.unwrap_err()))?;
                let plan = resolver::resolve(
                    &db.installed,
                    &metadata,
                    &requests,
                    &remote.index.packages,
                    false,
                )?;
                prepare_plan(plan, local, &remote)?
            };
            install_all(&mut db, prepared, cli.noconfirm, cli.trust_hooks)?;
        }
        Action::Remove { packages } => {
            let mut names = Vec::new();
            for name in packages {
                let record = db
                    .installed
                    .get(name)
                    .with_context(|| format!("package is not installed: {name}"))?;
                db.check_removal(record)?;
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            let mut final_state = db.installed.clone();
            for name in &names {
                final_state.remove(name);
            }
            resolver::validate(&final_state)?;
            let labels: Vec<_> = names
                .iter()
                .map(|name| format!("{name}-{}", db.installed[name].package.version))
                .collect();
            output::plan(&labels);
            if !output::confirm(cli.noconfirm)? {
                return Ok(());
            }
            hooks::authorize(names.iter().map(|n| &db.installed[n]), cli.trust_hooks)?;
            db.remove_all(&names)?;
            output::step("Removal complete.");
        }
        Action::Update => {
            let remote = load_remote(&config, cli.repo.as_deref())?;
            for old in db.installed.values() {
                if !remote
                    .index
                    .packages
                    .iter()
                    .any(|p| p.name == old.package.name)
                {
                    output::warn(format!(
                        "{}-{} is absent from the repository; keeping installed version",
                        old.package.name, old.package.version
                    ));
                }
            }
            let plan = resolver::resolve(&db.installed, &[], &[], &remote.index.packages, true)?;
            let mut held_back = false;
            for old in db.installed.values() {
                let selected = plan
                    .iter()
                    .find(|p| p.name == old.package.name)
                    .unwrap_or(&old.package);
                if remote.index.packages.iter().any(|p| {
                    p.name == selected.name && p.version.compare(&selected.version).is_gt()
                }) {
                    output::warn(format!(
                        "{} stays at compatible version {}; newer repository versions could not be selected",
                        selected.name, selected.version
                    ));
                    held_back = true;
                }
            }
            if plan.is_empty() && held_back {
                transaction::recover(&db.root)?;
                output::step("No compatible upgrades available.");
            } else {
                let prepared = prepare_plan(plan, Vec::new(), &remote)?;
                install_all(&mut db, prepared, cli.noconfirm, cli.trust_hooks)?;
            }
        }
        Action::Rollback => {
            ensure!(
                transaction::can_rollback(&db.root),
                "no completed transaction to roll back"
            );
            output::step(
                "Restore the files and package records from before the latest transaction.",
            );
            if output::confirm(cli.noconfirm)? {
                transaction::rollback(&db.root)?;
                transaction::prune(&db.root)?;
                output::step("Rollback complete.");
            }
        }
        Action::Recover => {
            transaction::recover(&db.root)?;
            output::step("Recovery complete; no interrupted transaction or maintenance remains.");
        }
    }
    Ok(())
}

fn prepare_plan(
    plan: Vec<model::Package>,
    local: Vec<Prepared>,
    remote: &Remote,
) -> Result<Vec<Prepared>> {
    let mut local: std::collections::BTreeMap<_, _> = local
        .into_iter()
        .map(|p| (p.record.package.name.clone(), p))
        .collect();
    let mut downloads = output::DownloadBatch::default();
    let mut prepared = Vec::new();
    for entry in plan {
        prepared.push(match local.remove(&entry.name) {
            Some(p) => p,
            None => fetch(remote, &entry, &mut downloads)?,
        });
    }
    downloads.finish();
    Ok(prepared)
}

fn load_remote(config: &Path, url: Option<&str>) -> Result<Remote> {
    output::step("Refreshing repository index...");
    Remote::load(config, url)
}

fn fetch(
    remote: &Remote,
    entry: &model::Package,
    downloads: &mut output::DownloadBatch,
) -> Result<Prepared> {
    let position = downloads.next_position();
    let download = remote.download(entry, position)?;
    let prepared = Prepared::open(download.path())?;
    ensure!(
        prepared.record.package.name == entry.name
            && prepared.record.package.version == entry.version
            && prepared.record.package.dependencies == entry.dependencies
            && prepared.record.package.provides == entry.provides
            && prepared.record.package.optional_dependencies == entry.optional_dependencies
            && prepared.record.package.conflicts == entry.conflicts
            && prepared.record.package.config_files == entry.config_files
            && prepared.record.package.triggers == entry.triggers
            && prepared.record.package.hooks == entry.hooks,
        "downloaded package metadata does not match repository entry for {}",
        entry.name
    );
    downloads.completed(download.as_file().metadata()?.len());
    Ok(prepared)
}

fn install_all(
    db: &mut Database,
    prepared: Vec<Prepared>,
    noconfirm: bool,
    trust_hooks: bool,
) -> Result<()> {
    if prepared.is_empty() {
        transaction::recover(&db.root)?;
        output::step("Everything is up to date.");
        return Ok(());
    }
    hooks::authorize(prepared.iter().map(|p| &p.record), trust_hooks)?;
    // Check incoming packages against each other before writing any files.
    // This temporary view is discarded even if a check fails.
    let original = db.installed.clone();
    let check = (|| -> Result<()> {
        let mut names = HashSet::new();
        for package in &prepared {
            let record = &package.record;
            ensure!(
                names.insert(&record.package.name),
                "package requested more than once: {}",
                record.package.name
            );
            db.preflight(record)?;
            db.installed
                .insert(record.package.name.clone(), record.clone());
        }
        resolver::validate(&db.installed)?;
        Ok(())
    })();
    db.installed = original;
    check?;
    let labels: Vec<_> = prepared
        .iter()
        .map(|package| {
            let p = &package.record.package;
            if let Some(old) = db.installed.get(&p.name) {
                format!(
                    "{}-{} -> {}-{}",
                    p.name, old.package.version, p.name, p.version
                )
            } else {
                format!("{}-{}", p.name, p.version)
            }
        })
        .collect();
    output::plan(&labels);
    if !output::confirm(noconfirm)? {
        return Ok(());
    }
    db.install_all(prepared)?;
    output::step("Installation complete.");
    Ok(())
}
