use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use tempfile::{TempDir, tempdir};

fn maple(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_maple"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: Output, message: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(message),
        "expected {message:?}, got {stderr:?}"
    );
}

fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn package(name: &str, version: &str, files: &[(&str, &str)]) -> (TempDir, PathBuf) {
    let dir = tempdir().unwrap();
    write(
        dir.path().join("metadata.toml"),
        format!("name = {name:?}\nversion = {version:?}\ndescription = \"A test tool\"\n"),
    );
    fs::create_dir(dir.path().join("payload")).unwrap();
    for (path, text) in files {
        write(dir.path().join("payload").join(path), text);
    }
    let archive = pack(dir.path(), false);
    (dir, archive)
}

fn pack(dir: &Path, dot_prefix: bool) -> PathBuf {
    let archive = dir.join("test.maple");
    let members = if dot_prefix {
        ["./metadata.toml", "./payload"]
    } else {
        ["metadata.toml", "payload"]
    };
    success(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .env("XZ_OPT", "-0")
            .arg("-cJf")
            .arg(&archive)
            .arg("-C")
            .arg(dir)
            .args(members)
            .output()
            .unwrap(),
    );
    archive
}

fn record(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("var/lib/maple/installed/{name}.toml"))).unwrap()
}

#[test]
fn install_preserves_file_types_and_remove_keeps_untracked_files() {
    let root = tempdir().unwrap();
    let (build, _) = package(
        "hello",
        "1.0.0",
        &[("usr/bin/hello", "#!/bin/sh\necho hello\n")],
    );
    let payload = build.path().join("payload");
    fs::set_permissions(
        payload.join("usr/bin/hello"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::hard_link(
        payload.join("usr/bin/hello"),
        payload.join("usr/bin/hard-link"),
    )
    .unwrap();
    symlink("hello", payload.join("usr/bin/alias")).unwrap();
    symlink("/missing/absolute-target", payload.join("usr/bin/dangling")).unwrap();
    fs::create_dir_all(payload.join("opt/empty")).unwrap();
    success(
        Command::new("mkfifo")
            .arg(payload.join("usr/bin/pipe"))
            .output()
            .unwrap(),
    );
    let archive = pack(build.path(), true);
    success(maple(
        root.path(),
        &["install", archive.to_str().unwrap(), "--noconfirm"],
    ));
    let binary = root.path().join("usr/bin/hello");
    assert_eq!(
        fs::metadata(&binary).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&binary).unwrap().ino(),
        fs::metadata(root.path().join("usr/bin/hard-link"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        fs::read_link(root.path().join("usr/bin/alias")).unwrap(),
        Path::new("hello")
    );
    assert_eq!(
        fs::read_link(root.path().join("usr/bin/dangling")).unwrap(),
        Path::new("/missing/absolute-target")
    );
    assert!(
        fs::metadata(root.path().join("usr/bin/pipe"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    assert!(root.path().join("opt/empty").is_dir());
    assert!(record(root.path(), "hello").contains("1.0.0"));
    write(root.path().join("usr/bin/user-file"), "keep me");
    success(maple(root.path(), &["remove", "hello", "--noconfirm"]));
    assert!(!binary.exists());
    assert!(fs::symlink_metadata(root.path().join("usr/bin/dangling")).is_err());
    assert!(!root.path().join("opt/empty").exists());
    assert!(root.path().join("usr/bin/user-file").exists());
}

#[test]
fn upgrade_removes_obsolete_files_and_preserves_shared_directories() {
    let root = tempdir().unwrap();
    let (_a, a) = package(
        "hello",
        "1.0.0",
        &[("usr/bin/hello", "old"), ("opt/stale/file", "stale")],
    );
    let (_b, b) = package("other", "1.0.0", &[("usr/bin/other", "other")]);
    success(maple(
        root.path(),
        &[
            "install",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "--noconfirm",
        ],
    ));
    let (_new, new) = package("hello", "1.1.0", &[("usr/bin/hello", "new")]);
    success(maple(
        root.path(),
        &["install", new.to_str().unwrap(), "--noconfirm"],
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/hello")).unwrap(),
        "new"
    );
    assert!(!root.path().join("opt/stale").exists());
    success(maple(root.path(), &["remove", "hello", "--noconfirm"]));
    assert!(root.path().join("usr/bin/other").exists());
    success(maple(root.path(), &["remove", "other", "--noconfirm"]));
    assert!(!root.path().join("usr/bin").exists());
}

#[test]
fn conflicts_and_rejected_confirmation_leave_payload_untouched() {
    let root = tempdir().unwrap();
    let (_a, a) = package("a", "1.0.0", &[("usr/bin/shared", "a")]);
    let (_b, b) = package("b", "1.0.0", &[("usr/bin/shared", "b")]);
    failure(
        maple(
            root.path(),
            &[
                "install",
                a.to_str().unwrap(),
                b.to_str().unwrap(),
                "--noconfirm",
            ],
        ),
        "belongs to a",
    );
    assert!(!root.path().join("usr/bin/shared").exists());
    success(maple(root.path(), &["install", a.to_str().unwrap()])); // EOF defaults to no.
    assert!(!root.path().join("usr/bin/shared").exists());
    write(root.path().join("usr/bin/shared"), "unowned");
    failure(
        maple(
            root.path(),
            &["install", a.to_str().unwrap(), "--noconfirm"],
        ),
        "unowned file",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/shared")).unwrap(),
        "unowned"
    );
}

#[test]
fn rejects_broken_archives_and_directory_symlink_traversal() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), root.path().join("usr")).unwrap();
    let (build, archive) = package("hello", "1.0.0", &[("usr/bin/hello", "hello")]);
    failure(
        maple(
            root.path(),
            &["install", archive.to_str().unwrap(), "--noconfirm"],
        ),
        "not a real directory",
    );
    assert!(!outside.path().join("bin/hello").exists());
    fs::write(build.path().join("broken.maple"), "not an xz file").unwrap();
    failure(
        maple(
            root.path(),
            &[
                "install",
                build.path().join("broken.maple").to_str().unwrap(),
                "--noconfirm",
            ],
        ),
        "tar.xz",
    );
    let (_bad, bad) = package("../bad", "1.0.0", &[]);
    failure(
        maple(
            root.path(),
            &["install", bad.to_str().unwrap(), "--noconfirm"],
        ),
        "invalid package name",
    );
}

#[test]
fn database_lock_blocks_concurrent_changes() {
    let root = tempdir().unwrap();
    let lock = root.path().join("var/lib/maple/lock");
    write(&lock, "");
    let file = fs::File::open(lock).unwrap();
    file.lock().unwrap();
    failure(
        maple(root.path(), &["remove", "hello", "--noconfirm"]),
        "another Maple command",
    );
}

// Serve test packages locally, including one redirect to exercise downloads.
struct Server {
    address: String,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(directory: &Path) -> Self {
        Self::start_with_length(directory, true)
    }

    fn start_with_length(directory: &Path, known_length: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let directory = directory.to_owned();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let worker = thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let mut stream = stream.unwrap();
                let mut request = [0; 8192];
                let count = stream.read(&mut request).unwrap();
                let text = String::from_utf8_lossy(&request[..count]);
                let path = text.split_whitespace().nth(1).unwrap_or("/");
                if path == "/packages/hello/1.9.0.maple" {
                    write!(stream, "HTTP/1.1 302 Found\r\nLocation: /hello.maple\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let (status, bytes) = match fs::read(directory.join(path.trim_start_matches('/'))) {
                    Ok(bytes) => ("200 OK", bytes),
                    Err(_) => ("404 Not Found", Vec::new()),
                };
                if known_length {
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    )
                    .unwrap();
                } else {
                    write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\n\r\n").unwrap();
                }
                // Early space rejection deliberately closes the response body.
                let _ = stream.write_all(&bytes);
            }
        });
        Self {
            address,
            stopped,
            worker: Some(worker),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/repository.toml", self.address)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(&self.address);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn index(directory: &Path, name: &str, version: &str) {
    write(
        directory.join("repository.toml"),
        format!(
            "[[package]]\nname = {name:?}\nversion = {version:?}\ndescription = \"A handy editor\"\n"
        ),
    );
}

#[test]
fn repository_search_install_update_and_http_errors() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let server = Server::start(repo.path());
    let url = server.url();
    index(repo.path(), "hello", "1.9.0");
    let (_v1, v1) = package("hello", "1.9.0", &[("usr/bin/hello", "v1")]);
    fs::copy(v1, repo.path().join("hello.maple")).unwrap();
    // Exercise the default config location, generated URLs, and redirects.
    write(
        root.path().join("etc/maple/config.toml"),
        format!("repository = {url:?}\n"),
    );
    let output = success(maple(root.path(), &["search", "EDITOR"]));
    assert!(output.contains("hello-1.9.0"));
    success(maple(root.path(), &["install", "hello", "--noconfirm"]));
    assert!(success(maple(root.path(), &["search", "hello"])).contains("[installed]"));
    let (_v2, v2) = package("hello", "1.10.0", &[("usr/bin/hello", "v2")]);
    fs::create_dir_all(repo.path().join("packages/hello")).unwrap();
    fs::copy(&v2, repo.path().join("packages/hello/1.10.0.maple")).unwrap();
    index(repo.path(), "hello", "1.10.0");
    success(maple(root.path(), &["update", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/hello")).unwrap(),
        "v2"
    );
    assert!(success(maple(root.path(), &["update", "--noconfirm"])).contains("up to date"));
    index(repo.path(), "hello", "1.8.0");
    assert!(success(maple(root.path(), &["update", "--noconfirm"])).contains("up to date"));
    assert!(record(root.path(), "hello").contains("1.10.0"));
    fs::copy(&v2, repo.path().join("packages/hello/2.0.0.maple")).unwrap();
    index(repo.path(), "hello", "2.0.0");
    failure(
        maple(root.path(), &["update", "--noconfirm"]),
        "does not match",
    );
    index(repo.path(), "hello", "3.0.0");
    failure(maple(root.path(), &["update", "--noconfirm"]), "404");
    assert!(record(root.path(), "hello").contains("1.10.0"));
}

#[test]
fn update_replaces_the_running_maple_executable() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let server = Server::start(repo.path());
    let (build, _) = package("maple", "0.1.0", &[]);
    let binary = build.path().join("payload/usr/bin/maple");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_maple"), &binary).unwrap();
    let initial = pack(build.path(), false);
    success(maple(
        root.path(),
        &["install", initial.to_str().unwrap(), "--noconfirm"],
    ));
    // Replace the running binary with a script so we can check what runs next.
    let (next, _) = package(
        "maple",
        "0.2.0",
        &[("usr/bin/maple", "#!/bin/sh\necho new-maple\n")],
    );
    fs::set_permissions(
        next.path().join("payload/usr/bin/maple"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::create_dir_all(repo.path().join("packages/maple")).unwrap();
    fs::copy(
        pack(next.path(), false),
        repo.path().join("packages/maple/0.2.0.maple"),
    )
    .unwrap();
    index(repo.path(), "maple", "0.2.0");
    let installed_binary = root.path().join("usr/bin/maple");
    success(
        Command::new(&installed_binary)
            .arg("--root")
            .arg(root.path())
            .args(["--repo", &server.url(), "update", "--noconfirm"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        success(Command::new(installed_binary).output().unwrap()).trim(),
        "new-maple"
    );
    assert!(record(root.path(), "maple").contains("0.2.0"));
}

#[test]
fn cli_exposes_noconfirm_and_rejects_removed_flags() {
    let root = tempdir().unwrap();
    let help = success(maple(root.path(), &["--help"]));
    for command in ["pack", "self-update"] {
        assert!(!help.split_whitespace().any(|word| word == command));
        failure(maple(root.path(), &[command]), "unrecognized subcommand");
    }
    assert!(help.contains("--noconfirm"));
    assert!(!help.contains("--config"));
    assert!(!help.contains("--yes"));
    for flag in ["--config", "-y", "--y", "--yes"] {
        failure(maple(root.path(), &[flag, "search"]), "unexpected argument");
    }
    let (_build, archive) = package("hello", "1.0.0", &[("usr/bin/hello", "hello")]);
    success(maple(
        root.path(),
        &["--noconfirm", "install", archive.to_str().unwrap()],
    ));
    assert!(root.path().join("usr/bin/hello").is_file());
    success(maple(root.path(), &["--noconfirm", "remove", "hello"]));
    assert!(!root.path().join("usr/bin/hello").exists());
}

fn publish(repo: &Path, name: &str, version: &str, dependencies: &str) {
    let (build, _) = package(name, version, &[("opt/placeholder", "unused")]);
    fs::remove_dir_all(build.path().join("payload/opt")).unwrap();
    write(
        build.path().join(format!("payload/usr/bin/{name}")),
        version,
    );
    write(
        build.path().join("metadata.toml"),
        format!("name = {name:?}\nversion = {version:?}\ndependencies = {dependencies}\n"),
    );
    let entry = format!(
        "[[package]]\nname = {name:?}\nversion = {version:?}\ndependencies = {dependencies}\n"
    );
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(repo.join("repository.toml"))
        .unwrap();
    file.write_all(entry.as_bytes()).unwrap();
    fs::create_dir_all(repo.join(format!("packages/{name}"))).unwrap();
    fs::copy(
        pack(build.path(), false),
        repo.join(format!("packages/{name}/{version}.maple")),
    )
    .unwrap();
}

#[test]
fn resolves_transitive_dependencies_and_blocks_breaking_removal_and_upgrade() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    publish(repo.path(), "app", "1.0.0", "{ middle = \"^1\" }");
    publish(repo.path(), "middle", "1.0.0", "{ base = \">=1, <2\" }");
    publish(repo.path(), "base", "1.0.0", "{}");
    let server = Server::start(repo.path());
    let output = success(maple(
        root.path(),
        &["--repo", &server.url(), "install", "app", "--noconfirm"],
    ));
    assert_eq!(output.matches("Downloading packages...").count(), 1);
    assert_eq!(output.matches("Downloaded 3 packages (").count(), 1);
    for (index, name) in ["base", "middle", "app"].iter().enumerate() {
        assert_eq!(
            output
                .matches(&format!("Downloaded [{}] {name}-1.0.0", index + 1))
                .count(),
            1
        );
    }
    assert!(!output.contains('\r'));
    assert!(!output.contains('\x1b'));
    for name in ["base", "middle", "app"] {
        assert!(root.path().join(format!("usr/bin/{name}")).exists());
    }
    failure(
        maple(root.path(), &["remove", "base", "--noconfirm"]),
        "would break this dependency",
    );
    let (_build, newer) = package("base", "2.0.0", &[("usr/bin/base", "2.0.0")]);
    failure(
        maple(
            root.path(),
            &[
                "--repo",
                &server.url(),
                "install",
                newer.to_str().unwrap(),
                "--noconfirm",
            ],
        ),
        "dependency conflict",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/base")).unwrap(),
        "1.0.0"
    );
    // Rollback should remove the dependencies along with the app.
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    for name in ["base", "middle", "app"] {
        assert!(!root.path().join(format!("usr/bin/{name}")).exists());
    }
}

#[test]
fn dependency_cycles_work_and_missing_or_conflicting_dependencies_do_not_install() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    publish(repo.path(), "a", "1.0.0", "{ b = \"^1\" }");
    publish(repo.path(), "b", "1.0.0", "{ a = \"^1\" }");
    publish(repo.path(), "broken", "1.0.0", "{ missing = \"*\" }");
    publish(repo.path(), "conflict", "1.0.0", "{ b = \"^2\" }");
    let server = Server::start(repo.path());
    failure(
        maple(
            root.path(),
            &["--repo", &server.url(), "install", "broken", "--noconfirm"],
        ),
        "dependency not found",
    );
    failure(
        maple(
            root.path(),
            &[
                "--repo",
                &server.url(),
                "install",
                "a",
                "conflict",
                "--noconfirm",
            ],
        ),
        "dependency conflict",
    );
    assert!(!root.path().join("usr/bin/a").exists());
    success(maple(
        root.path(),
        &["--repo", &server.url(), "install", "a", "--noconfirm"],
    ));
    success(maple(root.path(), &["remove", "a", "b", "--noconfirm"]));
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert!(root.path().join("usr/bin/a").exists());
    assert!(root.path().join("usr/bin/b").exists());
}

#[test]
fn rollback_restores_upgrades_and_removals_and_runs_without_archive_programs() {
    let root = tempdir().unwrap();
    let (build, _) = package(
        "hello",
        "1.0.0",
        &[("usr/bin/hello", "old"), ("opt/old", "keep")],
    );
    symlink("hello", build.path().join("payload/usr/bin/link")).unwrap();
    fs::hard_link(
        build.path().join("payload/usr/bin/hello"),
        build.path().join("payload/usr/bin/hard"),
    )
    .unwrap();
    success(
        Command::new("mkfifo")
            .arg(build.path().join("payload/usr/bin/pipe"))
            .output()
            .unwrap(),
    );
    let archive = pack(build.path(), false);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_maple"))
            .env("PATH", "")
            .arg("--root")
            .arg(root.path())
            .args(args)
            .output()
            .unwrap()
    };
    success(run(&["install", archive.to_str().unwrap(), "--noconfirm"]));
    assert!(
        fs::metadata(root.path().join("usr/bin/pipe"))
            .unwrap()
            .file_type()
            .is_fifo()
    );
    let (_next, next) = package(
        "hello",
        "2.0.0",
        &[("usr/bin/hello", "new"), ("opt/new", "new")],
    );
    success(run(&["install", next.to_str().unwrap(), "--noconfirm"]));
    success(run(&["rollback", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/hello")).unwrap(),
        "old"
    );
    assert_eq!(
        fs::metadata(root.path().join("usr/bin/hello"))
            .unwrap()
            .ino(),
        fs::metadata(root.path().join("usr/bin/hard"))
            .unwrap()
            .ino()
    );
    assert!(root.path().join("opt/old").exists());
    assert!(!root.path().join("opt/new").exists());
    assert!(record(root.path(), "hello").contains("1.0.0"));
    success(run(&["remove", "hello", "--noconfirm"]));
    success(run(&["rollback", "--noconfirm"]));
    assert!(root.path().join("usr/bin/hello").exists());
    failure(
        run(&["rollback", "--noconfirm"]),
        "no completed transaction",
    );
    success(run(&["recover"]));
}

#[test]
fn failure_during_second_package_restores_the_whole_transaction() {
    let root = tempdir().unwrap();
    let (_first, first) = package("a", "1.0.0", &[("usr/bin/a", "old")]);
    success(maple(
        root.path(),
        &["install", first.to_str().unwrap(), "--noconfirm"],
    ));
    let (_next, next) = package("a", "2.0.0", &[("usr/bin/a", "new")]);
    let bad = tempdir().unwrap();
    let path = bad.path().join("bad.maple");
    let encoder = xz2::write::XzEncoder::new(fs::File::create(&path).unwrap(), 1);
    let mut builder = tar::Builder::new(encoder);
    let metadata = b"name = \"zbad\"\nversion = \"1.0.0\"\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    builder
        .append_data(&mut header, "metadata.toml", &metadata[..])
        .unwrap();
    // Structurally valid metadata whose application fails on Linux: user
    // xattrs are forbidden on symlinks. The earlier package has changed by then.
    builder
        .append_pax_extensions([("SCHILY.xattr.user.test", b"value".as_slice())])
        .unwrap();
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Symlink);
    builder
        .append_link(&mut header, "payload/usr/bin/zbad", "missing-target")
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    failure(
        maple(
            root.path(),
            &[
                "install",
                next.to_str().unwrap(),
                path.to_str().unwrap(),
                "--noconfirm",
            ],
        ),
        "transaction rolled back",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/a")).unwrap(),
        "old"
    );
    assert!(record(root.path(), "a").contains("1.0.0"));
    assert!(!root.path().join("usr/bin/zbad").exists());
    assert!(
        !root
            .path()
            .join("var/lib/maple/installed/zbad.toml")
            .exists()
    );
    assert!(
        !root
            .path()
            .join("var/lib/maple/transactions/active.toml")
            .exists()
    );
    // The previous successful install should still be available to roll back.
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert!(!root.path().join("usr/bin/a").exists());
}

#[test]
fn rollback_handles_read_only_payload_directories() {
    let root = tempdir().unwrap();
    let (build, _) = package("readonly", "1.0.0", &[("opt/readonly/file", "data")]);
    fs::set_permissions(
        build.path().join("payload/opt/readonly"),
        fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    let archive = pack(build.path(), false);
    success(maple(
        root.path(),
        &["install", archive.to_str().unwrap(), "--noconfirm"],
    ));
    assert_eq!(
        fs::metadata(root.path().join("opt/readonly"))
            .unwrap()
            .mode()
            & 0o777,
        0o555
    );
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert!(!root.path().join("opt/readonly").exists());
    // Let TempDir clean up when the tests run without root.
    fs::set_permissions(
        build.path().join("payload/opt/readonly"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

#[test]
fn confirmation_defaults_to_yes_but_eof_and_no_cancel() {
    let (_build, archive) = package("hello", "1.0.0", &[("usr/bin/hello", "hello")]);
    for (answer, installed) in [
        ("\n", true),
        ("y\n", true),
        ("YES\n", true),
        ("n\n", false),
        ("", false),
    ] {
        let root = tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_maple"))
            .arg("--root")
            .arg(root.path())
            .arg("install")
            .arg(&archive)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        let output = success(child.wait_with_output().unwrap());
        assert!(output.contains(":: Proceed? [Y/n]"));
        assert!(output.contains("Packages (1)  hello-1.0.0"));
        assert_eq!(
            root.path().join("usr/bin/hello").exists(),
            installed,
            "{answer:?}"
        );
        assert_eq!(output.contains("Cancelled"), !installed);
        assert!(
            !output.contains("[maple INFO]") && !output.contains('\x1b'),
            "{output}"
        );
    }
}

#[test]
fn errors_use_pacman_style_prefix() {
    let root = tempdir().unwrap();
    for args in [
        &["remove", "missing", "--noconfirm"][..],
        &["--invalid"][..],
    ] {
        let output = maple(root.path(), args);
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.lines().all(|line| line.starts_with("error:")),
            "{stderr}"
        );
    }
}

fn with_metadata(build: &Path, extra: &str) -> PathBuf {
    let path = build.join("metadata.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(extra);
    fs::write(path, text).unwrap();
    pack(build, false)
}

#[test]
fn protected_config_upgrade_remove_and_rollback() {
    let root = tempdir().unwrap();
    let (v1, _) = package("app", "1.0.0", &[("etc/app.conf", "default one")]);
    let a = with_metadata(v1.path(), "config_files = [\"etc/app.conf\"]\n");
    success(maple(
        root.path(),
        &["install", a.to_str().unwrap(), "--noconfirm"],
    ));
    write(root.path().join("etc/app.conf"), "user edit");
    let (v2, _) = package("app", "2.0.0", &[("etc/app.conf", "default two")]);
    let b = with_metadata(v2.path(), "config_files = [\"etc/app.conf\"]\n");
    success(maple(
        root.path(),
        &["install", b.to_str().unwrap(), "--noconfirm"],
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf")).unwrap(),
        "user edit"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf.maple-new")).unwrap(),
        "default two"
    );
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf")).unwrap(),
        "user edit"
    );
    assert!(!root.path().join("etc/app.conf.maple-new").exists());
    success(maple(
        root.path(),
        &["install", b.to_str().unwrap(), "--noconfirm"],
    ));
    success(maple(root.path(), &["remove", "app", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf")).unwrap(),
        "user edit"
    );
    assert!(!root.path().join("etc/app.conf.maple-new").exists());
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert!(root.path().join("etc/app.conf.maple-new").exists());
}

#[test]
fn config_defaults_deletions_symlinks_and_sidecar_collisions() {
    let root = tempdir().unwrap();
    let (v1, _) = package("app", "1.0.0", &[("etc/app.conf", "one")]);
    let a = with_metadata(v1.path(), "config_files = [\"etc/app.conf\"]\n");
    let (v2, _) = package("app", "2.0.0", &[("etc/app.conf", "two")]);
    let b = with_metadata(v2.path(), "config_files = [\"etc/app.conf\"]\n");
    success(maple(
        root.path(),
        &["install", a.to_str().unwrap(), "--noconfirm"],
    ));
    success(maple(
        root.path(),
        &["install", b.to_str().unwrap(), "--noconfirm"],
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf")).unwrap(),
        "two"
    );
    assert!(!root.path().join("etc/app.conf.maple-new").exists());
    fs::remove_file(root.path().join("etc/app.conf")).unwrap();
    success(maple(
        root.path(),
        &["install", a.to_str().unwrap(), "--noconfirm"],
    ));
    assert!(!root.path().join("etc/app.conf").exists());
    let outside = tempdir().unwrap();
    write(outside.path().join("config"), "outside");
    symlink(
        outside.path().join("config"),
        root.path().join("etc/app.conf"),
    )
    .unwrap();
    success(maple(
        root.path(),
        &["install", b.to_str().unwrap(), "--noconfirm"],
    ));
    assert_eq!(
        fs::read_to_string(outside.path().join("config")).unwrap(),
        "outside"
    );
    write(root.path().join("etc/app.conf.maple-new"), "edited sidecar");
    failure(
        maple(
            root.path(),
            &["install", a.to_str().unwrap(), "--noconfirm"],
        ),
        "config sidecar",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("etc/app.conf.maple-new")).unwrap(),
        "edited sidecar"
    );
    assert!(record(root.path(), "app").contains("2.0.0"));
}

#[test]
fn virtual_optional_and_conflicting_dependencies() {
    let root = tempdir().unwrap();
    let (provider, _) = package("provider", "1.0.0", &[("lib/provider", "lib")]);
    let p = with_metadata(provider.path(), "[provides]\nvirtual = \"2.0.0\"\n");
    let (app, _) = package("app", "1.0.0", &[("bin/app", "app")]);
    let a = with_metadata(
        app.path(),
        "[dependencies]\nvirtual = \"^2\"\n[optional_dependencies]\nmissing = \"^1\"\n",
    );
    success(maple(
        root.path(),
        &[
            "install",
            a.to_str().unwrap(),
            p.to_str().unwrap(),
            "--noconfirm",
        ],
    ));
    failure(
        maple(root.path(), &["remove", "provider", "--noconfirm"]),
        "requires virtual",
    );
    let (conflict, _) = package("conflict", "1.0.0", &[("bin/conflict", "bad")]);
    let c = with_metadata(conflict.path(), "[conflicts]\nvirtual = \"^2\"\n");
    failure(
        maple(
            root.path(),
            &["install", c.to_str().unwrap(), "--noconfirm"],
        ),
        "conflicts with virtual",
    );
    assert!(!root.path().join("bin/conflict").exists());
    success(maple(
        root.path(),
        &["remove", "provider", "app", "--noconfirm"],
    ));
}

#[test]
fn retained_history_and_offline_systemd_survive_rollback() {
    let root = tempdir().unwrap();
    write(
        root.path().join("etc/maple/config.toml"),
        "rollback_retention = 2\n",
    );
    for version in ["1.0.0", "2.0.0", "3.0.0"] {
        let (build, _) = package("app", version, &[("app", version)]);
        let a = with_metadata(build.path(), "triggers = [\"systemd\"]\n");
        success(maple(
            root.path(),
            &["install", a.to_str().unwrap(), "--noconfirm"],
        ));
    }
    let directory = root.path().join("var/lib/maple/transactions");
    assert_eq!(
        fs::read_dir(&directory)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("snapshot-"))
            .count(),
        2
    );
    assert!(!directory.join("triggers.toml").exists());
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("app")).unwrap(),
        "2.0.0"
    );
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("app")).unwrap(),
        "1.0.0"
    );
    failure(
        maple(root.path(), &["rollback", "--noconfirm"]),
        "no completed transaction",
    );
    assert!(!directory.join("triggers.toml").exists());
}

#[test]
fn rejects_unknown_triggers_and_invalid_configs() {
    for extra in [
        "triggers = [\"sh -c evil\"]\n",
        "config_files = [\"../escape\"]\n",
        "config_files = [\"missing\"]\n",
    ] {
        let root = tempdir().unwrap();
        let (build, _) = package("app", "1.0.0", &[("app", "app")]);
        let a = with_metadata(build.path(), extra);
        let result = maple(
            root.path(),
            &["install", a.to_str().unwrap(), "--noconfirm"],
        );
        assert!(!result.status.success());
        assert!(!root.path().join("app").exists());
    }
}

fn set_xattr(path: &Path, name: &str, value: &[u8]) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    // SAFETY: C strings and value buffer are valid for this syscall.
    assert_eq!(
        unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}

fn get_xattr(path: &Path, name: &str) -> Vec<u8> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let name = CString::new(name).unwrap();
    let mut bytes = vec![0u8; 65536];
    // SAFETY: C strings and output buffer are valid for this syscall.
    let size = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    assert!(size >= 0, "{}", std::io::Error::last_os_error());
    bytes.truncate(size as usize);
    bytes
}

#[test]
fn gnu_pax_binary_xattrs_and_acls_install_and_rollback() {
    let root = tempdir().unwrap();
    let (build, archive) = package("attributes", "1.0.0", &[("etc/file", "contents")]);
    let file = build.path().join("payload/etc/file");
    let directory = build.path().join("payload/etc");
    let binary = [0, 10, 255, 13, 61];
    set_xattr(&file, "user.binary", &binary);
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, mode, id) in [
        (1u16, 6u16, u32::MAX),
        (2, 4, 12345),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(mode.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    set_xattr(&file, "system.posix_acl_access", &acl);
    set_xattr(&directory, "system.posix_acl_default", &acl);
    success(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .args([
                "--format=pax",
                "--numeric-owner",
                "--acls",
                "--xattrs",
                "--xattrs-include=*",
                "-cJf",
            ])
            .arg(&archive)
            .arg("-C")
            .arg(build.path())
            .args(["metadata.toml", "payload"])
            .output()
            .unwrap(),
    );
    success(maple(
        root.path(),
        &["install", archive.to_str().unwrap(), "--noconfirm"],
    ));
    let installed = root.path().join("etc/file");
    assert_eq!(get_xattr(&installed, "user.binary"), binary);
    assert_eq!(get_xattr(&installed, "system.posix_acl_access"), acl);
    assert_eq!(
        get_xattr(&root.path().join("etc"), "system.posix_acl_default"),
        acl
    );
    success(maple(root.path(), &["remove", "attributes", "--noconfirm"]));
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(get_xattr(&installed, "user.binary"), binary);
    assert_eq!(get_xattr(&installed, "system.posix_acl_access"), acl);
    assert_eq!(
        get_xattr(&root.path().join("etc"), "system.posix_acl_default"),
        acl
    );
}

#[test]
fn repository_selects_versioned_virtual_provider_and_checks_metadata() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let server = Server::start(repo.path());
    let (provider, _) = package("provider", "1.0.0", &[("lib/provider", "lib")]);
    let p = with_metadata(provider.path(), "[provides]\nvirtual = \"2.0.0\"\n");
    let (app, _) = package("app", "1.0.0", &[("bin/app", "app")]);
    let a = with_metadata(app.path(), "[dependencies]\nvirtual = \"^2\"\n");
    write(
        repo.path().join("packages/provider/1.0.0.maple"),
        fs::read(p).unwrap(),
    );
    write(
        repo.path().join("repository.toml"),
        "[[package]]\nname = \"provider\"\nversion = \"1.0.0\"\n[package.provides]\nvirtual = \"2.0.0\"\n",
    );
    success(maple(
        root.path(),
        &[
            "--repo",
            &server.url(),
            "install",
            a.to_str().unwrap(),
            "--noconfirm",
        ],
    ));
    assert!(root.path().join("lib/provider").exists());
    let other_root = tempdir().unwrap();
    write(
        repo.path().join("repository.toml"),
        "[[package]]\nname = \"provider\"\nversion = \"1.0.0\"\n[package.provides]\nvirtual = \"2.1.0\"\n",
    );
    failure(
        maple(
            other_root.path(),
            &[
                "--repo",
                &server.url(),
                "install",
                a.to_str().unwrap(),
                "--noconfirm",
            ],
        ),
        "metadata does not match",
    );
    assert!(!other_root.path().join("bin/app").exists());
}

#[test]
fn alternate_root_rejects_symlinked_transaction_storage() {
    let root = tempdir().unwrap();
    let outside = tempdir().unwrap();
    fs::create_dir_all(root.path().join("var/lib/maple")).unwrap();
    symlink(
        outside.path(),
        root.path().join("var/lib/maple/transactions"),
    )
    .unwrap();
    failure(
        maple(root.path(), &["recover"]),
        "parent is not a real directory",
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

// Re-execute the actual test in a private user namespace, giving it capabilities
// over its own fixtures without requiring sudo or touching host system state.
fn namespace_test(name: &str) -> bool {
    if std::env::var("MAPLE_NAMESPACE_TEST").as_deref() == Ok(name) {
        return true;
    }
    let probe = Command::new("unshare").args(["-Urm", "true"]).output();
    if !probe.as_ref().is_ok_and(|p| p.status.success()) {
        assert!(
            std::env::var_os("MAPLE_REQUIRE_NAMESPACE_TESTS").is_none(),
            "user namespaces required for {name}"
        );
        eprintln!(
            "SKIP {name}: unshare -Urm unavailable; set MAPLE_REQUIRE_NAMESPACE_TESTS=1 to require privileged integration coverage"
        );
        return false;
    }
    success(
        Command::new("unshare")
            .args(["-Urm"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("MAPLE_NAMESPACE_TEST", name)
            .output()
            .unwrap(),
    );
    false
}

#[test]
fn capabilities_acls_xattrs_survive_install_rollback_and_process_crash() {
    let name = "capabilities_acls_xattrs_survive_install_rollback_and_process_crash";
    if !namespace_test(name) {
        return;
    }
    let root = tempdir().unwrap();
    let (build, archive) = package("metadata", "1.0.0", &[("etc/tool", "old")]);
    let file = build.path().join("payload/etc/tool");
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, mode, id) in [
        (1u16, 7u16, u32::MAX),
        (2, 4, 0),
        (4, 4, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(mode.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    let cap = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    set_xattr(&file, "user.binary", &[0, 10, 255]);
    set_xattr(&file, "system.posix_acl_access", &acl);
    set_xattr(
        &build.path().join("payload/etc"),
        "system.posix_acl_default",
        &acl,
    );
    set_xattr(&file, "security.capability", &cap);
    let expected_cap = get_xattr(&file, "security.capability");
    success(
        Command::new("tar")
            .env_remove("TAR_OPTIONS")
            .args([
                "--format=pax",
                "--numeric-owner",
                "--acls",
                "--xattrs",
                "--xattrs-include=*",
                "-cJf",
            ])
            .arg(&archive)
            .arg("-C")
            .arg(build.path())
            .args(["metadata.toml", "payload"])
            .output()
            .unwrap(),
    );
    success(maple(
        root.path(),
        &["install", archive.to_str().unwrap(), "--noconfirm"],
    ));
    let check = || {
        let file = root.path().join("etc/tool");
        assert_eq!(get_xattr(&file, "security.capability"), expected_cap);
        assert_eq!(get_xattr(&file, "system.posix_acl_access"), acl);
        assert_eq!(get_xattr(&file, "user.binary"), [0, 10, 255]);
        assert_eq!(
            get_xattr(&root.path().join("etc"), "system.posix_acl_default"),
            acl
        );
    };
    check();
    let (_next, next) = package("metadata", "2.0.0", &[("etc/tool", "new")]);
    success(maple(
        root.path(),
        &["install", next.to_str().unwrap(), "--noconfirm"],
    ));
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    check();
    // Reuse Maple's real, flushed pre-upgrade snapshot, then crash a separate
    // process after publishing active.toml and partially replacing metadata.
    success(maple(
        root.path(),
        &["install", next.to_str().unwrap(), "--noconfirm"],
    ));
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "metadata_crash_child"])
        .env("MAPLE_METADATA_CRASH_ROOT", root.path())
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(77));
    success(maple(root.path(), &["recover"]));
    check();
    success(maple(root.path(), &["recover"]));
    check();
}

#[test]
fn metadata_crash_child() {
    let Some(root) = std::env::var_os("MAPLE_METADATA_CRASH_ROOT") else {
        return;
    };
    let root = Path::new(&root);
    let dir = root.join("var/lib/maple/transactions");
    fs::rename(dir.join("last.toml"), dir.join("active.toml")).unwrap();
    fs::File::open(&dir).unwrap().sync_all().unwrap();
    fs::write(root.join("etc/tool"), "interrupted").unwrap();
    set_xattr(&root.join("etc/tool"), "user.binary", b"changed");
    set_xattr(&root.join("etc/tool"), "user.extra", b"must disappear");
    let mut changed_acl = 2u32.to_le_bytes().to_vec();
    for (tag, mode) in [(1u16, 7u16), (4, 0), (32, 0)] {
        changed_acl.extend(tag.to_le_bytes());
        changed_acl.extend(mode.to_le_bytes());
        changed_acl.extend(u32::MAX.to_le_bytes());
    }
    set_xattr(&root.join("etc"), "system.posix_acl_default", &changed_acl);
    fs::File::open(root.join("etc/tool"))
        .unwrap()
        .sync_all()
        .unwrap();
    std::process::exit(77);
}

fn copy_shell(root: &Path) {
    copy_executable(root, Path::new("/bin/sh"), "bin/sh");
    let chroot = ["/usr/sbin/chroot", "/usr/bin/chroot"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.exists())
        .unwrap();
    copy_executable(root, chroot, "usr/bin/chroot");
}

fn copy_executable(root: &Path, source: &Path, destination: &str) {
    let shell = fs::canonicalize(source).unwrap();
    write(root.join(destination), fs::read(&shell).unwrap());
    fs::set_permissions(root.join(destination), fs::Permissions::from_mode(0o755)).unwrap();
    let output = success(Command::new("ldd").arg(shell).output().unwrap());
    for word in output.split_whitespace().filter(|w| w.starts_with('/')) {
        let path = Path::new(word);
        write(
            root.join(path.strip_prefix("/").unwrap()),
            fs::read(path).unwrap(),
        );
        fs::set_permissions(
            root.join(path.strip_prefix("/").unwrap()),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

#[test]
fn offline_maintenance_is_isolated_ordered_and_retryable() {
    if !namespace_test("offline_maintenance_is_isolated_ordered_and_retryable") {
        return;
    }
    let root = tempdir().unwrap();
    copy_shell(root.path());
    fs::create_dir_all(root.path().join("usr/lib/modules/6.12-mercury")).unwrap();
    fs::create_dir(root.path().join("boot")).unwrap();
    let outside = tempdir().unwrap();
    let sentinel = outside.path().join("host");
    write(&sentinel, "untouched");
    // Inside the target this absolute host pathname cannot resolve to the host.
    symlink(&sentinel, root.path().join("host-link")).unwrap();
    for (program, label) in [
        ("usr/bin/systemd-sysusers", "users"),
        ("usr/bin/systemd-tmpfiles", "files"),
        ("sbin/ldconfig", "cache"),
    ] {
        write(
            root.path().join(program),
            format!("#!/bin/sh\necho {label} >> /order\n"),
        );
        fs::set_permissions(root.path().join(program), fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(root.path().join("usr/bin/dracut"), "#!/bin/sh\nexit 42\n");
    fs::set_permissions(
        root.path().join("usr/bin/dracut"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (build, _) = package("boot", "1.0.0", &[("installed", "committed")]);
    let archive = with_metadata(
        build.path(),
        "triggers = [\"initramfs\", \"systemd-tmpfiles\", \"systemd-sysusers\", \"ldconfig\", \"systemd-sysusers\", \"systemd\"]\n",
    );
    let failed = maple(
        root.path(),
        &["install", archive.to_str().unwrap(), "--noconfirm"],
    );
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Installation complete"));
    failure(failed, "transaction committed");
    assert_eq!(
        fs::read_to_string(root.path().join("installed")).unwrap(),
        "committed"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("order")).unwrap(),
        "users\nfiles\ncache\n"
    );
    let queue = root.path().join("var/lib/maple/transactions/triggers.toml");
    let queued = fs::read_to_string(&queue).unwrap();
    assert!(queued.contains("initramfs"));
    assert!(!queued.contains("sysusers"));
    write(
        root.path().join("usr/bin/dracut"),
        "#!/bin/sh\n/usr/bin/chroot / /bin/sh -c ':' || exit 94\n[ \"$1 $2 $3 $4 $5\" = '--force --no-hostonly --no-hostonly-cmdline --kver 6.12-mercury' ] || exit 90\n[ ! -b /dev/sda ] || exit 91\necho escape > /host-link 2>/dev/null && exit 92\necho bad > /var/lib/maple/lock 2>/dev/null && exit 93\necho target > /boot/initramfs-6.12-mercury.img\n",
    );
    success(maple(root.path(), &["recover"]));
    assert!(!queue.exists());
    assert_eq!(
        fs::read_to_string(root.path().join("boot/initramfs-6.12-mercury.img")).unwrap(),
        "target\n"
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "untouched");
    assert_eq!(
        fs::read_to_string(root.path().join("order")).unwrap(),
        "users\nfiles\ncache\n"
    );
}

#[test]
fn failed_offline_trigger_does_not_block_rollback() {
    let root = tempdir().unwrap();
    let (build, _) = package("boot", "1.0.0", &[("installed", "committed")]);
    let archive = with_metadata(build.path(), "triggers = [\"initramfs\"]\n");
    failure(
        maple(
            root.path(),
            &["install", archive.to_str().unwrap(), "--noconfirm"],
        ),
        "transaction committed",
    );
    let repo = tempdir().unwrap();
    index(repo.path(), "boot", "1.0.0");
    let server = Server::start(repo.path());
    let update = maple(
        root.path(),
        &["--repo", &server.url(), "update", "--noconfirm"],
    );
    assert!(!String::from_utf8_lossy(&update.stdout).contains("Everything is up to date"));
    failure(update, "maintenance remains pending");
    failure(
        maple(root.path(), &["rollback", "--noconfirm"]),
        "installed target kernels",
    );
    assert!(!root.path().join("installed").exists());
    assert!(
        !root
            .path()
            .join("var/lib/maple/transactions/active.toml")
            .exists()
    );
    assert!(
        root.path()
            .join("var/lib/maple/transactions/triggers.toml")
            .exists()
    );
}

#[test]
fn temporary_filesystem_checks_downloads_and_high_ratio_decompression() {
    if !namespace_test("temporary_filesystem_checks_downloads_and_high_ratio_decompression") {
        return;
    }
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let temporary = tempdir().unwrap();
    let target = CString::new(temporary.path().as_os_str().as_bytes()).unwrap();
    // SAFETY: valid C strings; this test runs in its own user/mount namespaces.
    assert_eq!(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                target.as_ptr(),
                c"tmpfs".as_ptr(),
                0,
                c"size=12m".as_ptr().cast(),
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    struct Mount(CString);
    impl Drop for Mount {
        fn drop(&mut self) {
            // SAFETY: valid private mount-point path, no host mounts are affected.
            assert_eq!(unsafe { libc::umount(self.0.as_ptr()) }, 0);
        }
    }
    let _mount = Mount(target);
    let root = tempdir().unwrap();
    let (build, archive) = package(
        "expanded",
        "1.0.0",
        &[("large", &"x".repeat(6 * 1024 * 1024))],
    );
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_maple"))
            .arg("--root")
            .arg(root.path())
            .args(args)
            .env("TMPDIR", temporary.path())
            .output()
            .unwrap()
    };
    failure(
        invoke(&["install", archive.to_str().unwrap(), "--noconfirm"]),
        "temporary disk space",
    );
    assert!(!root.path().join("large").exists());
    assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
    drop(build);
    let repo = tempdir().unwrap();
    index(repo.path(), "large", "1.0.0");
    write(
        repo.path().join("packages/large/1.0.0.maple"),
        vec![0u8; 6 * 1024 * 1024],
    );
    for known_length in [true, false] {
        let server = Server::start_with_length(repo.path(), known_length);
        failure(
            invoke(&["--repo", &server.url(), "install", "large", "--noconfirm"]),
            "temporary disk space",
        );
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 0);
    }
}

#[test]
fn multiversion_backtracking_pins_updates_and_downgrades() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    publish(repo.path(), "app", "2.0.0", "{ lib = \"^2\" }");
    publish(repo.path(), "app", "1.0.0", "{ lib = \"^1\" }");
    publish(repo.path(), "lib", "1.9.0", "{}");
    publish(repo.path(), "lib", "1.10.0", "{}");
    publish(repo.path(), "lib", "2.0.0", "{ missing = \"*\" }");
    let server = Server::start(repo.path());
    success(maple(
        root.path(),
        &["--repo", &server.url(), "install", "app", "--noconfirm"],
    ));
    assert!(record(root.path(), "app").contains("1.0.0"));
    assert!(record(root.path(), "lib").contains("1.10.0"));
    success(maple(
        root.path(),
        &[
            "--repo",
            &server.url(),
            "install",
            "lib=1.9.0",
            "--noconfirm",
        ],
    ));
    assert!(record(root.path(), "lib").contains("1.9.0"));
    success(maple(
        root.path(),
        &["--repo", &server.url(), "update", "--noconfirm"],
    ));
    assert!(record(root.path(), "lib").contains("1.10.0"));
    failure(
        maple(
            root.path(),
            &[
                "--repo",
                &server.url(),
                "install",
                "app=2.0.0",
                "--noconfirm",
            ],
        ),
        "dependency conflict",
    );
    assert!(record(root.path(), "app").contains("1.0.0"));
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert!(record(root.path(), "lib").contains("1.9.0"));
}

#[test]
fn repository_checksums_and_duplicate_versions() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    publish(repo.path(), "app", "1.0.0", "{}");
    let server = Server::start(repo.path());
    let index = fs::read_to_string(repo.path().join("repository.toml")).unwrap();
    let sum = maple::repository::sha256(&repo.path().join("packages/app/1.0.0.maple")).unwrap();
    write(
        repo.path().join("repository.toml"),
        format!("{index}\n[sha256]\n\"app/1.0.0\" = {:?}\n", "0".repeat(64)),
    );
    failure(
        maple(
            root.path(),
            &["--repo", &server.url(), "install", "app", "--noconfirm"],
        ),
        "SHA-256 mismatch",
    );
    assert!(!root.path().join("usr/bin/app").exists());
    write(
        repo.path().join("repository.toml"),
        format!("{index}\n[sha256]\n\"app/1.0.0\" = {sum:?}\n"),
    );
    success(maple(
        root.path(),
        &["--repo", &server.url(), "install", "app", "--noconfirm"],
    ));
    write(
        repo.path().join("repository.toml"),
        format!("{index}\n{index}"),
    );
    failure(
        maple(root.path(), &["--repo", &server.url(), "search"]),
        "duplicate repository package version",
    );
}

fn hooks_package(version: &str, failing: bool) -> (TempDir, PathBuf) {
    let (build, _) = package("scripted", version, &[("tracked", version)]);
    let names = [
        "pre-install",
        "post-install",
        "pre-upgrade",
        "post-upgrade",
        "pre-remove",
        "post-remove",
    ];
    let metadata = fs::read_to_string(build.path().join("metadata.toml")).unwrap();
    write(
        build.path().join("metadata.toml"),
        format!("{metadata}\nhooks = {names:?}\n"),
    );
    for name in names {
        let script = format!(
            "#!/bin/sh\n[ \"$MAPLE_PACKAGE\" = scripted ] || exit 81\nprintf '%s %s %s\\n' {name} \"$1\" \"${{2-}}\" >> /hook-log\n[ ! -e /host-sentinel ] || exit 82\n[ ! -e /var/lib/maple/installed/scripted.toml ] || :\n{}\n",
            if failing && name == "post-upgrade" {
                "exit 42"
            } else {
                ":"
            }
        );
        write(build.path().join(format!("hooks/{name}")), script);
    }
    let archive = build.path().join("hooks.maple");
    success(
        Command::new("tar")
            .args(["-cJf"])
            .arg(&archive)
            .arg("-C")
            .arg(build.path())
            .args(["metadata.toml", "payload", "hooks"])
            .output()
            .unwrap(),
    );
    (build, archive)
}

#[test]
fn hooks_require_trust_and_never_fall_back_to_host_shell() {
    let root = tempdir().unwrap();
    let (_build, archive) = hooks_package("1.0.0", false);
    failure(
        maple(
            root.path(),
            &["install", archive.to_str().unwrap(), "--noconfirm"],
        ),
        "--trust-hooks",
    );
    assert!(!root.path().join("tracked").exists());
    // No target shell: even authorized scripts must fail and roll back.
    failure(
        maple(
            root.path(),
            &[
                "install",
                archive.to_str().unwrap(),
                "--noconfirm",
                "--trust-hooks",
            ],
        ),
        "transaction rolled back",
    );
    assert!(!root.path().join("tracked").exists());
    assert!(!root.path().join("hook-log").exists());
}

#[test]
fn lifecycle_hooks_install_upgrade_failure_remove_and_rollback() {
    if !namespace_test("lifecycle_hooks_install_upgrade_failure_remove_and_rollback") {
        return;
    }
    let root = tempdir().unwrap();
    copy_shell(root.path());
    let (_build, archive) = hooks_package("1.0.0", false);
    success(maple(
        root.path(),
        &[
            "install",
            archive.to_str().unwrap(),
            "--noconfirm",
            "--trust-hooks",
        ],
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("hook-log")).unwrap(),
        "pre-install 1.0.0 \npost-install 1.0.0 \n"
    );
    let (_bad, bad) = hooks_package("2.0.0", true);
    failure(
        maple(
            root.path(),
            &[
                "install",
                bad.to_str().unwrap(),
                "--noconfirm",
                "--trust-hooks",
            ],
        ),
        "transaction rolled back",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("tracked")).unwrap(),
        "1.0.0"
    );
    assert!(record(root.path(), "scripted").contains("1.0.0"));
    let (_good, good) = hooks_package("2.0.0", false);
    success(maple(
        root.path(),
        &[
            "install",
            good.to_str().unwrap(),
            "--noconfirm",
            "--trust-hooks",
        ],
    ));
    failure(
        maple(root.path(), &["remove", "scripted", "--noconfirm"]),
        "--trust-hooks",
    );
    success(maple(
        root.path(),
        &["remove", "scripted", "--noconfirm", "--trust-hooks"],
    ));
    assert!(!root.path().join("tracked").exists());
    let log = fs::read_to_string(root.path().join("hook-log")).unwrap();
    assert!(log.contains("pre-upgrade 2.0.0 1.0.0\npost-upgrade 2.0.0 1.0.0"));
    assert!(log.ends_with("pre-remove 2.0.0 \npost-remove 2.0.0 \n"));
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(
        fs::read_to_string(root.path().join("tracked")).unwrap(),
        "2.0.0"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("hook-log")).unwrap(),
        log
    ); // never replay scripts
}

fn arch_fixture(path: &Path, version: &str, extra: &str, script: Option<&str>) {
    let encoder = zstd::stream::write::Encoder::new(fs::File::create(path).unwrap(), 1).unwrap();
    let mut tar = tar::Builder::new(encoder);
    let append = |tar: &mut tar::Builder<zstd::stream::write::Encoder<'_, fs::File>>,
                  name: &str,
                  bytes: &[u8],
                  mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(bytes.len() as u64);
        h.set_mode(mode);
        h.set_cksum();
        tar.append_data(&mut h, name, bytes).unwrap();
    };
    append(&mut tar,".PKGINFO",format!("pkgname = arch-tool\npkgver = {version}\npkgdesc = Fixture\nbackup = etc/tool.conf\nprovides = virtual-tool=2:1-1\nprovides = unversioned\n{extra}").as_bytes(),0o644);
    if let Some(script) = script {
        append(&mut tar, ".INSTALL", script.as_bytes(), 0o644);
    }
    append(&mut tar, "etc/tool.conf", b"default\n", 0o640);
    tar.append_pax_extensions([("SCHILY.xattr.user.binary", b"\0\n\xff".as_slice())])
        .unwrap();
    append(
        &mut tar,
        "usr/bin/tool",
        b"#!/bin/sh\necho converted\n",
        0o755,
    );
    let mut h = tar::Header::new_gnu();
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_mode(0o777);
    h.set_entry_type(tar::EntryType::Symlink);
    h.set_size(0);
    tar.append_link(&mut h, "usr/bin/alias", "tool").unwrap();
    h.set_entry_type(tar::EntryType::Link);
    tar.append_link(&mut h, "usr/bin/hard", "usr/bin/tool")
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn arch_conversion_repository_install_upgrade_config_and_remove() {
    let root = tempdir().unwrap();
    let build = tempdir().unwrap();
    let repo = tempdir().unwrap();
    let input = build.path().join("arch.pkg.tar.zst");
    // A native package with a non-SemVer version satisfies a converted Arch
    // dependency through the same comparator and resolver.
    publish(repo.path(), "runtime", "1.9", "{}");
    publish(repo.path(), "runtime", "1.10", "{}");
    let mut entries = fs::read_to_string(repo.path().join("repository.toml")).unwrap();
    let mut hashes = String::from("\n[sha256]\n");
    for version in ["2:1.9-1", "2:1.10-2"] {
        arch_fixture(&input, version, "depend = runtime>=1.9\n", None);
        let output = repo
            .path()
            .join(format!("packages/arch-tool/{version}.maple"));
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        let stdout = success(
            Command::new(env!("CARGO_BIN_EXE_arch-to-maple"))
                .arg(&input)
                .arg(&output)
                .output()
                .unwrap(),
        );
        let (entry, hash) = stdout.split_once("[sha256]\n").unwrap();
        entries.push_str(entry);
        hashes.push_str(hash);
    }
    write(
        repo.path().join("repository.toml"),
        format!("{entries}{hashes}"),
    );
    let server = Server::start(repo.path());
    success(maple(
        root.path(),
        &[
            "--repo",
            &server.url(),
            "install",
            "arch-tool=2:1.9-1",
            "--noconfirm",
        ],
    ));
    assert!(record(root.path(), "runtime").contains("version = \"1.10\""));
    assert_eq!(
        get_xattr(&root.path().join("usr/bin/tool"), "user.binary"),
        b"\0\n\xff"
    );
    assert_eq!(
        fs::metadata(root.path().join("usr/bin/tool"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(root.path().join("usr/bin/tool"))
            .unwrap()
            .ino(),
        fs::metadata(root.path().join("usr/bin/hard"))
            .unwrap()
            .ino()
    );
    assert_eq!(
        fs::read_link(root.path().join("usr/bin/alias")).unwrap(),
        Path::new("tool")
    );
    write(root.path().join("etc/tool.conf"), "edited");
    success(maple(
        root.path(),
        &["--repo", &server.url(), "update", "--noconfirm"],
    ));
    assert!(record(root.path(), "arch-tool").contains("2:1.10-2"));
    assert_eq!(
        fs::read_to_string(root.path().join("etc/tool.conf.maple-new")).unwrap(),
        "default\n"
    );
    success(maple(root.path(), &["remove", "arch-tool", "--noconfirm"]));
    assert!(!root.path().join("usr/bin/tool").exists());
    assert_eq!(
        fs::read_to_string(root.path().join("etc/tool.conf")).unwrap(),
        "edited"
    );
}

#[test]
fn arch_converter_rejects_unsupported_metadata_without_output_and_never_executes_scripts() {
    let build = tempdir().unwrap();
    let input = build.path().join("arch.pkg.tar.zst");
    let output = build.path().join("out.maple");
    arch_fixture(&input, "1-1", "replaces = obsolete\n", None);
    failure(
        Command::new(env!("CARGO_BIN_EXE_arch-to-maple"))
            .arg(&input)
            .arg(&output)
            .output()
            .unwrap(),
        "unsupported Arch replaces",
    );
    assert!(!output.exists());
    let sentinel = build.path().join("executed");
    arch_fixture(
        &input,
        "1-1",
        "",
        Some(&format!(
            "echo bad > '{}'\npost_install() {{ :; }}",
            sentinel.display()
        )),
    );
    success(
        Command::new(env!("CARGO_BIN_EXE_arch-to-maple"))
            .arg(&input)
            .arg(&output)
            .output()
            .unwrap(),
    );
    assert!(!sentinel.exists());
    let p = maple::package::Prepared::open(&output).unwrap();
    assert_eq!(p.record.hook_scripts.len(), 6);
    assert!(p.record.package.dependencies.contains_key("bash"));
}

#[test]
fn converted_capabilities_and_arch_install_scripts_work_in_target() {
    if !namespace_test("converted_capabilities_and_arch_install_scripts_work_in_target") {
        return;
    }
    let root = tempdir().unwrap();
    let build = tempdir().unwrap();
    copy_executable(root.path(), Path::new("/bin/bash"), "bin/bash");
    let (_bash, bash) = package("bash", "1.0.0", &[]);
    success(maple(
        root.path(),
        &["install", bash.to_str().unwrap(), "--noconfirm"],
    ));
    let input = build.path().join("source.pkg.tar.zst");
    let output = build.path().join("source.maple");
    let cap = [1, 0, 0, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut tar = tar::Builder::new(
        zstd::stream::write::Encoder::new(fs::File::create(&input).unwrap(), 1).unwrap(),
    );
    for (name, contents) in [
        (
            ".PKGINFO",
            "pkgname = caps\npkgver = 1:2.0-1\ndepend = bash\n",
        ),
        (
            ".INSTALL",
            "pre_install() { [ \"$1\" = '1:2.0-1' ]; }\npost_install() { [ -f /usr/bin/cap-tool ] && echo installed > /converted-hook; }\npost_remove() { [ ! -f /usr/bin/cap-tool ] && echo removed >> /converted-hook; }\n",
        ),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mtime(0);
        h.set_size(contents.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, name, contents.as_bytes()).unwrap();
    }
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(cap);
    tar.append_pax_extensions([("LIBARCHIVE.xattr.security.capability", encoded.as_bytes())])
        .unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_size(1);
    h.set_mode(0o755);
    h.set_cksum();
    tar.append_data(&mut h, "usr/bin/cap-tool", b"x".as_slice())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    success(
        Command::new(env!("CARGO_BIN_EXE_arch-to-maple"))
            .arg(&input)
            .arg(&output)
            .output()
            .unwrap(),
    );
    success(maple(
        root.path(),
        &[
            "install",
            output.to_str().unwrap(),
            "--trust-hooks",
            "--noconfirm",
        ],
    ));
    assert_eq!(
        &get_xattr(&root.path().join("usr/bin/cap-tool"), "security.capability")[..20],
        cap
    );
    assert_eq!(
        fs::read_to_string(root.path().join("converted-hook")).unwrap(),
        "installed\n"
    );
    success(maple(
        root.path(),
        &["remove", "caps", "--trust-hooks", "--noconfirm"],
    ));
    assert_eq!(
        fs::read_to_string(root.path().join("converted-hook")).unwrap(),
        "installed\nremoved\n"
    );
    success(maple(root.path(), &["rollback", "--noconfirm"]));
    assert_eq!(
        &get_xattr(&root.path().join("usr/bin/cap-tool"), "security.capability")[..20],
        cap
    );
}

#[test]
fn malformed_hook_declarations_and_interpreters_are_rejected() {
    let root = tempdir().unwrap();
    let (build, _) = package("bad", "1.0.0", &[]);
    let missing = with_metadata(build.path(), "hooks = [\"pre-install\"]\n");
    failure(
        maple(
            root.path(),
            &[
                "install",
                missing.to_str().unwrap(),
                "--trust-hooks",
                "--noconfirm",
            ],
        ),
        "declared hooks must exactly match",
    );
    let (_hookbuild, archive) = hooks_package("1.0.0", false);
    let mut prepared = maple::package::Prepared::open(&archive).unwrap();
    prepared.record.hook_scripts.insert(
        maple::hooks::Hook::PreInstall,
        "#!/usr/bin/python\npass\n".into(),
    );
    assert!(
        maple::hooks::validate_script(
            &prepared.record.hook_scripts[&maple::hooks::Hook::PreInstall]
        )
        .is_err()
    );
}

#[test]
fn native_alpm_versions_exact_pins_and_prerelease_update_rules() {
    let root = tempdir().unwrap();
    let repo = tempdir().unwrap();
    publish(repo.path(), "native", "1.0.0-rc.1", "{}");
    publish(repo.path(), "native", "1.0.0", "{}");
    publish(repo.path(), "native", "0:1.0.0", "{}");
    let server = Server::start(repo.path());
    let invoke = |args: &[&str]| {
        let mut command = vec!["--repo"];
        let url = server.url();
        command.push(&url);
        command.extend(args);
        command.push("--noconfirm");
        maple(root.path(), &command)
    };
    success(invoke(&["install", "native=1.0.0-rc.1"]));
    assert!(success(invoke(&["update"])).contains("up to date")); // missing pkgrel compares equal
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/native")).unwrap(),
        "1.0.0-rc.1"
    );
    // Equal under ALPM, but exact pins distinguish all three original strings.
    for version in ["1.0.0", "0:1.0.0", "1.0.0-rc.1"] {
        success(invoke(&["install", &format!("native={version}")]));
        let installed: maple::model::Installed =
            toml::from_str(&record(root.path(), "native")).unwrap();
        assert_eq!(installed.package.version.0, version);
    }
    publish(repo.path(), "native", "1.0.0+build.9", "{}");
    success(invoke(&["update"])); // +build has ALPM significance
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/native")).unwrap(),
        "1.0.0+build.9"
    );
    success(invoke(&["rollback"]));
    assert_eq!(
        fs::read_to_string(root.path().join("usr/bin/native")).unwrap(),
        "1.0.0-rc.1"
    );
    failure(
        invoke(&["install", "native=1.0.0-rc.01"]),
        "package not found",
    );
}
