//! One ALPM comparator for package versions, provisions, and constraint bounds.
//! Versions remain opaque strings; no format detection or SemVer ordering.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, fmt};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Version(pub String);
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Version {
    pub fn validate(&self) -> Result<()> {
        validate_text(&self.0)
    }
    pub fn compare(&self, other: &Self) -> Ordering {
        compare(&self.0, &other.0)
    }
}

fn validate_text(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty()
            && s.len() <= 200
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_graphic() && !b"/\\<> =,*^|?\"".contains(&b)),
        "invalid version: {s}"
    );
    Ok(())
}

/// The original expression is retained in metadata; only explicit legacy
/// constraint syntax is expanded. Candidate versions always use ALPM.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct VersionReq(String);
impl From<VersionReq> for String {
    fn from(r: VersionReq) -> Self {
        r.0
    }
}
impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl TryFrom<String> for VersionReq {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        clauses(&s)?;
        Ok(Self(s))
    }
}
impl VersionReq {
    pub fn any() -> Self {
        Self("*".into())
    }
    pub fn matches(&self, v: &Version) -> bool {
        // Parsing was validated at construction, including through serde.
        let bounds = clauses(&self.0).expect("validated constraint");
        if v.0 == "*" {
            return bounds.is_empty();
        } // unversioned provision
        bounds.iter().all(|(op, bound)| {
            let c = compare(&v.0, bound);
            match *op {
                "=" => c.is_eq(),
                ">" => c.is_gt(),
                ">=" => !c.is_lt(),
                "<" => c.is_lt(),
                "<=" => !c.is_gt(),
                _ => unreachable!(),
            }
        })
    }
}
fn clauses(expression: &str) -> Result<Vec<(&'static str, String)>> {
    // Earlier converters emitted this prefix. It is now just a compatibility
    // alias: both prefixed and plain comparisons use the same algorithm.
    let expression = expression
        .strip_prefix("arch:")
        .unwrap_or(expression)
        .trim();
    let mut result = Vec::new();
    for part in expression.split(',').map(str::trim) {
        if part == "*" {
            continue;
        }
        if !part.starts_with(['=', '>', '<'])
            && (part.starts_with(['^', '~'])
                || part.contains('*')
                || part.split('.').any(|p| matches!(p, "x" | "X")))
        {
            result.extend(legacy_range(part)?);
        } else {
            let op = [">=", "<=", "=", ">", "<"]
                .into_iter()
                .find(|op| part.starts_with(op))
                .unwrap_or("");
            let bound = part[op.len()..].trim();
            validate_text(bound)?;
            result.push((if op.is_empty() { "=" } else { op }, bound.to_owned()));
        }
    }
    Ok(result)
}
// semver is only a parser for explicitly requested range shorthand. Neither
// SemVer Version parsing nor VersionReq::matches participates in comparison.
fn legacy_range(text: &str) -> Result<Vec<(&'static str, String)>> {
    use semver::Op;
    let req = semver::VersionReq::parse(text)
        .context("invalid legacy range; use explicit ALPM bounds such as >=1.2.0, <2.0.0")?;
    if req.comparators.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        req.comparators.len() == 1,
        "use a comma between constraint clauses"
    );
    let c = &req.comparators[0];
    ensure!(
        c.pre.is_empty(),
        "legacy prerelease ranges require migration to explicit ALPM bounds"
    );
    let lower = [c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0)];
    let bump = match c.op {
        Op::Caret if c.major != 0 || c.minor.is_none() => 0,
        Op::Caret if c.minor != Some(0) || c.patch.is_none() => 1,
        Op::Caret => 2,
        Op::Tilde | Op::Wildcard if c.minor.is_none() => 0,
        Op::Tilde | Op::Wildcard => 1,
        _ => anyhow::bail!("unsupported legacy range; use explicit ALPM bounds"),
    };
    let mut upper = lower;
    upper[bump] = upper[bump]
        .checked_add(1)
        .context("legacy range bound overflows; use explicit ALPM bounds")?;
    upper[bump + 1..].fill(0);
    let format = |v: [u64; 3]| format!("{}.{}.{}", v[0], v[1], v[2]);
    Ok(vec![(">=", format(lower)), ("<", format(upper))])
}

fn evr(s: &str) -> (&str, &str, Option<&str>) {
    let (epoch, rest) = s
        .split_once(':')
        .filter(|(e, _)| e.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(("0", s));
    let (version, release) = rest
        .rsplit_once('-')
        .map_or((rest, None), |(v, r)| (v, Some(r)));
    (if epoch.is_empty() { "0" } else { epoch }, version, release)
}
pub fn compare(a: &str, b: &str) -> Ordering {
    let (ae, av, ar) = evr(a);
    let (be, bv, br) = evr(b);
    segments(ae, be)
        .then_with(|| segments(av, bv))
        .then_with(|| match (ar, br) {
            (Some(a), Some(b)) => segments(a, b),
            _ => Ordering::Equal,
        })
}
// ALPM ordering: numeric runs compare by magnitude, alpha runs by bytes;
// separator lengths and trailing alpha suffixes have special precedence.
fn segments(mut a: &str, mut b: &str) -> Ordering {
    while !a.is_empty() && !b.is_empty() {
        let aa = a.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let bb = b.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let separators = (a.len() - aa.len()).cmp(&(b.len() - bb.len()));
        a = aa;
        b = bb;
        if a.is_empty() || b.is_empty() {
            break;
        }
        if !separators.is_eq() {
            return separators;
        }
        let numeric = a.as_bytes()[0].is_ascii_digit();
        let end = |s: &str| {
            s.bytes()
                .take_while(|c| {
                    if numeric {
                        c.is_ascii_digit()
                    } else {
                        c.is_ascii_alphabetic()
                    }
                })
                .count()
        };
        let an = end(a);
        let bn = end(b);
        if bn == 0 {
            return if numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        let (mut ax, mut bx) = (&a[..an], &b[..bn]);
        if numeric {
            ax = ax.trim_start_matches('0');
            bx = bx.trim_start_matches('0');
        }
        let c = if numeric {
            ax.len().cmp(&bx.len()).then_with(|| ax.cmp(bx))
        } else {
            ax.cmp(bx)
        };
        if !c.is_eq() {
            return c;
        }
        a = &a[an..];
        b = &b[bn..];
    }
    if a.is_empty() && b.is_empty() {
        Ordering::Equal
    } else if (a.is_empty() && !b.as_bytes()[0].is_ascii_alphabetic())
        || a.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
    {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn matches(req: &str, version: &str) -> bool {
        VersionReq::try_from(req.to_owned())
            .unwrap()
            .matches(&Version(version.into()))
    }
    #[test]
    fn pacman_vercmp_corpus() {
        let mut count = 0;
        for line in include_str!("../tests/fixtures/pacman-vercmptest.sh").lines() {
            let Some(case) = line.strip_prefix("tap_runtest ") else {
                continue;
            };
            let args: Vec<_> = case.split_whitespace().collect();
            assert_eq!(args.len(), 3);
            let expected = args[2].parse::<i32>().unwrap().cmp(&0);
            assert_eq!(
                compare(args[0], args[1]),
                expected,
                "{} vs {}",
                args[0],
                args[1]
            );
            assert_eq!(
                compare(args[1], args[0]),
                expected.reverse(),
                "{} vs {}",
                args[1],
                args[0]
            );
            count += 2;
        }
        assert_eq!(count, 92, "all upstream cases and their reversals must run");
    }
    #[test]
    fn matches_system_vercmp_when_available() {
        use std::process::Command;
        match Command::new("vercmp").args(["1", "1"]).output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    std::env::var_os("MAPLE_REQUIRE_VERCMP_TESTS").is_none(),
                    "vercmp is required"
                );
                eprintln!(
                    "SKIP: vercmp not installed; set MAPLE_REQUIRE_VERCMP_TESTS=1 to require it"
                );
                return;
            }
            result => assert!(result.unwrap().status.success()),
        }
        // Cartesian comparisons include ambiguous pkgrel, non-SemVer strings,
        // long numeric runs, separator lengths, and SemVer-looking versions.
        let versions = [
            "1",
            "1-1",
            "1-2",
            "1.0",
            "1.0.0",
            "1.0.0-rc.1",
            "1.0.0+build.2",
            "1.0alpha",
            "1.0.a",
            "1.0~rc1",
            "01.002",
            "1..0",
            "2_0",
            "2___a",
            "2_a",
            "2:0.1-9",
            "0:1.0",
            "99999999999999999999999999999999999:1-1",
            "v2026r10",
            "v2026r9",
            "1.99999999999999999999999999999999999",
            "1.00000000000000000000000000000000001",
        ];
        for a in versions {
            for b in versions {
                let output = Command::new("vercmp").args([a, b]).output().unwrap();
                assert!(output.status.success());
                let expected = std::str::from_utf8(&output.stdout)
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap()
                    .cmp(&0);
                assert_eq!(compare(a, b), expected, "{a} vs {b}");
            }
        }
    }
    #[test]
    fn alpm_constraints_and_unversioned_provisions() {
        for (req, v, expected) in [
            ("=1.0", "1.0-9", true),
            ("1.0", "1.0.0", false),
            ("=1.0-1", "1.0-2", false),
            (">=2:1-1, <3:0", "2:4-2", true),
            ("arch:>=2:1-1, <3:0", "2:4-2", true),
            ("*", "1.0.0-rc.1", true),
            ("=1.0.0", "1.0.0-rc.1", true),
            ("<1.0.0", "1.0.0-rc.1", false),
            ("<1.0.0", "1.0.0rc1", true),
            (">1.0.0", "1.0.0+build.1", true),
            ("=v2026r10", "v2026r9", false),
            ("=1.x", "1.x", true),
            ("=1.x", "1.2", false),
            ("*", "*", true),
            (">=1", "*", false),
            ("arch:*", "*", true),
        ] {
            assert_eq!(matches(req, v), expected, "{req} matching {v}");
        }
    }
    #[test]
    fn legacy_ranges_expand_to_alpm_bounds_without_parsing_candidate_versions() {
        for (range, bounds) in [
            ("^1.2", ">=1.2.0, <2.0.0"),
            ("^0", ">=0.0.0, <1.0.0"),
            ("^0.0", ">=0.0.0, <0.1.0"),
            ("^0.2.3", ">=0.2.3, <0.3.0"),
            ("^0.0.3", ">=0.0.3, <0.0.4"),
            ("~1", ">=1.0.0, <2.0.0"),
            ("~1.2", ">=1.2.0, <1.3.0"),
            ("~1.2.3", ">=1.2.3, <1.3.0"),
            ("1.*", ">=1.0.0, <2.0.0"),
            ("1.2.*", ">=1.2.0, <1.3.0"),
            ("1.2.x", ">=1.2.0, <1.3.0"),
            ("^1, <1.5", ">=1.0.0, <2.0.0, <1.5"),
        ] {
            assert_eq!(clauses(range).unwrap(), clauses(bounds).unwrap());
            for v in [
                "0.0.3",
                "0.2.9",
                "1.0.0",
                "1.2.3",
                "1.2.4-5",
                "1.2.9custom",
                "1.2.4+build",
                "1.3.0",
                "1.5",
                "2.0.0",
                "1:0",
            ] {
                assert_eq!(matches(range, v), matches(bounds, v), "{range}: {v}");
            }
        }
        assert!(matches("^1.2", "1.3.0-rc.1")); // ALPM, not SemVer prerelease exclusion
    }
    #[test]
    fn reject_invalid_input_and_legacy_ranges_that_need_manual_migration() {
        for req in [
            "",
            ">=1 || <3",
            "^1.0.0-rc.1",
            "~1.2.0-beta",
            "^18446744073709551615",
            "~0.18446744073709551615",
            "1.*,",
            "==1",
            "arch:",
        ] {
            assert!(VersionReq::try_from(req.to_owned()).is_err(), "{req}");
        }
        for v in ["../bad", "a/b", "..", "", "1\n2", "1 2", "1\\2", "*"] {
            assert!(Version(v.into()).validate().is_err(), "{v}");
        }
    }
    #[test]
    fn strings_round_trip_without_normalization() {
        for value in [
            "01.002",
            "0:1.0-01",
            "1.0.0-rc.1+build.9",
            "v2026r10",
            "2:1.2-3",
        ] {
            let text =
                format!("name = \"test\"\nversion = {value:?}\nprovides = {{ api = {value:?} }}\n");
            let package: crate::model::Package = toml::from_str(&text).unwrap();
            package.validate().unwrap();
            let encoded = toml::to_string(&package).unwrap();
            let decoded: crate::model::Package = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.version.0, value);
            assert_eq!(decoded.provides["api"].0, value);
        }
    }
}
