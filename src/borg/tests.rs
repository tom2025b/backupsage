//! Mock-backed tests are API/error/process plumbing ONLY on writable temp
//! directories. None proves filesystem immutability or backend enablement.
use super::capability::MockBackend;
use super::*;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    dir: tempfile::TempDir,
    source: PathBuf,
    state: PrivateState,
    credential: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("repo");
        fs::create_dir(&source).unwrap();
        let keys = dir.path().join("keys");
        fs::create_dir(&keys).unwrap();
        fs::set_permissions(&keys, fs::Permissions::from_mode(0o500)).unwrap();
        let credential = dir.path().join("private credential");
        fs::write(&credential, b"SENTINEL_SYNTHETIC_SECRET_82\n").unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        let state = PrivateState::create(
            &dir.path().join("state"),
            &keys,
            Some(&credential),
            &[&source],
            &[&dir.path().join("index.db")],
            &[],
        )
        .unwrap();
        Self {
            dir,
            source,
            state,
            credential,
        }
    }
    fn env(&self, encrypted: bool) -> BorgEnvironment<'_> {
        let inherited = if encrypted {
            vec![("BORG_PASSCOMMAND".into(), self.passcommand().into())]
        } else {
            vec![]
        };
        BorgEnvironment::from_inherited(&self.state, inherited).unwrap()
    }

    fn passcommand(&self) -> String {
        let helper = self.dir.path().join("credential-helper.py");
        fs::write(
            &helper,
            include_bytes!("../../tests/common/borg_credential.py"),
        )
        .unwrap();
        format!(
            "/usr/bin/python3 '{}' '{}' '{}'",
            helper.display(),
            self.credential.display(),
            self.state.base.join("helper-argv").display()
        )
    }
}

#[test]
fn environment_exact_finite_map_and_secret_refusals() {
    let f = Fixture::new();
    let inherited = [
        "HOME",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "PYTHONPATH",
        "BORG_REPO",
        "BORG_UNKNOWN_FUTURE",
        "BORG_LOGGING_CONF",
        "SSH_AUTH_SOCK",
        "BORG_RSH",
        "BORG_RELOCATED_REPO_ACCESS_IS_OK",
        "BORG_UNKNOWN_UNENCRYPTED_REPO_ACCESS_IS_OK",
        "BORG_CHECK_I_KNOW_WHAT_I_AM_DOING",
        "PATH",
        "LC_ALL",
        "TZ",
    ];
    let env =
        BorgEnvironment::from_inherited(&f.state, inherited.map(|k| (k.into(), "poison".into())))
            .unwrap();
    let expected: BTreeMap<OsString, OsString> = [
        ("PATH", OsString::from("/usr/bin:/bin")),
        ("LC_ALL", "C.UTF-8".into()),
        ("TZ", "UTC".into()),
        ("BORG_BASE_DIR", f.state.base.as_os_str().to_owned()),
        ("BORG_CACHE_DIR", f.state.cache.as_os_str().to_owned()),
        ("BORG_SECURITY_DIR", f.state.security.as_os_str().to_owned()),
        ("BORG_KEYS_DIR", f.state.keys.as_os_str().to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v))
    .collect();
    assert_eq!(env.as_map(), &expected);
    let with_helper = f.env(true);
    assert_eq!(with_helper.as_map().len(), 8);
    assert_eq!(
        with_helper
            .as_map()
            .get(&OsString::from("BORG_PASSCOMMAND"))
            .unwrap(),
        &OsString::from(f.passcommand())
    );
    for k in [
        "BORG_PASSPHRASE",
        "BORG_NEW_PASSPHRASE",
        "BORG_PASSPHRASE_FD",
    ] {
        let error =
            BorgEnvironment::from_inherited(&f.state, [(k.into(), "SENTINEL_SECRET_VALUE".into())])
                .err()
                .unwrap();
        assert_eq!(error, Error::ConflictingSecretEnvironment);
        for rendered in [
            format!("{error}"),
            format!("{error:?}"),
            format!("{error:#?}"),
        ] {
            assert!(!rendered.contains("SENTINEL_SECRET_VALUE"));
        }
        assert!(std::error::Error::source(&error).is_none());
    }
}

/// MockBackend API/error plumbing ONLY; does not prove filesystem immutability.
#[test]
fn mock_backend_exact_argv_and_detached_argument_injection() {
    let f = Fixture::new();
    let cap = MockBackend.validate(&f.source).unwrap();
    let repo = Operation::repository_metadata(&cap);
    assert_eq!(
        repo.argv(),
        ["list", "--json", "--bypass-lock", "--", "/proc/self/fd/9"]
    );
    let archive = Operation::archive_entries(&cap, ArchiveName::parse("daily").unwrap());
    assert_eq!(
        archive.argv(),
        [
            "list",
            "--json-lines",
            "--consider-part-files",
            "--bypass-lock",
            "--format",
            "{type}{mode}{uid}{gid}{size}{isomtime}{archiveid}{archivename}",
            "--",
            "/proc/self/fd/9::daily"
        ]
    );
    let extract = Operation::extract_file(
        &cap,
        ArchiveName::parse("--delete").unwrap(),
        RegularFilePath::parse("--output=evil file").unwrap(),
    );
    assert_eq!(
        extract.argv(),
        [
            "extract",
            "--stdout",
            "--consider-part-files",
            "--bypass-lock",
            "--",
            "/proc/self/fd/9::--delete",
            "pf:--output=evil file"
        ]
    );
    let mut injected = repo.argv();
    injected.push("--repair".into());
    assert_eq!(repo.argv().len(), 5);
    assert!(matches!(
        Runtime.list(
            &extract,
            &f.env(false),
            &Limits::default(),
            &Cancellation::default()
        ),
        Err(Error::InvalidInput)
    ));
}

fn fake(f: &Fixture, scenario: &str) -> PathBuf {
    fs::write(f.source.join("scenario"), scenario).unwrap();
    let exe = f.dir.path().join("fake-borg");
    fs::write(&exe, include_bytes!("../../tests/common/borg_hostile.py")).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}
fn assert_reaped(f: &Fixture, count: usize) {
    let pids: Vec<i32> = fs::read_to_string(f.source.join("pids"))
        .unwrap()
        .lines()
        .map(|s| s.parse().unwrap())
        .collect();
    assert_eq!(
        pids.len(),
        count,
        "the hostile process tree must actually have run"
    );
    for pid in pids {
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "process {pid} still exists (including zombie)"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
fn fake_run(
    f: &Fixture,
    scenario: &str,
    limits: &Limits,
    cancellation: &Cancellation,
    consume: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let cap = MockBackend.validate(&f.source).unwrap();
    process::run(
        &fake(f, scenario),
        &Operation::repository_metadata(&cap).argv(),
        &f.env(false),
        cap.pin(),
        limits,
        cancellation,
        limits.listing_bytes as u64,
        consume,
    )
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_stderr_saturation_and_nonzero_sanitization() {
    for (scenario, expected) in [("stderr", Ok(())), ("nonzero", Err(Error::ChildFailed))] {
        let f = Fixture::new();
        let mut output = Vec::new();
        let result = fake_run(
            &f,
            scenario,
            &Limits::default(),
            &Cancellation::default(),
            &mut |b| {
                output.extend_from_slice(b);
                Ok(())
            },
        );
        assert_eq!(result, expected);
        assert!(!format!("{result:?}").contains("SENTINEL_PRIVATE_DIAGNOSTIC"));
        assert!(!String::from_utf8_lossy(&output).contains("SENTINEL_PRIVATE_DIAGNOSTIC"));
        if scenario == "stderr" {
            assert_eq!(output, b"healthy stdout");
        }
        assert_reaped(&f, 1);
    }
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_timeout_kills_and_reaps_sigterm_ignoring_descendants() {
    let f = Fixture::new();
    let start = Instant::now();
    let limits = Limits {
        timeout: Duration::from_millis(600),
        ..Limits::default()
    };
    assert_eq!(
        fake_run(
            &f,
            "timeout",
            &limits,
            &Cancellation::default(),
            &mut |_| Ok(())
        ),
        Err(Error::Timeout)
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_reaped(&f, 3);
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_cancellation_kills_and_reaps_hostile_descendants() {
    let f = Fixture::new();
    let cancellation = Cancellation::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            while fs::read_to_string(f.source.join("pids"))
                .map(|s| s.lines().count())
                .unwrap_or(0)
                != 3
            {
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(5));
            }
            cancellation.cancel();
        });
        assert_eq!(
            fake_run(
                &f,
                "cancel",
                &Limits::default(),
                &cancellation,
                &mut |_| Ok(())
            ),
            Err(Error::Cancelled)
        );
    });
    assert_reaped(&f, 3);
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_early_exit_and_parser_failure_reap_descendants() {
    for (scenario, expected) in [
        ("early_exit", Error::ChildFailed),
        ("parser_failure", Error::Consumer),
    ] {
        let f = Fixture::new();
        assert_eq!(
            fake_run(
                &f,
                scenario,
                &Limits::default(),
                &Cancellation::default(),
                &mut |_| Err(Error::InvalidInput)
            ),
            Err(expected)
        );
        assert_reaped(&f, 3);
    }
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_consumer_panic_guard_reaps_descendants() {
    let f = Fixture::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fake_run(
            &f,
            "panic",
            &Limits::default(),
            &Cancellation::default(),
            &mut |_| panic!("synthetic consumer panic"),
        )
    }));
    assert!(result.is_err());
    assert_reaped(&f, 3);
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_bounded_listing_and_streamed_stdout() {
    let f = Fixture::new();
    let mut chunks = 0;
    let mut total = 0;
    let limits = Limits {
        listing_bytes: 64 * 1024 * 1024,
        ..Limits::default()
    };
    fake_run(&f, "stream", &limits, &Cancellation::default(), &mut |b| {
        assert!(b.len() <= 65536);
        chunks += 1;
        total += b.len();
        Ok(())
    })
    .unwrap();
    assert_eq!(total, 32 * 1024 * 1024);
    assert!(chunks >= 512);
    assert_reaped(&f, 1);
    let f = Fixture::new();
    let limits = Limits {
        listing_bytes: 100,
        ..Limits::default()
    };
    assert_eq!(
        fake_run(
            &f,
            "stdout_limit",
            &limits,
            &Cancellation::default(),
            &mut |_| Ok(())
        ),
        Err(Error::OutputLimit)
    );
    assert_reaped(&f, 1);
}

/// MockBackend API/error/process plumbing ONLY; no filesystem immutability proof.
#[test]
fn mock_backend_null_stdin_no_tty_finite_env_and_closed_unneeded_fds() {
    let f = Fixture::new();
    let mut bytes = Vec::new();
    let leak = fs::File::open(&f.credential).unwrap();
    let leaked_fd = unsafe { libc::fcntl(leak.as_raw_fd(), libc::F_DUPFD, 100) };
    assert!(leaked_fd >= 100);
    fake_run(
        &f,
        "hygiene",
        &Limits::default(),
        &Cancellation::default(),
        &mut |b| {
            bytes.extend_from_slice(b);
            Ok(())
        },
    )
    .unwrap();
    unsafe {
        libc::close(leaked_fd);
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["stdin"], "");
    assert_eq!(v["tty"], false);
    assert_eq!(v["fds"], serde_json::json!([9]));
    let expected: BTreeMap<String, String> = f
        .env(false)
        .as_map()
        .iter()
        .map(|(k, v)| (k.to_str().unwrap().into(), v.to_str().unwrap().into()))
        .collect();
    assert_eq!(v["env"], serde_json::to_value(expected).unwrap());
    assert_reaped(&f, 1);
}

// Fixture authoring is deliberately outside the sealed production API. It uses
// only synthetic data and finite temp state. The same FD locator avoids an
// automatic relocation override; init/create establish operator-managed trust.
fn author(f: &Fixture, cap: &VerifiedImmutableSnapshot, encrypted: bool) {
    let seed = f.dir.path().join("seed");
    fs::create_dir(&seed).unwrap();
    fs::write(seed.join("file.txt"), b"synthetic archive content\n").unwrap();
    fs::write(seed.join("file.txt.extra"), b"must not be extracted\n").unwrap();
    let env = f.env(encrypted);
    for args in [
        vec![
            "init",
            if encrypted {
                "--encryption=repokey"
            } else {
                "--encryption=none"
            },
            "/proc/self/fd/9",
        ],
        vec![
            "create",
            "/proc/self/fd/9::daily",
            "file.txt",
            "file.txt.extra",
        ],
    ] {
        let mut cmd = Command::new("/usr/bin/borg");
        cmd.args(args)
            .env_clear()
            .envs(env.as_map())
            .current_dir(&seed)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let pin = cap.pin().as_raw_fd();
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(pin, 9) < 0
                    || libc::fcntl(9, libc::F_SETFD, 0) < 0
                    || libc::setsid() < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                libc::umask(0o077);
                Ok(())
            });
        }
        assert!(
            cmd.status()
                .expect("installed Borg fixture authoring")
                .success(),
            "synthetic fixture authoring failed (diagnostics suppressed)"
        );
    }
}

fn real_roundtrip(encrypted: bool) {
    let f = Fixture::new();
    let cap = MockBackend.validate(&f.source).unwrap();
    author(&f, &cap, encrypted);
    let env = f.env(encrypted);
    let cancel = Cancellation::default();
    let limits = Limits::default();
    for _ in 0..2 {
        let repo = Runtime
            .list(
                &Operation::repository_metadata(&cap),
                &env,
                &limits,
                &cancel,
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&repo).unwrap();
        assert_eq!(value["archives"][0]["name"], "daily");
        let items = Runtime
            .list(
                &Operation::archive_entries(&cap, ArchiveName::parse("daily").unwrap()),
                &env,
                &limits,
                &cancel,
            )
            .unwrap();
        let text = std::str::from_utf8(&items).unwrap();
        let entries: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        assert!(entries
            .iter()
            .all(|e| e["archiveid"] == value["archives"][0]["id"]));
        let mut content = Vec::new();
        let operation = Operation::extract_file(
            &cap,
            ArchiveName::parse("daily").unwrap(),
            RegularFilePath::parse("file.txt").unwrap(),
        );
        Runtime
            .extract(&operation, &env, &limits, &cancel, |b| {
                content.extend_from_slice(b);
                Ok(())
            })
            .unwrap();
        assert_eq!(content, b"synthetic archive content\n");
        for bytes in [&repo, &items, &content] {
            assert!(!String::from_utf8_lossy(bytes).contains("SENTINEL_SYNTHETIC_SECRET_82"));
        }
        assert!(!format!("{:?}", operation.argv()).contains("SENTINEL_SYNTHETIC_SECRET_82"));
        f.state.validate().unwrap();
    }
    assert!(fs::read_dir(&f.state.security).unwrap().next().is_some());
    if encrypted {
        let missing = f.env(false);
        let failed = BorgEnvironment::from_inherited(
            &f.state,
            [(
                "BORG_PASSCOMMAND".into(),
                format!("{} --fail", f.passcommand()).into(),
            )],
        )
        .unwrap();
        for env in [missing, failed] {
            let result = Runtime.list(
                &Operation::repository_metadata(&cap),
                &env,
                &limits,
                &cancel,
            );
            assert_eq!(result, Err(Error::ChildFailed));
            assert!(!format!("{result:?}").contains("SENTINEL_SYNTHETIC_SECRET_82"));
        }
        let invocations = fs::read_to_string(f.state.base.join("helper-argv")).unwrap();
        assert!(!invocations.contains("SENTINEL_SYNTHETIC_SECRET_82"));
        assert!(
            invocations.lines().count() >= 8,
            "real helper must actually execute"
        );
        for line in invocations.lines() {
            let argv: Vec<String> = serde_json::from_str(line).unwrap();
            assert_eq!(Path::new(&argv[1]), f.credential);
            assert_eq!(Path::new(&argv[2]), f.state.base.join("helper-argv"));
        }
    }
}

/// MockBackend API/error/real Borg plumbing ONLY; an ordinary writable fixture
/// proves trust-warning refusal, NOT filesystem immutability or confinement.
#[test]
fn mock_backend_unknown_unencrypted_trust_is_not_auto_initialized() {
    let f = Fixture::new();
    let cap = MockBackend.validate(&f.source).unwrap();
    author(&f, &cap, false);
    // A second genuinely new private profile has never trusted this repo. Keep
    // the authoring profile intact: do not erase/replace established security.
    let fresh = PrivateState::create(
        &f.dir.path().join("uninitialized-state"),
        &f.state.keys,
        Some(&f.credential),
        &[&f.source],
        &[],
        &[],
    )
    .unwrap();
    let env = BorgEnvironment::from_inherited(
        &fresh,
        [
            (
                "BORG_UNKNOWN_UNENCRYPTED_REPO_ACCESS_IS_OK".into(),
                "yes".into(),
            ),
            ("BORG_RELOCATED_REPO_ACCESS_IS_OK".into(), "yes".into()),
        ],
    )
    .unwrap();
    assert_eq!(
        Runtime.list(
            &Operation::repository_metadata(&cap),
            &env,
            &Limits::default(),
            &Cancellation::default()
        ),
        Err(Error::ChildFailed)
    );
    assert!(fs::read_dir(&f.state.security).unwrap().next().is_some());
}

/// MockBackend API/error/REAL Borg 1.4.4 plumbing ONLY on an ordinary writable
/// unencrypted temp repository. This explicitly does NOT prove immutability.
#[test]
fn mock_backend_real_borg_144_unencrypted_happy_path() {
    real_roundtrip(false);
}

/// MockBackend API/error/REAL Borg 1.4.4 plumbing ONLY on an ordinary writable
/// encrypted temp repository with synthetic credentials. This explicitly does
/// NOT prove filesystem immutability or helper filesystem containment.
#[test]
fn mock_backend_real_borg_144_encrypted_passcommand_happy_path() {
    real_roundtrip(true);
}
