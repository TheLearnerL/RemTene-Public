//! Brand-owned local-path migration for the RemTene rename.
//!
//! ADR-0008 deliberately keeps the Tauri Bundle ID and encrypted-store ABI,
//! but new brand-owned subdirectories use `RemTene`. Existing user data is
//! durably copied forward without overwriting anything already written by
//! RemTene. Once every destination is ready, legacy data copies are removed;
//! only the old instance-lock path remains for cross-version exclusion.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

pub(crate) const STORAGE_DIRECTORY: &str = "RemTene";
pub(crate) const LEGACY_STORAGE_DIRECTORY: &str = "Bard";
pub(crate) const INSTANCE_LEASE_FILE: &str = "instance.lock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DesktopStoragePaths {
    data_root: PathBuf,
    cache_root: PathBuf,
    legacy_data_root: PathBuf,
}

impl DesktopStoragePaths {
    pub(crate) fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub(crate) fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    #[cfg(test)]
    pub(crate) fn legacy_data_root(&self) -> &Path {
        &self.legacy_data_root
    }
}

pub(crate) fn prepare_desktop_storage(
    app_data_base: &Path,
    app_cache_base: &Path,
) -> io::Result<DesktopStoragePaths> {
    let data_root = app_data_base.join(STORAGE_DIRECTORY);
    let cache_root = app_cache_base.join(STORAGE_DIRECTORY);
    let legacy_data_root = app_data_base.join(LEGACY_STORAGE_DIRECTORY);
    let legacy_cache_root = app_cache_base.join(LEGACY_STORAGE_DIRECTORY);

    let legacy_data_exists = entry_exists(&legacy_data_root)?;
    if legacy_data_exists {
        require_real_directory(&legacy_data_root)?;
        validate_real_tree(&legacy_data_root, "legacy storage migration")?;
    }
    let legacy_cache_exists = entry_exists(&legacy_cache_root)?;
    if legacy_cache_exists {
        require_real_directory(&legacy_cache_root)?;
        validate_real_tree(&legacy_cache_root, "legacy cache cleanup")?;
    }

    ensure_real_directory(&data_root)?;
    if legacy_data_exists {
        migrate_persistent_entries(&legacy_data_root, &data_root)?;
    }
    ensure_real_directory(&cache_root)?;
    if legacy_cache_exists {
        discard_legacy_cache(&legacy_cache_root)?;
    }

    Ok(DesktopStoragePaths {
        data_root,
        cache_root,
        legacy_data_root,
    })
}

fn discard_legacy_cache(path: &Path) -> io::Result<()> {
    if !entry_exists(path)? {
        return Ok(());
    }
    require_real_directory(path)?;
    validate_real_tree(path, "legacy cache cleanup")?;
    remove_real_entry(path, "legacy cache cleanup")
}

pub(crate) fn real_directory_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn migrate_persistent_entries(source: &Path, destination: &Path) -> io::Result<()> {
    validate_real_tree(source, "legacy storage migration")?;

    for entry in sorted_entries(source)? {
        if entry.file_name() == INSTANCE_LEASE_FILE {
            require_regular_file(&entry.path(), "legacy instance lease")?;
            continue;
        }
        let destination_entry = destination.join(entry.file_name());
        if entry_exists(&destination_entry)? {
            // A current-brand value always wins. Mixing two SQLite stores,
            // master keys or settings generations would produce an invalid
            // application state. Verify and persist it before retiring the
            // conflicting legacy copy.
            validate_real_tree(&destination_entry, "current storage migration target")?;
            sync_real_tree(&destination_entry, "current storage migration target")?;
            continue;
        }
        copy_entry_atomically(&entry.path(), &destination_entry)?;
    }

    sync_directory(destination)?;

    // Revalidate the complete source before deleting any legacy copy. The
    // process holds the legacy instance lease, so a supported old application
    // cannot mutate this tree while migration is running.
    validate_real_tree(source, "legacy storage cleanup")?;
    for entry in sorted_entries(source)? {
        if entry.file_name() == INSTANCE_LEASE_FILE {
            require_regular_file(&entry.path(), "legacy instance lease")?;
            continue;
        }
        remove_real_entry(&entry.path(), "legacy storage cleanup")?;
    }
    sync_directory(source)?;
    Ok(())
}

fn copy_entry_atomically(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "legacy storage migration refuses symbolic links",
        ));
    }

    let temporary = temporary_sibling(destination)?;
    remove_reserved_temporary(&temporary)?;
    let copied = if metadata.is_file() {
        copy_regular_file(source, &temporary, &metadata)
    } else if metadata.is_dir() {
        copy_directory(source, &temporary, &metadata)
    } else {
        Err(io::Error::other(
            "legacy storage migration refuses special files",
        ))
    };

    if let Err(error) = copied {
        cleanup_temporary(&temporary);
        return Err(error);
    }
    match rename_no_replace(&temporary, destination) {
        Ok(()) => {
            let parent = destination
                .parent()
                .ok_or_else(|| io::Error::other("migration destination has no parent"))?;
            sync_directory(parent)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            cleanup_temporary(&temporary);
            validate_real_tree(destination, "current storage migration target")?;
            sync_real_tree(destination, "current storage migration target")?;
        }
        Err(error) => {
            cleanup_temporary(&temporary);
            return Err(error);
        }
    }
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let mut source_file = File::open(source)?;
    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    io::copy(&mut source_file, &mut destination_file)?;
    fs::set_permissions(destination, metadata.permissions())?;
    destination_file.sync_all()
}

fn copy_directory(source: &Path, destination: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy_entry_atomically(&entry.path(), &destination.join(entry.file_name()))?;
    }
    fs::set_permissions(destination, metadata.permissions())?;
    sync_directory(destination)
}

fn temporary_sibling(destination: &Path) -> io::Result<PathBuf> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("migration destination has no parent"))?;
    let file_name = destination
        .file_name()
        .ok_or_else(|| io::Error::other("migration destination has no file name"))?
        .to_string_lossy();
    Ok(parent.join(format!(".{file_name}.remtene-migration")))
}

fn cleanup_temporary(path: &Path) {
    let _ = remove_reserved_temporary(path);
}

fn remove_reserved_temporary(path: &Path) -> io::Result<()> {
    if !entry_exists(path)? {
        return Ok(());
    }
    validate_real_tree(path, "storage migration temporary cleanup")?;
    remove_real_entry(path, "storage migration temporary cleanup")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags};

    rustix::fs::renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE)
        .map_err(io::Error::from)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    if entry_exists(destination)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "storage migration destination already exists",
        ));
    }
    fs::rename(source, destination)
}

fn validate_real_tree(path: &Path, operation: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{operation} refuses symbolic links"
        )));
    }
    if metadata.is_file() {
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{operation} refuses special files"
        )));
    }
    for entry in sorted_entries(path)? {
        validate_real_tree(&entry.path(), operation)?;
    }
    Ok(())
}

fn sync_real_tree(path: &Path, operation: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{operation} refuses symbolic links"
        )));
    }
    if metadata.is_file() {
        return File::open(path)?.sync_all();
    }
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{operation} refuses special files"
        )));
    }
    for entry in sorted_entries(path)? {
        sync_real_tree(&entry.path(), operation)?;
    }
    sync_directory(path)
}

fn remove_real_entry(path: &Path, operation: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other(format!(
            "{operation} refuses symbolic links"
        )));
    }
    if metadata.is_file() {
        return fs::remove_file(path);
    }
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{operation} refuses special files"
        )));
    }
    for entry in sorted_entries(path)? {
        remove_real_entry(&entry.path(), operation)?;
    }
    fs::remove_dir(path)
}

fn require_regular_file(path: &Path, operation: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{operation} must be a real regular file"
        )))
    }
}

fn sorted_entries(path: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> io::Result<()> {
    let _ = path;
    Ok(())
}

fn require_real_directory(path: &Path) -> io::Result<()> {
    if real_directory_exists(path)? {
        Ok(())
    } else {
        Err(io::Error::other(
            "legacy storage root must be a real directory",
        ))
    }
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    if entry_exists(path)? {
        require_real_directory(path)
    } else {
        fs::create_dir_all(path)?;
        require_real_directory(path)
    }
}

fn entry_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "remtene-identity-migration-{name}-{}",
            Uuid::new_v4()
        ))
    }

    #[test]
    fn migrates_legacy_data_without_overwriting_current_values_or_duplicating_secrets() {
        let root = test_root("copy");
        let data_base = root.join("data");
        let cache_base = root.join("cache");
        let legacy = data_base.join(LEGACY_STORAGE_DIRECTORY);
        let current = data_base.join(STORAGE_DIRECTORY);
        fs::create_dir_all(legacy.join("secrets")).expect("create legacy secrets");
        fs::create_dir_all(&current).expect("create current data");
        fs::write(legacy.join("settings.json"), b"legacy-settings").expect("legacy settings");
        fs::write(legacy.join("history.json"), b"legacy-history").expect("legacy history");
        fs::write(legacy.join("secrets/master-key.bin"), b"legacy-key").expect("legacy key");
        fs::write(legacy.join(INSTANCE_LEASE_FILE), b"legacy-lock").expect("legacy lock");
        fs::write(current.join("settings.json"), b"current-settings").expect("current settings");
        let legacy_cache = cache_base.join(LEGACY_STORAGE_DIRECTORY);
        fs::create_dir_all(legacy_cache.join("audio")).expect("create legacy audio cache");
        fs::create_dir_all(legacy_cache.join("logs")).expect("create legacy log cache");
        fs::write(legacy_cache.join("audio/stale.wav"), b"stale-audio")
            .expect("write legacy audio artifact");
        fs::write(legacy_cache.join("logs/bard-2026-08-01.log"), b"old-log")
            .expect("write legacy log");

        let paths = prepare_desktop_storage(&data_base, &cache_base).expect("migrate data");

        assert_eq!(paths.data_root(), current);
        assert_eq!(paths.cache_root(), cache_base.join(STORAGE_DIRECTORY));
        assert_eq!(paths.legacy_data_root(), legacy);
        assert_eq!(
            fs::read(current.join("settings.json")).expect("read current settings"),
            b"current-settings"
        );
        assert_eq!(
            fs::read(current.join("history.json")).expect("read migrated history"),
            b"legacy-history"
        );
        assert_eq!(
            fs::read(current.join("secrets/master-key.bin")).expect("read migrated key"),
            b"legacy-key"
        );
        assert!(!current.join(INSTANCE_LEASE_FILE).exists());
        assert!(legacy.is_dir());
        assert!(legacy.join(INSTANCE_LEASE_FILE).is_file());
        assert!(!legacy.join("settings.json").exists());
        assert!(!legacy.join("history.json").exists());
        assert!(!legacy.join("secrets").exists());
        assert!(paths.cache_root().is_dir());
        assert!(!legacy_cache.exists());

        let second = prepare_desktop_storage(&data_base, &cache_base)
            .expect("migration must remain idempotent");
        assert_eq!(second, paths);
        assert_eq!(
            fs::read(current.join("history.json")).expect("read history after second pass"),
            b"legacy-history"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_legacy_symlinks_without_copying_their_targets() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink");
        let data_base = root.join("data");
        let cache_base = root.join("cache");
        let legacy = data_base.join(LEGACY_STORAGE_DIRECTORY);
        let external = root.join("external-settings.json");
        fs::create_dir_all(&legacy).expect("create legacy data");
        fs::write(&external, b"external").expect("write external data");
        symlink(&external, legacy.join("settings.json")).expect("create legacy symlink");

        let error = prepare_desktop_storage(&data_base, &cache_base)
            .expect_err("symbolic link must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&external).expect("read external"), b"external");
        assert!(
            !data_base
                .join(STORAGE_DIRECTORY)
                .join("settings.json")
                .exists()
        );

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_inside_legacy_cache_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let root = test_root("cache-symlink");
        let data_base = root.join("data");
        let cache_base = root.join("cache");
        let legacy_cache = cache_base.join(LEGACY_STORAGE_DIRECTORY);
        let external = root.join("external-audio.wav");
        fs::create_dir_all(&legacy_cache).expect("create legacy cache");
        fs::write(&external, b"external-audio").expect("write external audio");
        symlink(&external, legacy_cache.join("stale.wav")).expect("create cache symlink");

        let error = prepare_desktop_storage(&data_base, &cache_base)
            .expect_err("symbolic link in legacy cache must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            fs::read(&external).expect("read external audio"),
            b"external-audio"
        );
        assert!(legacy_cache.join("stale.wav").is_symlink());

        fs::remove_dir_all(root).expect("remove test root");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_in_the_current_target_without_deleting_legacy_data() {
        use std::os::unix::fs::symlink;

        let root = test_root("current-symlink");
        let data_base = root.join("data");
        let cache_base = root.join("cache");
        let legacy = data_base.join(LEGACY_STORAGE_DIRECTORY);
        let current = data_base.join(STORAGE_DIRECTORY);
        let external = root.join("external-settings.json");
        fs::create_dir_all(&legacy).expect("create legacy data");
        fs::create_dir_all(&current).expect("create current data");
        fs::write(legacy.join("settings.json"), b"legacy-settings").expect("legacy settings");
        fs::write(legacy.join(INSTANCE_LEASE_FILE), b"legacy-lock").expect("legacy lock");
        fs::write(&external, b"external-current").expect("write external current value");
        symlink(&external, current.join("settings.json")).expect("create current symlink");

        let error = prepare_desktop_storage(&data_base, &cache_base)
            .expect_err("symbolic current target must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            fs::read(&external).expect("read external"),
            b"external-current"
        );
        assert_eq!(
            fs::read(legacy.join("settings.json")).expect("legacy source must remain"),
            b"legacy-settings"
        );

        fs::remove_dir_all(root).expect("remove test root");
    }
}
