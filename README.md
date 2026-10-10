# maple

Mercury Linux’s package manager, written in Rust. maple installs `.maple` archives into a live system or an alternate filesystem root, resolves dependencies, and journals package changes for recovery and rollback.

## Features

- **Multi-version repositories:** retain older releases, install exact versions, upgrade, reinstall, or downgrade. Each root has one installed version per package name.
- **One version comparator:** native and Arch-converted packages use ALPM-compatible ordering, including `epoch:pkgver-pkgrel`. Original version strings are preserved.
- **Dependency planning:** recursive dependencies, versioned virtual providers, conflicts, cycles, reverse-dependency checks, and backtracking across available versions.
- **Recoverable transactions:** compressed backups, durable journals, automatic recovery of interrupted operations, and configurable rollback history.
- **Filesystem preservation:** permissions, numeric ownership, symlinks, hard links, extended attributes, POSIX ACLs, and Linux capabilities. Declared configuration files preserve local changes.
- **Lifecycle integration:** fixed post-transaction maintenance triggers and optional, explicitly authorized package scripts. Offline execution uses the target’s tools inside Bubblewrap.
- **Arch conversion:** a separate `arch-to-maple` executable translates `.pkg.tar.zst` archives and reports unsupported features.

## Contents

- [Getting started](#getting-started)
- [Commands and configuration](#commands-and-configuration)
- [Package format](#package-format)
- [Versions and dependency resolution](#versions-and-dependency-resolution)
- [Repositories](#repositories)
- [Lifecycle scripts and maintenance](#lifecycle-scripts-and-maintenance)
- [Arch package conversion](#arch-package-conversion)
- [Under the hood](#under-the-hood)
- [Limitations](#limitations)
- [Development](#development)

## Getting started

Build on Linux with Rust **1.89 or newer**, Cargo, a C compiler, and native build tools such as `make`:

```sh
cargo build --release --locked --bins
target/release/maple --help
target/release/arch-to-maple --help
```

The binaries are in `target/release/`. Archive handling runs in-process; bundled xz and zstd libraries avoid runtime dependencies on external `tar`, `xz`, or `zstd` commands. The examples below use external `tar` and `xz` to create a package.

### Try a local package

From the project directory:

```sh
mkdir -p target/hello/payload/usr/bin
cp examples/metadata.toml target/hello/metadata.toml
printf '#!/bin/sh\necho "Hello from maple!"\n' > target/hello/payload/usr/bin/hello
chmod +x target/hello/payload/usr/bin/hello
tar -cJf target/hello-1.0.0.maple -C target/hello metadata.toml payload

demo_root=$(mktemp -d)
target/release/maple --root "$demo_root" install ./target/hello-1.0.0.maple --noconfirm
"$demo_root/usr/bin/hello"
target/release/maple --root "$demo_root" remove hello --noconfirm
```

This example needs no root privileges, repository, or Bubblewrap because the package has no dependencies, scripts, or maintenance triggers. Running the installed `hello` here uses the host’s shell; `--root` directs package management into the directory, not arbitrary commands you run afterward.

## Commands and configuration

| Command | Behavior |
| --- | --- |
| `maple search [query]` | List repository versions, optionally filtering names and descriptions. |
| `sudo maple install hello` | Select a compatible version, preferring newer releases. |
| `sudo maple install hello=1.0.0` | Select that exact version string, including a reinstall or downgrade. |
| `sudo maple install ./hello-1.0.0.maple` | Install a local archive; resolve missing dependencies from the repository when needed. |
| `sudo maple remove hello` | Remove tracked files while preserving modified declared configs. Reject broken dependencies. |
| `sudo maple update` | Find compatible upgrades for installed packages without downgrading them. |
| `sudo maple rollback` | Restore the latest retained transaction’s files and package records. |
| `sudo maple recover` | Undo an interrupted file transaction and retry pending maintenance. |

`install` and `remove` accept multiple packages. Installation, removal, and manual rollback show a plan and ask for confirmation: Enter or `y` accepts; `n` or closed input cancels. `update` uses the same installation confirmation when changes are needed. maple itself receives updates only when registered as an installed package in the selected root.

| Global option | Behavior |
| --- | --- |
| `--root PATH` | Target another filesystem root; defaults to the live system `/`. |
| `--repo URL` | Override the repository index URL. |
| `--noconfirm` | Accept the operation without prompting. |
| `--trust-hooks` | Authorize lifecycle scripts for all selected packages, including dependencies, for this operation. |

Managing `/` normally requires administrator privileges; searching does not. `NO_COLOR` or `TERM=dumb` disables colors. Redirected output uses plain progress lines instead of terminal animation.

Configuration lives at `/etc/maple/config.toml` **inside the selected root**. Both settings are optional:

```toml
repository = "https://raw.githubusercontent.com/Lucas-Maribona/mcxpkgs/main/repository.toml"
rollback_retention = 3
```

The URL shown is the default repository. `--repo` overrides it. Rollback retention defaults to **1**, with an allowed range of **1–100**. Only one repository index is used per operation.

## Package format

A `.maple` file is a **tar.xz** archive:

```text
metadata.toml
payload/
  usr/bin/hello
  etc/hello.conf
hooks/                 # Optional; only declared scripts belong here
  post-install
```

Payload paths are relative to the target root: `payload/usr/bin/hello` becomes `/usr/bin/hello`. Metadata requires only `name` and `version`:

```toml
name = "hello"
version = "1.0.0"
description = "A simple hello-world program"
```

Optional fields belong before table headings; each dependency-related table maps a package or virtual name to a string:

```toml
config_files = ["etc/hello.conf"]
triggers = ["ldconfig"]
hooks = ["post-install"]

[dependencies]
shell = ">=1.2, <2"

[provides]
greeting-service = "1.0"

[optional_dependencies]
translations = "*"

[conflicts]
other-hello = "*"
```

Add only declarations appropriate to the package. Every declared config must exist in the payload, and every declared hook must have a matching script. Optional dependencies are informational; maple does not install or require them. `provides` values are versions, or `"*"` for an unversioned provision—not version constraints. See [example metadata](examples/metadata.toml).

Names start with an ASCII letter or digit and otherwise contain only letters, digits, `.`, `_`, `+`, or `-`. Versions are nonempty strings of at most 200 printable ASCII bytes, excluding whitespace, path separators, traversal names, and reserved constraint delimiters. Unknown metadata fields are rejected.

### Building an archive

Write the output outside the prepared package directory:

```sh
tar -cJf hello-1.0.0.maple -C path/to/package metadata.toml payload
```

Add `hooks` to that command when scripts are present. `./metadata.toml` and `./payload/` archive paths also work. For GNU tar packages carrying ACLs or capabilities, preserve numeric identities and PAX attributes:

```sh
tar --format=pax --numeric-owner --acls --xattrs --xattrs-include='*' \
    -cJf hello-1.0.0.maple -C path/to/package metadata.toml payload
```

### Files and attributes

maple supports regular files, directories, symlinks, hard links, FIFOs, and character/block devices. Numeric ownership is restored when running as root; otherwise files belong to the installing user. Device creation and privileged attributes require suitable privileges and filesystem support.

Paths must be UTF-8. Hard links must refer to regular files within the payload. Symlinks may have absolute or dangling targets, but installation never follows directory symlinks. For systems with `/bin -> /usr/bin`, package `usr/bin/tool` directly. Existing unowned files and files owned by another package are not overwritten. File-to-directory or directory-to-file changes require removing the old package first.

Permissions, xattrs, numeric POSIX access/default ACLs, and `security.capability` are preserved through installation and backups. Ownership and modes are applied before attributes; an attribute failure fails the transaction. Supported PAX encodings include `SCHILY.xattr.*`, `SCHILY.acl.access`, `SCHILY.acl.default`, and base64 `LIBARCHIVE.xattr.*`. Percent-encoded xattr names are rejected. Metadata and individual extended headers are limited to 16 MiB. Timestamps have one-second precision; Unix sockets are unsupported.

### Configuration protection

Declare root-relative regular files in `config_files`; they cannot be hard-linked. maple stores their original default bytes in the installed record.

| Current config state | Upgrade behavior |
| --- | --- |
| Matches the recorded default | Replace with the new default. |
| Edited, deleted, or replaced by a symlink | Preserve that state; write the incoming default to `<path>.maple-new`. |
| No recorded baseline | Treat conservatively as modified. |

Generated sidecars are tracked. An unchanged sidecar can be replaced; an edited or unowned sidecar causes an error instead of being overwritten. Merge and remove it before retrying. There is no automatic merge.

Removal and upgrades that drop a config delete unchanged defaults and sidecars but leave modified ones unowned. A package cannot retain a config payload path while dropping its protection declaration. Undeclared files follow normal replace/remove rules. Rollback restores configs, sidecars, and baselines from the transaction snapshot, overwriting edits made afterward.

## Versions and dependency resolution

### ALPM version rules

**All packages use the same comparator.** maple preserves version strings and uses ALPM ordering for candidate selection, dependency bounds, conflicts, provisions, and upgrades. It never guesses a version format or sorts versions lexicographically. Numeric components compare by magnitude without integer overflow; epochs, release numbers, alphabetic segments, and separators follow pacman `vercmp` behavior.

```toml
name = "example"
version = "2:1.10.0-3"

[dependencies]
libexample = ">=2:1.9-1, <3:0"
```

Constraints accept `=`, `<`, `<=`, `>`, `>=`, comma-separated AND conditions, a bare version meaning equality, and `*` meaning unconstrained. The old converter’s `arch:` prefix remains a compatibility alias; new conversions omit it.

ALPM compares pkgrel only when **both** versions contain it. Thus `1.0` compares equal to either `1.0-1` or `1.0-2`, while `1.0-1 < 1.0-2`. This is not a total order. maple orders candidates by inserting each before the first strictly older candidate, without a lexical tie-break; repository order can matter for equal versions. Keep release-number usage consistent within a package’s history.

CLI pins use **exact string identity**: `install example=1.0-1` selects that entry even if another string compares equal. Pins apply to that operation and are not saved as a future update policy. `update` skips ALPM-equal versions; explicitly install to switch between them.

### Compatibility with older packages

Packages without newer optional fields remain supported. The retired `version_scheme` field is rejected: remove it from repository entries, archive metadata, and installed records, preserving the version strings. Repack changed archives and regenerate checksums. Finish interrupted transactions with the old binary first. Older rollback snapshots may also contain the field; retain the old binary to restore that history, then migrate restored records before using the new binary. maple does not rewrite these files automatically.

Explicit legacy SemVer shorthand is translated into ALPM bounds:

| Constraint | Expanded bounds |
| --- | --- |
| `^1.2` | `>=1.2.0, <2.0.0` |
| `^0.2.3` | `>=0.2.3, <0.3.0` |
| `^0.0.3` | `>=0.0.3, <0.0.4` |
| `~1.2.3` | `>=1.2.3, <1.3.0` |
| `1.*` / `1.x` | `>=1.0.0, <2.0.0` |
| `1.2.*` / `1.2.x` | `>=1.2.0, <1.3.0` |

The `semver` crate parses only this explicit constraint syntax; candidate versions always use ALPM. Caret/tilde prerelease bounds and overflowing generated bounds are rejected with migration diagnostics. Use explicit comparisons instead. `=1.x` is a literal bound, not a wildcard.

Review old implicit SemVer ranges: bare `1.2.3` now means equality, not `^1.2.3`. Partial comparisons are literal: `=1.2` is not `1.2.*`, and `>1` accepts `1.0`. Update constraints consistently in archive metadata, repository entries, and installed records.

Prerelease ordering necessarily differs from SemVer:

- `1.0.0-rc.1` compares equal to `1.0.0`: the hyphen introduces pkgrel, which the latter omits.
- `1.0.0rc1` is older than `1.0.0`, while `1.0.0.rc1` is newer.
- `+build` affects ordering. There is no automatic exclusion of prereleases from ranges.

The comparator is tested against the [vendored pacman corpus](tests/fixtures/README.md) and optionally the real `vercmp` executable.

### How resolution works

maple plans a compatible final package set **before downloading repository archives**. Explicit versions and supplied local archives are fixed. Unpinned requests prefer newer versions; ordinary installs reuse installed dependencies where possible. Updates consider newer compatible versions without downgrading installed packages, and warn about missing packages or held-back upgrades.

The resolver follows dependencies recursively and backtracks across versions and providers when constraints or conflicts fail. Versioned provisions satisfy requirements using the provision’s version; an unversioned `"*"` provision only satisfies unconstrained requirements. Missing dependencies prefer a real package, then virtual providers by name. Conflicts are checked in both directions, including virtual names.

All installed package names remain in the solution. Reverse dependencies can require additional package changes, which appear in the plan. maple never automatically removes packages to solve conflicts. Cycles share a transaction; dependency-first ordering is used where possible, but cycles have no strict ordering. Removing a required package fails unless the final state remains valid—for example, by removing its dependents in the same command.

An unsatisfiable plan or the 100,000-branch search limit produces an error. Filesystem ownership conflicts are checked after archive preparation and are not inputs to dependency backtracking.

## Repositories

Serve an HTTP(S) index and archives with this layout:

```text
repository.toml
packages/
  hello/
    1.0.0.maple
    1.1.0.maple
```

Use one entry per exact name/version pair, retaining old entries for downgrades and dependency fallback:

```toml
[[package]]
name = "hello"
version = "1.0.0"
description = "A simple hello-world program"

[[package]]
name = "hello"
version = "1.1.0"
description = "A simple hello-world program"

[sha256]
# Populate with actual 64-digit hexadecimal digests:
# "hello/1.0.0" = "..."
# "hello/1.1.0" = "..."
```

Mirror dependency, provision, optional-dependency, conflict, config, trigger, and hook declarations from the archive. Repository tables use the `package.` prefix, such as `[package.dependencies]`, beneath the relevant entry. Downloaded archive metadata must match these declarations and the exact name/version. See [the example index](examples/repository.toml). An empty index can contain `package = []`.

Archive URLs are derived from the final index URL after redirects as `packages/<name>/<version>.maple`, with safely encoded path segments. There is no per-package URL field. Older flat repository layouts must be migrated.

The optional top-level `[sha256]` table is keyed by `name/version`. Supplied hashes are checked before archive preparation; indexes without hashes remain supported. Hashes from the same index detect corruption, **not publisher authenticity**. Use trusted packages and repositories; signing and key management are not implemented.

## Lifecycle scripts and maintenance

### Trusted package scripts

Declare any of `pre-install`, `post-install`, `pre-upgrade`, `post-upgrade`, `pre-remove`, or `post-remove` in `hooks`, with a matching regular UTF-8 file at `hooks/<name>`. Declarations and archive scripts must match exactly. Scripts need not have executable mode; they use `/bin/sh` by default, or an exact `#!/bin/sh` / `#!/bin/bash` shebang. Other interpreters, NUL bytes, and scripts over 64 KiB are rejected.

Installing or removing packages declaring scripts requires **`--trust-hooks`**. It authorizes every selected package, including dependencies, for that operation. `--noconfirm` does not grant trust, and neither package names nor repository URLs imply trust.

| Phase | Timing and arguments |
| --- | --- |
| Install | All `pre-install` scripts run before payload changes; all `post-install` scripts run after payloads and records change. `$1` is the version. |
| Upgrade, reinstall, downgrade | Use the incoming package’s `pre-upgrade` / `post-upgrade`. `$1` is the new version; `$2` is the old version. |
| Remove | Use scripts retained in the installed record. `$1` is the removed version. |

The rollback snapshot precedes all pre-hooks; durable commit follows all post-hooks. Pre-hooks cannot depend on newly installed payloads, even from dependencies. Post-hooks see the complete selected payload set. Scripts receive `MAPLE_PACKAGE`, fixed `PATH`, `LANG=C`, and `SYSTEMD_OFFLINE`; other environment variables are cleared and stdin is closed.

A failed hook aborts the transaction and restores tracked files and records. Recovery and rollback **never replay lifecycle scripts**. Arbitrary script effects—accounts, services, untracked files, or external resources—cannot be undone automatically. Scripts must tolerate partial execution; prefer declarative triggers for derived system state.

### Declarative maintenance triggers

Existing declarative triggers need no `--trust-hooks` flag and cannot specify custom commands or arguments. Triggers from incoming and outgoing packages are deduplicated and executed **after commit**, in this fixed order:

| Trigger | Live-root command | Alternate-root behavior |
| --- | --- | --- |
| `systemd-sysusers` | `/usr/bin/systemd-sysusers` | Run the target utility to create accounts. |
| `systemd-tmpfiles` | `/usr/bin/systemd-tmpfiles --create` | Add `--boot`; run after account creation. |
| `ldconfig` | `/sbin/ldconfig` | Rebuild the target cache. |
| `initramfs` | `/usr/bin/dracut --regenerate-all --force` | Generate a generic image for each installed target kernel. |
| `systemd` | `/usr/bin/systemctl daemon-reload` | Skip reload; the target manager reads units at boot. |

Completion is recorded durably after each trigger. A crash between execution and recording may cause a retry, so maintenance must be idempotent.

**Maintenance failure leaves package files committed and returns an error.** Run `maple recover` to retry or `maple rollback` to restore package files. Pending maintenance does not block a repair installation; successful modifying transactions merge and retry the queue. An otherwise up-to-date `update` also retries it. Rollback can restore files successfully yet return an error if subsequent maintenance fails.

### Running against an alternate root

With the default root `/`, scripts and maintenance intentionally run on the live system. For another root, maple uses the host’s **`/usr/bin/bwrap`** and the **target’s** shell and utilities. There is no host-shell or unsandboxed fallback. Install Bubblewrap on the host and ensure the target already contains the shell, libraries, and tools needed by pre-hooks.

The sandbox isolates mount, PID, network, IPC, and UTS namespaces; uses a private `/dev`, read-only `/proc`, empty `/sys`, temporary `/run` and `/tmp`; restricts capabilities; and makes maple’s database read-only. Runtime mount points must be real directories. Filesystems mounted below the target are rejected if the same device is also mounted outside it. Dedicated target partitions such as `/boot` are supported when mounted only inside the target. Use an inactive, trusted tree and do not change its mounts concurrently.

Offline initramfs generation enumerates target `/usr/lib/modules` directories and runs target dracut with `--force --no-hostonly --no-hostonly-cmdline --kver VERSION`. It never selects the host’s running kernel. Mount the target’s boot partitions first. Missing kernels, tools, namespace support, or failed commands produce errors; sandbox startup failures leave maintenance pending. Target filesystems are flushed after successful offline maintenance.

Hardware-dependent dracut modules, network access, and writable kernel interfaces are unavailable. Offline images are generic, and sandbox `/run` and `/tmp` contents are temporary. Accounts, caches, and generated images are derived state: rollback queues maintenance again but does not automatically remove everything previously generated.

## Arch package conversion

`arch-to-maple` converts a package without executing its scripts or extracting its payload onto the build host:

```sh
mkdir -p repository/packages/example
target/release/arch-to-maple example.pkg.tar.zst \
  'repository/packages/example/1:2.0-1.maple' > entry.toml
```

Choose the output name to match the original `.PKGINFO` version. The destination must not exist. The converter stages decompressed tar data, streams a new xz archive, and validates it through maple’s package loader before publishing it. Standard output contains a repository entry and SHA-256 table; merge entries into your index and combine checksum keys into **one** `[sha256]` table. Notices go to stderr.

| Input feature | Translation or limitation |
| --- | --- |
| Name, version, description | Preserved as maple metadata; versions use the same ALPM rules as native packages. |
| Runtime/optional dependencies, provides, conflicts | Translated to corresponding tables; optional-dependency descriptions are omitted. |
| Backup files | Become `config_files`. |
| Files, modes, numeric ownership, symlinks, hard links, FIFOs/devices | Carried into the payload, subject to maple’s validation and installation privileges. |
| Supported PAX xattrs, ACLs, capabilities | Preserved; fractional mtime is truncated and atime/ctime loss is reported. |
| `.INSTALL` | Wrapped in six Bash lifecycle adapters; adds a `bash` dependency and requires trust at installation. |
| Architecture, license, groups, build dependencies, provenance | Reported as omitted; architecture is not enforced. |
| `.BUILDINFO`, `.MTREE` | Reported and omitted; original `.MTREE` hashes are not verified. |
| `replaces`, unknown operational fields, repeated constraints for a name, ALPM transaction hook files, unsupported PAX features | Rejected with a diagnostic; no converted archive is published. |

Each Bash adapter includes the original `.INSTALL` body and calls its matching function when present, using Arch’s version arguments. Top-level code therefore runs at every phase, even if that function is absent; the converter reports this. `.INSTALL` is limited to 32 KiB. Review distribution assumptions, pacman calls, and service/network requirements. Bash and libraries required by pre-hooks must already exist in the target even though the converter adds a dependency.

Conversion changes package format, not distribution ABI compatibility. Use architecture-specific repositories and review every conversion notice. Decompression, tar checksums, structural validation, and output SHA-256 provide integrity checks, not authentication of the source package.

## Under the hood

### Installation flow

1. **Open and recover.** Canonicalize the target root, acquire the database lock, and restore any interrupted file transaction before a modifying command proceeds.
2. **Plan.** Read installed records and local metadata; fetch the repository index if needed. Resolve a compatible final set using metadata, then download only selected archives.
3. **Validate.** Verify supplied hashes and index/archive agreement. Expand each xz archive once into temporary storage; validate paths, members, attributes, config declarations, hooks, and hard-link targets. Reject traversal, duplicates, maple database paths, ownership conflicts, and unsupported file-type changes before mutation.
4. **Confirm and snapshot.** Check hook authorization and final dependencies, confirm the plan, and check available disk space. Back up affected files and installed records, then persist the active journal.
5. **Apply.** Run all pre-hooks, change payloads and records, remove obsolete tracked paths, and run all post-hooks. Remove directories only when empty and unshared. A pre-commit failure restores the snapshot.
6. **Commit and maintain.** Synchronize changed data and atomically rename the active journal to mark commitment. Retain rollback history, then execute the durable maintenance queue.

Removal uses the same snapshot, hook, commit, and maintenance machinery after validating remaining dependencies. `search` holds a shared lock and refuses to read an interrupted transaction until recovery completes; modifying commands hold an exclusive lock.

### State, rollback, and recovery

All paths below are relative to the selected root:

| Path | Contents |
| --- | --- |
| `/etc/maple/config.toml` | Repository URL and rollback retention. |
| `/var/lib/maple/installed/*.toml` | Package metadata, tracked files/directories, config defaults, and retained hook scripts. |
| `/var/lib/maple/lock` | Process lock. |
| `/var/lib/maple/transactions/` | Journals, rollback backups, and maintenance state. |

Backups use streaming xz compression at level 1. Shared inodes are stored once without hard-linking backups to live files; older uncompressed backups remain readable. Restoration preserves supported attributes and links between backed-up paths, removing attributes introduced since the snapshot.

An active journal means the file transaction must be undone; a committed journal means package files stay and pending maintenance must finish. Recovery can itself be retried after interruption. If restoration fails, maple retains the backup and journal for another attempt. Keep the transaction directory intact.

Repeated `rollback` walks backward through retained points and consumes each restored point. It restores affected files and records to their pre-operation state, including overwriting later local edits. Unrelated paths and arbitrary script effects are outside the snapshot. To repair an offline system, use a working binary with `--root /path/to/mounted-root recover`.

### Downloads and storage

Repository requests use HTTP(S) with Rustls TLS. Archives of at least 1 MiB use four parallel ranges when the server advertises byte ranges and a strong ETag. Range responses are validated; unsupported servers use one stream, and failed range transfers fall back to a checked complete download. There is no persistent cache or separate synchronization command.

Temporary staging follows `TMPDIR`, normally `/tmp`; converter output is staged beside its destination. Known-length downloads check the advertised size plus 8 MiB headroom before writing. Streaming downloads, decompression, and conversion check free space during writes. Package decompression also starts with an approximate four-times-compressed-size estimate. Temporary files are removed on failure.

Transaction space checks cover affected filesystems, backups, and metadata headroom, conservatively charging the whole incoming payload to each filesystem. These estimates can overstate requirements and do not reserve space, cover quotas, or prevent concurrent writers from consuming it. Allocation failures are still handled as errors.

## Limitations

- **Trust:** no package signatures, key management, or automatic publisher trust. Checksums are optional. Offline isolation does not make untrusted package code safe to authorize.
- **Repository policy:** one index per operation, no dedicated authentication support, persistent version holds, automatic architecture selection, or parallel installed versions of the same name.
- **Resolution:** no automatic conflict-driven removal, replacements, orphan cleanup, or solving around payload ownership collisions. Optional dependencies are informational and search has a finite branch limit.
- **Compatibility:** ALPM ordering changes SemVer prerelease behavior. Arch conversion does not supply an ABI, a pacman compatibility environment, or global ALPM hooks.
- **Filesystem coverage:** no directory-symlink traversal, live sockets, or automatic file/directory type replacement. Timestamps have second precision; hard links outside the backed-up set cannot be restored.
- **Rollback scope:** file transactions are recoverable, not atomic filesystem snapshots. Running applications can see intermediate changes. Arbitrary script effects and generated maintenance state are not fully reversible. Recovery depends on intact backups and filesystem synchronization; maple is not a full-system backup tool.
- **Offline execution:** requires Bubblewrap, Linux namespaces, and working target tools. Hardware-specific maintenance and a complete Mercury boot require separate validation.

## Development

The implementation uses synchronous orchestration with parallel range workers for eligible downloads:

| Area | Source |
| --- | --- |
| CLI and terminal output | [main.rs](src/main.rs), [output.rs](src/output.rs) |
| Metadata, version comparison, dependency planning | [model.rs](src/model.rs), [version.rs](src/version.rs), [resolver.rs](src/resolver.rs) |
| Repository loading and downloads | [repository.rs](src/repository.rs), [download.rs](src/download.rs) |
| Package validation, archives, attributes | [package.rs](src/package.rs), [archive.rs](src/archive.rs), [attributes.rs](src/attributes.rs), [pax.rs](src/pax.rs) |
| Installed state, transactions, space checks | [database.rs](src/database.rs), [transaction.rs](src/transaction.rs), [space.rs](src/space.rs) |
| Lifecycle scripts and maintenance isolation | [hooks.rs](src/hooks.rs), [triggers.rs](src/triggers.rs) |
| Arch conversion | [convert.rs](src/convert.rs), [arch-to-maple.rs](src/bin/arch-to-maple.rs) |

Run:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Tests use temporary roots and a local HTTP server. They cover version selection, dependency backtracking, conflicts/cycles, file ownership and config protection, conversion, hooks, maintenance retries, rollback, crash recovery, and archive handling without external programs in `PATH`.

GNU tar and xz are needed for interoperability fixtures. Namespace, capability, and constrained-filesystem tests also need `unshare`, Bubblewrap, and `ldd`, plus usable user/mount namespaces. Tests report skips when namespaces are unavailable. Version tests always run the vendored pacman corpus (92 comparisons); an additional 484-pair test uses installed `vercmp` and reports a skip if unavailable. Require both optional coverage groups with:

```sh
MAPLE_REQUIRE_NAMESPACE_TESTS=1 MAPLE_REQUIRE_VERCMP_TESTS=1 cargo test --locked --all-targets
```

[CI](.github/workflows/cargo_tests.yml) runs formatting, linting, and tests. Sandbox fixtures exercise target programs and simulated dracut failure/retry; they do not build or boot a complete Mercury image.

## License

[GNU General Public License, version 2 (GPLv2)](LICENSE).
