//! Public API and ordinary temporary-filesystem hygiene only. These tests do
//! not enable a snapshot backend or claim filesystem immutability.
#![cfg(target_os = "linux")]
use backupsage::borg::{
    ArchiveName, Error, PrivateState, RegularFilePath, UnsupportedBackend, ValidationBackend,
};
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn unsupported_backend_refuses_without_borg_access() {
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        UnsupportedBackend.validate(d.path()),
        Err(Error::UnsupportedBackend)
    ));
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 0);
}

#[test]
fn literal_inputs_refuse_path_lists_options_and_noncanonical_paths() {
    for a in [
        "",
        "r::a",
        "{now}",
        "*",
        "a?",
        "a/b",
        "a.checkpoint",
        "a.checkpoint.1",
        "a\n--repair",
    ] {
        assert!(ArchiveName::parse(a).is_err());
    }
    for p in [
        "", "/abs", "../x", "x/../y", "x//y", "x/", "x?", "a\0b", "a\nb",
    ] {
        assert!(RegularFilePath::parse(p).is_err());
    }
    // Option-looking data stays one literal field, behind -- and/or pf:.
    assert!(ArchiveName::parse("--repair").is_ok());
    assert!(RegularFilePath::parse("--strip-components=2").is_ok());
}

#[test]
fn private_state_real_modes_persistence_and_no_clobber() {
    let d = tempfile::tempdir().unwrap();
    let keys = d.path().join("keys");
    fs::create_dir(&keys).unwrap();
    let key = keys.join("synthetic-key");
    fs::write(&key, b"synthetic key input").unwrap();
    fs::set_permissions(&key, fs::Permissions::from_mode(0o400)).unwrap();
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o500)).unwrap();
    let root = d.path().join("state");
    let db = d.path().join("index.db");
    let state = PrivateState::create(&root, &keys, None, &[], &[&db], &[]).unwrap();
    state.create_metadata_file("marker", b"metadata").unwrap();
    assert!(state
        .create_metadata_file("marker", b"replacement")
        .is_err());
    drop(state);
    let _state = PrivateState::create(&root, &keys, None, &[], &[&db], &[]).unwrap();
    for name in ["", "base", "cache", "security"] {
        assert_eq!(
            fs::metadata(root.join(name)).unwrap().permissions().mode() & 0o7777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(root.join("base/marker"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    assert_eq!(fs::read(root.join("base/marker")).unwrap(), b"metadata");
    assert_eq!(
        fs::metadata(&keys).unwrap().permissions().mode() & 0o7777,
        0o500
    );
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o7777,
        0o400
    );
}

#[test]
fn private_state_rejects_modes_symlinks_hardlinks_and_containment() {
    let d = tempfile::tempdir().unwrap();
    let keys = d.path().join("keys");
    fs::create_dir(&keys).unwrap();
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o500)).unwrap();
    let source = d.path().join("source");
    fs::create_dir(&source).unwrap();
    assert!(
        PrivateState::create(&source.join("state"), &keys, None, &[&source], &[], &[]).is_err()
    );
    assert!(!source.join("state").exists());
    let root = d.path().join("state");
    let state = PrivateState::create(&root, &keys, None, &[&source], &[], &[]).unwrap();
    fs::write(source.join("input"), b"synthetic").unwrap();
    fs::hard_link(source.join("input"), root.join("base/alias")).unwrap();
    assert!(state.create_metadata_file("blocked", b"x").is_err());
    fs::remove_file(root.join("base/alias")).unwrap();
    std::os::unix::fs::symlink(&source, root.join("cache/link")).unwrap();
    assert!(state.create_metadata_file("blocked", b"x").is_err());
    fs::remove_file(root.join("cache/link")).unwrap();
    fs::set_permissions(root.join("cache"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(state.create_metadata_file("blocked", b"x").is_err());
}

#[test]
fn private_state_rejects_key_credential_db_aliases_and_sidecars() {
    let d = tempfile::tempdir().unwrap();
    let keys = d.path().join("keys");
    fs::create_dir(&keys).unwrap();
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o500)).unwrap();
    let db = d.path().join("index.db");
    fs::write(&db, b"synthetic").unwrap();
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    let credential = d.path().join("credential");
    fs::hard_link(&db, &credential).unwrap();
    assert!(PrivateState::create(
        &d.path().join("state"),
        &keys,
        Some(&credential),
        &[],
        &[&db],
        &[]
    )
    .is_err());
    assert!(PrivateState::create(
        &d.path().join("index.db-wal"),
        &keys,
        None,
        &[],
        &[&db],
        &[]
    )
    .is_err());
    assert!(PrivateState::create(&keys, &keys, None, &[], &[], &[]).is_err());
}
