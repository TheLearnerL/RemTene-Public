//! Platform-owned resolution of the narrow ASR App Group layout.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(target_os = "macos")]
mod macos;

const REMTENE_DIRECTORY: &str = "RemTene";
// ADR-0008: only used to migrate the pre-rename App Group subtree in place.
const LEGACY_STORAGE_DIRECTORY: &str = "Bard";
const ASR_DIRECTORY: &str = "ASR";
const MODELS_DIRECTORY: &str = "models";
const ACTIVE_MODELS_DIRECTORY: &str = "active";
const CANDIDATE_MODELS_DIRECTORY: &str = "candidates";
const GRANTS_DIRECTORY: &str = "grants";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AsrSharedDataPaths {
    root: PathBuf,
    models_root: PathBuf,
    active_models_root: PathBuf,
    candidate_models_root: PathBuf,
    grants_root: PathBuf,
}

impl AsrSharedDataPaths {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn models_root(&self) -> &Path {
        &self.models_root
    }

    #[must_use]
    pub fn active_models_root(&self) -> &Path {
        &self.active_models_root
    }

    #[must_use]
    pub fn candidate_models_root(&self) -> &Path {
        &self.candidate_models_root
    }

    #[must_use]
    pub fn grants_root(&self) -> &Path {
        &self.grants_root
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum AsrSharedDataError {
    #[error("the App Group identifier is invalid")]
    InvalidIdentifier,
    #[error("ASR App Group storage is not supported on this platform")]
    UnsupportedPlatform,
    #[error("the ASR App Group container is unavailable for this signed process")]
    ContainerUnavailable,
}

pub fn resolve_macos_app_group(
    group_identifier: &str,
) -> Result<AsrSharedDataPaths, AsrSharedDataError> {
    validate_group_identifier(group_identifier)?;

    #[cfg(target_os = "macos")]
    {
        let container_root = macos::resolve_container(group_identifier)?;
        prepare_layout(&container_root)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = group_identifier;
        Err(AsrSharedDataError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "macos")]
fn prepare_layout(container_root: &Path) -> Result<AsrSharedDataPaths, AsrSharedDataError> {
    let (canonical_container, container) = open_canonical_container_root(container_root)?;
    migrate_legacy_brand_directory(&container)?;

    let remtene = prepare_private_child(&container, REMTENE_DIRECTORY)?;
    let asr = prepare_private_child(&remtene, ASR_DIRECTORY)?;
    let models = prepare_private_child(&asr, MODELS_DIRECTORY)?;
    let active = prepare_private_child(&models, ACTIVE_MODELS_DIRECTORY)?;
    let candidates = prepare_private_child(&models, CANDIDATE_MODELS_DIRECTORY)?;
    let grants = prepare_private_child(&asr, GRANTS_DIRECTORY)?;

    // Confirm that every pathname still names the directory capability opened above.
    // This catches a concurrent rename/replacement before returning path-based handles.
    verify_private_child(&container, REMTENE_DIRECTORY, &remtene)?;
    verify_private_child(&remtene, ASR_DIRECTORY, &asr)?;
    verify_private_child(&asr, MODELS_DIRECTORY, &models)?;
    verify_private_child(&models, ACTIVE_MODELS_DIRECTORY, &active)?;
    verify_private_child(&models, CANDIDATE_MODELS_DIRECTORY, &candidates)?;
    verify_private_child(&asr, GRANTS_DIRECTORY, &grants)?;
    verify_absolute_directory(&canonical_container, &container)?;

    let remtene_root = contained_child(
        &canonical_container,
        &canonical_container,
        REMTENE_DIRECTORY,
    )?;
    let root = contained_child(&canonical_container, &remtene_root, ASR_DIRECTORY)?;
    let models_root = contained_child(&canonical_container, &root, MODELS_DIRECTORY)?;
    let active_models_root =
        contained_child(&canonical_container, &models_root, ACTIVE_MODELS_DIRECTORY)?;
    let candidate_models_root = contained_child(
        &canonical_container,
        &models_root,
        CANDIDATE_MODELS_DIRECTORY,
    )?;
    let grants_root = contained_child(&canonical_container, &root, GRANTS_DIRECTORY)?;

    let paths = AsrSharedDataPaths {
        root,
        models_root,
        active_models_root,
        candidate_models_root,
        grants_root,
    };
    if [
        paths.root(),
        paths.models_root(),
        paths.active_models_root(),
        paths.candidate_models_root(),
        paths.grants_root(),
    ]
    .into_iter()
    .all(|path| path.starts_with(&canonical_container))
    {
        Ok(paths)
    } else {
        Err(AsrSharedDataError::ContainerUnavailable)
    }
}

#[cfg(target_os = "macos")]
fn migrate_legacy_brand_directory(
    container: &std::os::fd::OwnedFd,
) -> Result<(), AsrSharedDataError> {
    use rustix::fs::{Mode, RenameFlags};

    match rustix::fs::openat(
        container,
        REMTENE_DIRECTORY,
        private_directory_open_flags(),
        Mode::empty(),
    ) {
        Ok(_) => return Ok(()),
        Err(rustix::io::Errno::NOENT) => {}
        Err(_) => return Err(AsrSharedDataError::ContainerUnavailable),
    }

    let legacy = match rustix::fs::openat(
        container,
        LEGACY_STORAGE_DIRECTORY,
        private_directory_open_flags(),
        Mode::empty(),
    ) {
        Ok(legacy) => legacy,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(_) => return Err(AsrSharedDataError::ContainerUnavailable),
    };
    verify_private_child(container, LEGACY_STORAGE_DIRECTORY, &legacy)?;

    rustix::fs::renameat_with(
        container,
        LEGACY_STORAGE_DIRECTORY,
        container,
        REMTENE_DIRECTORY,
        RenameFlags::NOREPLACE,
    )
    .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    verify_private_child(container, REMTENE_DIRECTORY, &legacy)
}

#[cfg(target_os = "macos")]
fn open_canonical_container_root(
    container_root: &Path,
) -> Result<(PathBuf, std::os::fd::OwnedFd), AsrSharedDataError> {
    use rustix::fs::Mode;

    if !container_root.is_absolute() {
        return Err(AsrSharedDataError::ContainerUnavailable);
    }
    let container = rustix::fs::open(
        container_root,
        private_directory_open_flags(),
        Mode::empty(),
    )
    .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    let canonical = std::fs::canonicalize(container_root)
        .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    if !canonical.is_absolute() {
        return Err(AsrSharedDataError::ContainerUnavailable);
    }
    verify_absolute_directory(&canonical, &container)?;
    Ok((canonical, container))
}

#[cfg(target_os = "macos")]
fn prepare_private_child(
    parent: &std::os::fd::OwnedFd,
    component: &str,
) -> Result<std::os::fd::OwnedFd, AsrSharedDataError> {
    use rustix::fs::Mode;

    match rustix::fs::mkdirat(parent, component, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(_) => return Err(AsrSharedDataError::ContainerUnavailable),
    }
    let directory = rustix::fs::openat(
        parent,
        component,
        private_directory_open_flags(),
        Mode::empty(),
    )
    .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    rustix::fs::fchmod(&directory, Mode::RWXU)
        .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    Ok(directory)
}

#[cfg(target_os = "macos")]
fn verify_private_child(
    parent: &std::os::fd::OwnedFd,
    component: &str,
    expected: &std::os::fd::OwnedFd,
) -> Result<(), AsrSharedDataError> {
    use rustix::fs::Mode;

    let observed = rustix::fs::openat(
        parent,
        component,
        private_directory_open_flags(),
        Mode::empty(),
    )
    .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    verify_same_directory(expected, &observed)
}

#[cfg(target_os = "macos")]
fn verify_absolute_directory(
    path: &Path,
    expected: &std::os::fd::OwnedFd,
) -> Result<(), AsrSharedDataError> {
    use rustix::fs::Mode;

    let observed = rustix::fs::open(path, private_directory_open_flags(), Mode::empty())
        .map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    verify_same_directory(expected, &observed)
}

#[cfg(target_os = "macos")]
fn verify_same_directory(
    expected: &std::os::fd::OwnedFd,
    observed: &std::os::fd::OwnedFd,
) -> Result<(), AsrSharedDataError> {
    let expected_stat =
        rustix::fs::fstat(expected).map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    let observed_stat =
        rustix::fs::fstat(observed).map_err(|_| AsrSharedDataError::ContainerUnavailable)?;
    if expected_stat.st_dev == observed_stat.st_dev && expected_stat.st_ino == observed_stat.st_ino
    {
        Ok(())
    } else {
        Err(AsrSharedDataError::ContainerUnavailable)
    }
}

#[cfg(target_os = "macos")]
fn private_directory_open_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;

    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

#[cfg(target_os = "macos")]
fn contained_child(
    container_root: &Path,
    parent: &Path,
    component: &str,
) -> Result<PathBuf, AsrSharedDataError> {
    use std::path::Component;

    let mut components = Path::new(component).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || !parent.starts_with(container_root)
    {
        return Err(AsrSharedDataError::ContainerUnavailable);
    }
    let child = parent.join(component);
    if child.starts_with(container_root) {
        Ok(child)
    } else {
        Err(AsrSharedDataError::ContainerUnavailable)
    }
}

fn validate_group_identifier(group_identifier: &str) -> Result<(), AsrSharedDataError> {
    let bytes = group_identifier.as_bytes();
    let valid = (3..=255).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && !group_identifier.contains("..")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AsrSharedDataError::InvalidIdentifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn app_group_identifier_rejects_paths_empty_segments_and_whitespace() {
        for invalid in [
            "",
            "a",
            ".group.remtene",
            "group.remtene.",
            "group..remtene",
            "group/remtene",
            "group remtene",
            "group_remtene",
        ] {
            assert_eq!(
                validate_group_identifier(invalid),
                Err(AsrSharedDataError::InvalidIdentifier),
                "identifier {invalid:?}"
            );
        }
    }

    #[test]
    fn app_group_identifier_accepts_macos_registered_identifier_shapes() {
        for valid in [
            "group.com.remtene.desktop.asr",
            "TEAM123456.group.com.remtene.desktop.asr",
            "TEAM123456.remtene-asr",
        ] {
            assert_eq!(validate_group_identifier(valid), Ok(()));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn layout_contains_only_models_and_ephemeral_grants() {
        let container = std::env::temp_dir().join(format!(
            "remtene-asr-shared-layout-{}",
            Uuid::new_v4().hyphenated()
        ));
        std::fs::create_dir_all(&container).expect("create test container");
        let paths = prepare_layout(&container).expect("prepare ASR layout");
        let canonical_container =
            std::fs::canonicalize(&container).expect("canonicalize test container");

        assert_eq!(paths.root(), canonical_container.join("RemTene/ASR"));
        assert_eq!(paths.models_root(), paths.root().join("models"));
        assert_eq!(
            paths.active_models_root(),
            paths.models_root().join("active")
        );
        assert_eq!(
            paths.candidate_models_root(),
            paths.models_root().join("candidates")
        );
        assert_eq!(paths.grants_root(), paths.root().join("grants"));
        let names = std::fs::read_dir(paths.root())
            .expect("read ASR root")
            .map(|entry| entry.expect("layout entry").file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            ["grants", "models"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect()
        );
        for directory in [
            canonical_container.join("RemTene"),
            paths.root().to_path_buf(),
            paths.models_root().to_path_buf(),
            paths.active_models_root().to_path_buf(),
            paths.candidate_models_root().to_path_buf(),
            paths.grants_root().to_path_buf(),
        ] {
            use std::os::unix::fs::PermissionsExt;

            let metadata = std::fs::symlink_metadata(directory).expect("read layout metadata");
            assert!(metadata.is_dir());
            assert!(!metadata.file_type().is_symlink());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        std::fs::remove_dir_all(container).expect("remove test container");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn layout_atomically_migrates_the_legacy_brand_subtree() {
        let container = std::env::temp_dir().join(format!(
            "remtene-asr-shared-legacy-layout-{}",
            Uuid::new_v4().hyphenated()
        ));
        let legacy_active = container
            .join(LEGACY_STORAGE_DIRECTORY)
            .join("ASR/models/active");
        std::fs::create_dir_all(&legacy_active).expect("create legacy active models");
        std::fs::write(legacy_active.join("model.manifest.json"), b"legacy-model")
            .expect("write legacy model marker");

        let paths = prepare_layout(&container).expect("migrate ASR layout");

        assert!(!container.join(LEGACY_STORAGE_DIRECTORY).exists());
        assert_eq!(
            std::fs::read(paths.active_models_root().join("model.manifest.json"))
                .expect("read migrated model marker"),
            b"legacy-model"
        );
        assert!(paths.candidate_models_root().is_dir());
        assert!(paths.grants_root().is_dir());

        std::fs::remove_dir_all(container).expect("remove test container");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn layout_rejects_remtene_ancestor_symlink_without_touching_external_directory() {
        assert_ancestor_symlink_is_rejected(&[REMTENE_DIRECTORY], "ASR");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn layout_rejects_asr_ancestor_symlink_without_touching_external_directory() {
        assert_ancestor_symlink_is_rejected(&[REMTENE_DIRECTORY, ASR_DIRECTORY], "models");
    }

    #[cfg(target_os = "macos")]
    fn assert_ancestor_symlink_is_rejected(components: &[&str], forbidden_child: &str) {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let test_root = std::env::temp_dir().join(format!(
            "remtene-asr-shared-symlink-{}",
            Uuid::new_v4().hyphenated()
        ));
        let container = test_root.join("container");
        let external = test_root.join("external");
        std::fs::create_dir_all(&container).expect("create test container");
        std::fs::create_dir_all(&external).expect("create external directory");
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o751))
            .expect("set external permissions");

        let (ancestors, symlink_name) = components.split_at(components.len() - 1);
        let mut parent = container.clone();
        for ancestor in ancestors {
            parent.push(ancestor);
            std::fs::create_dir(&parent).expect("create real ancestor");
        }
        symlink(&external, parent.join(symlink_name[0])).expect("create ancestor symlink");

        assert_eq!(
            prepare_layout(&container),
            Err(AsrSharedDataError::ContainerUnavailable)
        );
        assert!(!external.join(forbidden_child).exists());
        let external_mode = std::fs::symlink_metadata(&external)
            .expect("read external metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(external_mode, 0o751);

        std::fs::remove_dir_all(test_root).expect("remove symlink test root");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires REMTENE_RUN_LIVE_MACOS_APP_GROUP=1 and a matching signed entitlement"]
    fn live_signed_process_resolves_its_registered_app_group() {
        if std::env::var("REMTENE_RUN_LIVE_MACOS_APP_GROUP").as_deref() != Ok("1") {
            return;
        }
        let identifier = std::env::var("REMTENE_MACOS_APP_GROUP_ID")
            .expect("REMTENE_MACOS_APP_GROUP_ID must name the signed App Group");
        let paths =
            resolve_macos_app_group(&identifier).expect("resolve signed App Group container");
        assert!(paths.root().is_absolute());
        assert!(paths.models_root().is_dir());
        assert!(paths.grants_root().is_dir());
    }
}
