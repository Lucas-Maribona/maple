//! Metadata-only backtracking. Each branch assigns one version per real name;
//! immutable assignments make cycles finite and incompatible choices reversible.
use crate::{
    model::{Installed, Package},
    version::VersionReq,
};
use anyhow::{Result, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub fn satisfies(package: &Package, name: &str, requirement: &VersionReq) -> bool {
    (package.name == name && requirement.matches(&package.version))
        || package
            .provides
            .get(name)
            .is_some_and(|v| requirement.matches(v))
}
fn conflict(a: &Package, b: &Package) -> bool {
    a.name != b.name && a.conflicts.iter().any(|(n, r)| satisfies(b, n, r))
}
pub fn validate(packages: &BTreeMap<String, Installed>) -> Result<()> {
    for record in packages.values() {
        let p = &record.package;
        for other in packages.values() {
            ensure!(
                !conflict(p, &other.package),
                "{} conflicts with {}",
                p.name,
                other.package.name
            );
        }
        for (name, req) in &p.dependencies {
            ensure!(
                packages.values().any(|p| satisfies(&p.package, name, req)),
                "{} requires {name} {req}; the requested operation would break this dependency",
                p.name
            );
        }
    }
    Ok(())
}

/// None means choose the newest compatible repository version; Some pins the
/// exact archive version string (including SemVer build metadata).
pub struct Request {
    pub name: String,
    pub version: Option<String>,
}

// ALPM equality is not transitive when pkgrel is omitted (1 == 1-1,
// 1 == 1-2, but 1-1 < 1-2). Do not feed it to a total-order sort.
// Insert each candidate before the first strictly older candidate; retain
// input preference when none is older. Exact pins still use string equality.
fn prefer_newer(versions: &mut [Package]) {
    for i in 1..versions.len() {
        if let Some(j) = (0..i).find(|&j| versions[i].version.compare(&versions[j].version).is_gt())
        {
            versions[j..=i].rotate_right(1);
        }
    }
}

pub fn resolve(
    installed: &BTreeMap<String, Installed>,
    local: &[Package],
    requests: &[Request],
    available: &[Package],
    update: bool,
) -> Result<Vec<Package>> {
    let mut domains: BTreeMap<String, Vec<Package>> = BTreeMap::new();
    for p in available {
        domains.entry(p.name.clone()).or_default().push(p.clone());
    }
    for old in installed.values() {
        let versions = domains.entry(old.package.name.clone()).or_default();
        versions.retain(|p| p.version != old.package.version);
        versions.push(old.package.clone());
    }
    for versions in domains.values_mut() {
        prefer_newer(versions);
        if let Some(old) = installed.get(&versions[0].name) {
            if update {
                versions.retain(|p| {
                    p.version == old.package.version
                        || p.version.compare(&old.package.version).is_gt()
                });
            } else if let Some(i) = versions.iter().position(|p| p == &old.package) {
                let p = versions.remove(i);
                versions.insert(0, p);
            }
        }
    }
    let mut required = Vec::new();
    let mut explicit = BTreeSet::new();
    for request in requests {
        ensure!(
            explicit.insert(request.name.clone()),
            "package requested more than once: {}",
            request.name
        );
        let versions: Vec<_> = available
            .iter()
            .filter(|p| {
                p.name == request.name && request.version.as_ref().is_none_or(|v| *v == p.version.0)
            })
            .cloned()
            .collect();
        ensure!(
            !versions.is_empty(),
            "package not found: {}{}",
            request.name,
            request
                .version
                .as_ref()
                .map(|v| format!("={v}"))
                .unwrap_or_default()
        );
        domains.insert(request.name.clone(), versions);
        prefer_newer(domains.get_mut(&request.name).unwrap());
        required.push(request.name.clone());
    }
    for p in local {
        ensure!(
            explicit.insert(p.name.clone()),
            "package requested more than once: {}",
            p.name
        );
        domains.insert(p.name.clone(), vec![p.clone()]);
        required.push(p.name.clone());
    }
    for name in installed.keys() {
        if !required.contains(name) {
            required.push(name.clone());
        }
    }
    let mut budget = 100_000usize;
    let mut reason = String::new();
    let Some(selected) = search(
        &domains,
        &required,
        BTreeMap::new(),
        &mut budget,
        &mut reason,
    )?
    else {
        bail!("dependency conflict: no compatible solution; {reason}");
    };
    // Dependency order includes virtual providers. Cycles are installed as a
    // group; post hooks run only after every payload has been installed.
    fn visit(
        name: &str,
        selected: &BTreeMap<String, Package>,
        seen: &mut BTreeSet<String>,
        order: &mut Vec<Package>,
    ) {
        if !seen.insert(name.into()) {
            return;
        }
        let p = &selected[name];
        for (name, req) in &p.dependencies {
            if let Some(provider) = selected.values().find(|p| satisfies(p, name, req)) {
                visit(&provider.name, selected, seen, order);
            }
        }
        order.push(p.clone());
    }
    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    for name in selected.keys() {
        visit(name, &selected, &mut seen, &mut order);
    }
    order.retain(|p| {
        explicit.contains(&p.name) || installed.get(&p.name).is_none_or(|old| old.package != *p)
    });
    Ok(order)
}
fn search(
    domains: &BTreeMap<String, Vec<Package>>,
    required: &[String],
    selected: BTreeMap<String, Package>,
    budget: &mut usize,
    reason: &mut String,
) -> Result<Option<BTreeMap<String, Package>>> {
    ensure!(
        *budget > 0,
        "dependency search limit exceeded; pin versions to narrow the search"
    );
    *budget -= 1;
    let mut candidates = Vec::new();
    if let Some(name) = required.iter().find(|n| !selected.contains_key(*n)) {
        candidates.extend(domains[name].iter());
    } else {
        let missing = selected.values().find_map(|p| {
            p.dependencies
                .iter()
                .find(|(n, r)| !selected.values().any(|p| satisfies(p, n, r)))
                .map(|(n, r)| (p, n, r))
        });
        let Some((p, name, req)) = missing else {
            return Ok(Some(selected));
        };
        *reason = format!(
            "{} requires {name} {req}; dependency not found among compatible candidates",
            p.name
        );
        candidates.extend(
            domains
                .iter()
                .filter(|(n, _)| !selected.contains_key(*n))
                .flat_map(|(_, v)| v)
                .filter(|p| satisfies(p, name, req)),
        );
        candidates.sort_by_key(|p| p.name != *name); // real name before virtual providers
    }
    for p in candidates {
        if let Some(other) = selected
            .values()
            .find(|other| conflict(p, other) || conflict(other, p))
        {
            let (owner, other) = if conflict(p, other) {
                (p, other)
            } else {
                (other, p)
            };
            let (name, req) = owner
                .conflicts
                .iter()
                .find(|(n, r)| satisfies(other, n, r))
                .unwrap();
            *reason = format!(
                "{} conflicts with {name} {req} provided by {}",
                owner.name, other.name
            );
            continue;
        }
        let mut next = selected.clone();
        next.insert(p.name.clone(), p.clone());
        // Forward checking prunes constraints with no remaining provider.
        if let Some((owner, name, req)) = next.values().find_map(|p| {
            p.dependencies
                .iter()
                .find(|(n, r)| {
                    !next.values().any(|p| satisfies(p, n, r))
                        && !domains
                            .iter()
                            .filter(|(n, _)| !next.contains_key(*n))
                            .flat_map(|(_, v)| v)
                            .any(|p| satisfies(p, n, r))
                })
                .map(|(n, r)| (&p.name, n, r))
        }) {
            *reason = format!(
                "{owner} requires {name} {req}; dependency not found among compatible candidates"
            );
            continue;
        }
        if let Some(solution) = search(domains, required, next, budget, reason)? {
            return Ok(Some(solution));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(name: &str, version: &str, extra: &str) -> Package {
        let p: Package =
            toml::from_str(&format!("name = {name:?}\nversion = {version:?}\n{extra}")).unwrap();
        p.validate().unwrap();
        p
    }
    fn req(name: &str) -> Request {
        Request {
            name: name.into(),
            version: None,
        }
    }
    fn installed(p: Package) -> Installed {
        Installed {
            package: p,
            files: vec![],
            directories: vec![],
            config_defaults: BTreeMap::new(),
            hook_scripts: BTreeMap::new(),
        }
    }
    #[test]
    fn backtracks_versions_and_virtual_providers_with_cycles() {
        let repo = vec![
            p(
                "app",
                "2.0.0",
                "dependencies = { api = \"^2\", loop = \"*\" }",
            ),
            p("app", "1.0.0", "dependencies = { api = \"^1\" }"),
            p(
                "a-provider",
                "1.0.0",
                "provides = { api = \"2.0.0\" }\nconflicts = { loop = \"*\" }",
            ),
            p("b-provider", "1.0.0", "provides = { api = \"2.0.0\" }"),
            p("loop", "1.0.0", "dependencies = { app = \"^2\" }"),
        ];
        let plan = resolve(&BTreeMap::new(), &[], &[req("app")], &repo, false).unwrap();
        assert!(plan.iter().any(|p| p.name == "b-provider"));
        assert!(!plan.iter().any(|p| p.name == "a-provider"));
        let state = plan
            .into_iter()
            .map(|p| (p.name.clone(), installed(p)))
            .collect();
        validate(&state).unwrap();
    }
    #[test]
    fn backtracks_installed_reverse_dependencies_and_preserves_update_floor() {
        let a = p("app", "1.0.0", "dependencies = { lib = \"^1\" }");
        let b = p("lib", "1.0.0", "");
        let old = [
            (a.name.clone(), installed(a.clone())),
            (b.name.clone(), installed(b.clone())),
        ]
        .into_iter()
        .collect();
        let newer = p("lib", "2.0.0", "");
        let repo = vec![
            a,
            b,
            newer.clone(),
            p("app", "2.0.0", "dependencies = { lib = \"^2\" }"),
        ];
        let plan = resolve(&old, std::slice::from_ref(&newer), &[], &repo, false).unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|p| p.version.0 == "2.0.0"));
        let pinned = p("app", "1.0.0", "dependencies = { lib = \"^1\" }");
        assert!(resolve(&old, &[newer, pinned], &[], &repo, false).is_err());
    }
    #[test]
    fn unversioned_providers_cannot_satisfy_versioned_requirements() {
        let provider = p("provider", "1.0.0", "provides = { api = \"*\" }");
        assert!(satisfies(&provider, "api", &VersionReq::any()));
        assert!(!satisfies(
            &provider,
            "api",
            &VersionReq::try_from(">=1".to_owned()).unwrap()
        ));
    }
    #[test]
    fn solves_all_small_version_constraint_combinations() {
        // Compare the search result with exhaustive enumeration, including a
        // cycle, crossed constraints, and conflicts in both directions.
        for mask in 0..64 {
            let mut repo = Vec::new();
            for (index, name) in ["a", "b", "c"].iter().enumerate() {
                for v in 1..=2 {
                    let target = ["b", "c", "a"][index];
                    let bound = if mask & (1 << (index * 2 + v - 1)) == 0 {
                        1
                    } else {
                        2
                    };
                    repo.push(p(
                        name,
                        &format!("{v}.0.0"),
                        &format!("dependencies = {{ {target} = \"={bound}.0.0\" }}"),
                    ));
                }
            }
            let possible = (0..8).any(|choice| {
                let state = (0..3)
                    .map(|i| {
                        let p = repo[i * 2 + ((choice >> i) & 1)].clone();
                        (p.name.clone(), installed(p))
                    })
                    .collect();
                validate(&state).is_ok()
            });
            let result = resolve(&BTreeMap::new(), &[], &[req("a")], &repo, false);
            assert_eq!(result.is_ok(), possible, "mask {mask}");
            if let Ok(plan) = result {
                validate(
                    &plan
                        .into_iter()
                        .map(|p| (p.name.clone(), installed(p)))
                        .collect(),
                )
                .unwrap();
            }
        }
    }
}
