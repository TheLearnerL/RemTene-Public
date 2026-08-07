//! Model Manifest and integrity verification.
//!
//! This module defines the structure and validation logic for ASR model packages.
//! Each model must have a manifest that declares its identity, hash, and compatibility.
//!
//! ## Security Properties
//!
//! - Package integrity verified via SHA-256 hash
//! - Manifest schema versioned for forward compatibility
//! - Worker compatibility declared explicitly
//! - License information required for audit trail

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Model manifest schema version.
///
/// Increment when making backwards-incompatible changes to the manifest format.
pub const MANIFEST_SCHEMA_VERSION: u16 = 1;

/// Model engine type.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelEngine {
    /// Qwen3-ASR streaming engine
    Qwen,
    /// Whisper.cpp batch engine
    Whisper,
}

/// Model quantization format.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Quantization {
    /// No quantization (full precision)
    None,
    /// 4-bit quantization
    Q4_0,
    /// 5-bit quantization
    Q5_0,
    /// 8-bit quantization
    Q8_0,
    /// Integer 8-bit quantization
    Int8,
}

/// Target platform for the model binary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelPlatform {
    /// macOS ARM64 (Apple Silicon)
    MacosArm64,
    /// macOS x86_64 (Intel)
    MacosX64,
    /// Windows x86_64
    WindowsX64,
    /// Platform-independent (e.g., GGUF models that work across platforms)
    Any,
}

/// One declared file inside a directory-shaped model package.
///
/// `path` is always relative to the package directory and must not escape it, so a
/// manifest can never point the integrity check at a file outside the package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageFileEntry {
    /// Relative path inside the package directory (POSIX separators)
    pub path: String,
    /// SHA-256 hash of that file (hex string, 64 chars)
    pub sha256: String,
}

/// License information for compliance and audit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LicenseInfo {
    /// SPDX license identifier (e.g., "Apache-2.0", "MIT")
    pub spdx_id: String,
    /// URL to the full license text
    pub url: String,
    /// Optional attribution requirements
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

/// Model manifest declaring identity, integrity, and compatibility.
///
/// The manifest must be named `{model_id}.manifest.json` and placed alongside
/// the model file in the `models/active/` or `models/candidates/` directory.
///
/// ## Example
///
/// ```json
/// {
///   "schema_version": 1,
///   "model_id": "qwen3-asr-0.6b-v1",
///   "engine": "qwen",
///   "architecture": "0.6b",
///   "quantization": "int8",
///   "version": "1.0.0",
///   "platform": "any",
///   "package_sha256": "abcd1234...",
///   "worker_compat": ">=0.1.0",
///   "license": {
///     "spdx_id": "Apache-2.0",
///     "url": "https://www.apache.org/licenses/LICENSE-2.0"
///   }
/// }
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelManifest {
    /// Schema version (must equal MANIFEST_SCHEMA_VERSION)
    pub schema_version: u16,

    /// Unique model identifier (e.g., "qwen3-asr-0.6b-v1")
    pub model_id: String,

    /// Engine type
    pub engine: ModelEngine,

    /// Architecture name (e.g., "0.6b", "large-v3-turbo")
    pub architecture: String,

    /// Quantization format
    pub quantization: Quantization,

    /// Model version (semver recommended, e.g., "1.0.0")
    pub version: String,

    /// Target platform
    pub platform: ModelPlatform,

    /// SHA-256 hash of a single-file model package (hex string, 64 chars).
    ///
    /// Exactly one of `package_sha256` and `package_files` must be present, so every
    /// package has exactly one integrity proof and neither shape can be left unchecked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_sha256: Option<String>,

    /// Per-file hashes of a directory-shaped model package (DEC-MODEL-01).
    ///
    /// Multi-file weight layouts (Qwen) cannot be proven by one hash, so the manifest
    /// enumerates every file it covers. Verification additionally rejects undeclared
    /// files, so nothing can be smuggled into a verified package directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_files: Option<Vec<PackageFileEntry>>,

    /// Worker compatibility requirement (semver range, e.g., ">=0.1.0")
    pub worker_compat: String,

    /// License information
    pub license: LicenseInfo,

    /// Optional human-readable description
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ManifestError {
    #[error("manifest schema version mismatch: expected {expected}, got {actual}")]
    SchemaVersionMismatch { expected: u16, actual: u16 },

    #[error("model_id is empty")]
    ModelIdEmpty,

    #[error("architecture is empty")]
    ArchitectureEmpty,

    #[error("version is empty")]
    VersionEmpty,

    #[error("package_sha256 is invalid: {reason}")]
    InvalidPackageHash { reason: String },

    #[error("worker_compat is empty")]
    WorkerCompatEmpty,

    #[error("license.spdx_id is empty")]
    LicenseSpdxIdEmpty,

    #[error("license.url is empty")]
    LicenseUrlEmpty,

    #[error("manifest file not found: {path}")]
    ManifestNotFound { path: String },

    #[error("manifest parse error: {reason}")]
    ParseError { reason: String },

    #[error("model package not found: {path}")]
    PackageNotFound { path: String },

    #[error("package hash mismatch: expected {expected}, got {actual}")]
    PackageHashMismatch { expected: String, actual: String },

    #[error("manifest must declare exactly one of package_sha256 or package_files")]
    PackageIntegrityUndecided,

    #[error("package_files is empty")]
    PackageFilesEmpty,

    #[error("package file path is unsafe: {path}")]
    UnsafePackageFilePath { path: String },

    #[error("package directory contains undeclared entry: {path}")]
    UndeclaredPackageEntry { path: String },

    #[error("package shape mismatch: {reason}")]
    PackageShapeMismatch { reason: String },

    #[error("I/O error: {message}")]
    Io { message: String },
}

impl ModelManifest {
    /// Validate the manifest structure (does not verify file hashes).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::SchemaVersionMismatch {
                expected: MANIFEST_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }

        if self.model_id.trim().is_empty() {
            return Err(ManifestError::ModelIdEmpty);
        }

        if self.architecture.trim().is_empty() {
            return Err(ManifestError::ArchitectureEmpty);
        }

        if self.version.trim().is_empty() {
            return Err(ManifestError::VersionEmpty);
        }

        match (&self.package_sha256, &self.package_files) {
            (Some(hash), None) => validate_sha256_hex(hash)?,
            (None, Some(files)) => validate_package_files(files)?,
            _ => return Err(ManifestError::PackageIntegrityUndecided),
        }

        if self.worker_compat.trim().is_empty() {
            return Err(ManifestError::WorkerCompatEmpty);
        }

        if self.license.spdx_id.trim().is_empty() {
            return Err(ManifestError::LicenseSpdxIdEmpty);
        }

        if self.license.url.trim().is_empty() {
            return Err(ManifestError::LicenseUrlEmpty);
        }

        Ok(())
    }

    /// Load a manifest from a JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self, ManifestError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ManifestError::ManifestNotFound {
                    path: path.display().to_string(),
                }
            } else {
                ManifestError::Io {
                    message: format!("failed to read {}: {}", path.display(), e),
                }
            }
        })?;

        let manifest: ModelManifest =
            serde_json::from_str(&content).map_err(|e| ManifestError::ParseError {
                reason: e.to_string(),
            })?;

        manifest.validate()?;

        Ok(manifest)
    }

    /// Whether this manifest describes a directory-shaped package.
    #[must_use]
    pub fn is_directory_package(&self) -> bool {
        self.package_files.is_some()
    }

    /// Verify the integrity of the model package.
    ///
    /// Single-file packages must match `package_sha256`. Directory packages must contain
    /// exactly the files listed in `package_files`, each a regular file with the declared
    /// hash: an extra, missing, replaced or symlinked entry all fail closed.
    pub fn verify_package_integrity(&self, package_path: &Path) -> Result<(), ManifestError> {
        if !package_path.exists() {
            return Err(ManifestError::PackageNotFound {
                path: package_path.display().to_string(),
            });
        }

        match (&self.package_sha256, &self.package_files) {
            (Some(expected), None) => verify_single_file(package_path, expected),
            (None, Some(files)) => verify_directory(package_path, files),
            _ => Err(ManifestError::PackageIntegrityUndecided),
        }
    }
}

fn verify_single_file(package_path: &Path, expected: &str) -> Result<(), ManifestError> {
    if !package_path.is_file() {
        return Err(ManifestError::PackageShapeMismatch {
            reason: format!(
                "{} must be a regular file for a single-hash manifest",
                package_path.display()
            ),
        });
    }

    let computed_hash = compute_file_sha256(package_path)?;
    if computed_hash != expected {
        return Err(ManifestError::PackageHashMismatch {
            expected: expected.to_owned(),
            actual: computed_hash,
        });
    }
    Ok(())
}

fn verify_directory(package_path: &Path, files: &[PackageFileEntry]) -> Result<(), ManifestError> {
    if !package_path.is_dir() {
        return Err(ManifestError::PackageShapeMismatch {
            reason: format!(
                "{} must be a directory for a per-file manifest",
                package_path.display()
            ),
        });
    }

    let mut declared = std::collections::BTreeSet::new();
    for entry in files {
        let relative = safe_relative_path(&entry.path)?;
        let file_path = package_path.join(&relative);
        let metadata =
            std::fs::symlink_metadata(&file_path).map_err(|_| ManifestError::PackageNotFound {
                path: file_path.display().to_string(),
            })?;
        if !metadata.is_file() {
            return Err(ManifestError::PackageShapeMismatch {
                reason: format!("{} is not a regular file", file_path.display()),
            });
        }

        let computed_hash = compute_file_sha256(&file_path)?;
        if computed_hash != entry.sha256 {
            return Err(ManifestError::PackageHashMismatch {
                expected: entry.sha256.clone(),
                actual: computed_hash,
            });
        }
        declared.insert(relative);
    }

    reject_undeclared_entries(package_path, package_path, &declared)?;
    Ok(())
}

/// Walk the package directory and reject anything the manifest does not cover.
fn reject_undeclared_entries(
    root: &Path,
    current: &Path,
    declared: &std::collections::BTreeSet<PathBuf>,
) -> Result<(), ManifestError> {
    let entries = std::fs::read_dir(current).map_err(|e| ManifestError::Io {
        message: format!("failed to read {}: {}", current.display(), e),
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| ManifestError::Io {
            message: format!("failed to read entry in {}: {}", current.display(), e),
        })?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| ManifestError::Io {
            message: format!("failed to stat {}: {}", path.display(), e),
        })?;

        if metadata.is_dir() {
            reject_undeclared_entries(root, &path, declared)?;
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| ManifestError::UndeclaredPackageEntry {
                path: path.display().to_string(),
            })?
            .to_path_buf();
        if !declared.contains(&relative) {
            return Err(ManifestError::UndeclaredPackageEntry {
                path: relative.display().to_string(),
            });
        }
    }

    Ok(())
}

fn validate_package_files(files: &[PackageFileEntry]) -> Result<(), ManifestError> {
    if files.is_empty() {
        return Err(ManifestError::PackageFilesEmpty);
    }

    let mut seen = std::collections::BTreeSet::new();
    for entry in files {
        let relative = safe_relative_path(&entry.path)?;
        if !seen.insert(relative) {
            return Err(ManifestError::UnsafePackageFilePath {
                path: entry.path.clone(),
            });
        }
        validate_sha256_hex(&entry.sha256)?;
    }
    Ok(())
}

/// Accept only plain relative paths: no root, no `..`, no empty or current-dir parts.
fn safe_relative_path(declared: &str) -> Result<PathBuf, ManifestError> {
    use std::path::Component;

    if declared.trim().is_empty() {
        return Err(ManifestError::UnsafePackageFilePath {
            path: declared.to_owned(),
        });
    }

    let candidate = PathBuf::from(declared);
    let safe = candidate
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    if !safe {
        return Err(ManifestError::UnsafePackageFilePath {
            path: declared.to_owned(),
        });
    }

    Ok(candidate)
}

/// Validate a SHA-256 hash string (must be 64 lowercase hex characters).
fn validate_sha256_hex(hash: &str) -> Result<(), ManifestError> {
    if hash.len() != 64 {
        return Err(ManifestError::InvalidPackageHash {
            reason: format!("expected 64 characters, got {}", hash.len()),
        });
    }

    if !hash
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    {
        return Err(ManifestError::InvalidPackageHash {
            reason: "must be lowercase hex".to_owned(),
        });
    }

    Ok(())
}

/// Compute the SHA-256 hash of a file.
fn compute_file_sha256(path: &Path) -> Result<String, ManifestError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|e| ManifestError::Io {
        message: format!("failed to open {}: {}", path.display(), e),
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let n = file.read(&mut buffer).map_err(|e| ManifestError::Io {
            message: format!("failed to read {}: {}", path.display(), e),
        })?;

        if n == 0 {
            break;
        }

        hasher.update(&buffer[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_manifest() -> ModelManifest {
        ModelManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            model_id: "qwen3-asr-0.6b-v1".to_owned(),
            engine: ModelEngine::Qwen,
            architecture: "0.6b".to_owned(),
            quantization: Quantization::Int8,
            version: "1.0.0".to_owned(),
            platform: ModelPlatform::Any,
            package_sha256: Some("a".repeat(64)),
            package_files: None,
            worker_compat: ">=0.1.0".to_owned(),
            license: LicenseInfo {
                spdx_id: "Apache-2.0".to_owned(),
                url: "https://www.apache.org/licenses/LICENSE-2.0".to_owned(),
                attribution: None,
            },
            description: Some("Qwen3-ASR 0.6B INT8 quantized model".to_owned()),
        }
    }

    #[test]
    fn valid_manifest_passes_validation() {
        let manifest = example_manifest();
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn schema_version_mismatch_is_rejected() {
        let mut manifest = example_manifest();
        manifest.schema_version = 999;
        assert_eq!(
            manifest.validate(),
            Err(ManifestError::SchemaVersionMismatch {
                expected: MANIFEST_SCHEMA_VERSION,
                actual: 999
            })
        );
    }

    #[test]
    fn empty_model_id_is_rejected() {
        let mut manifest = example_manifest();
        manifest.model_id = "".to_owned();
        assert_eq!(manifest.validate(), Err(ManifestError::ModelIdEmpty));
    }

    #[test]
    fn invalid_package_hash_length_is_rejected() {
        let mut manifest = example_manifest();
        manifest.package_sha256 = Some("short".to_owned());
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidPackageHash { .. })
        ));
    }

    #[test]
    fn uppercase_hash_is_rejected() {
        let mut manifest = example_manifest();
        manifest.package_sha256 = Some("A".repeat(64));
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidPackageHash { .. })
        ));
    }

    #[test]
    fn non_hex_hash_is_rejected() {
        let mut manifest = example_manifest();
        manifest.package_sha256 = Some("g".repeat(64));
        assert!(matches!(
            manifest.validate(),
            Err(ManifestError::InvalidPackageHash { .. })
        ));
    }

    /// 建立一个目录型模型包夹具，返回目录与覆盖它的 manifest。
    fn directory_package_fixture(tag: &str) -> (PathBuf, ModelManifest) {
        let root = std::env::temp_dir().join(format!(
            "remtene-dir-package-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock must be after the epoch")
                .as_nanos()
        ));
        let package = root.join("qwen3-asr-0.6b-v1");
        std::fs::create_dir_all(&package).expect("fixture directory must be creatable");
        std::fs::write(package.join("weights.bin"), b"weights").expect("weights must be written");
        std::fs::write(package.join("vocab.json"), b"vocab").expect("vocab must be written");

        let mut manifest = example_manifest();
        manifest.package_sha256 = None;
        manifest.package_files = Some(vec![
            PackageFileEntry {
                path: "weights.bin".to_owned(),
                sha256: compute_file_sha256(&package.join("weights.bin")).expect("hash"),
            },
            PackageFileEntry {
                path: "vocab.json".to_owned(),
                sha256: compute_file_sha256(&package.join("vocab.json")).expect("hash"),
            },
        ]);
        (package, manifest)
    }

    #[test]
    fn declaring_both_or_neither_integrity_proof_is_rejected() {
        let mut both = example_manifest();
        both.package_files = Some(vec![PackageFileEntry {
            path: "weights.bin".to_owned(),
            sha256: "a".repeat(64),
        }]);
        assert_eq!(
            both.validate(),
            Err(ManifestError::PackageIntegrityUndecided)
        );

        let mut neither = example_manifest();
        neither.package_sha256 = None;
        assert_eq!(
            neither.validate(),
            Err(ManifestError::PackageIntegrityUndecided)
        );
    }

    #[test]
    fn package_file_paths_must_stay_inside_the_package() {
        for unsafe_path in [
            "../escape.bin",
            "/etc/passwd",
            "nested/../../escape.bin",
            " ",
        ] {
            let mut manifest = example_manifest();
            manifest.package_sha256 = None;
            manifest.package_files = Some(vec![PackageFileEntry {
                path: unsafe_path.to_owned(),
                sha256: "a".repeat(64),
            }]);
            assert!(
                matches!(
                    manifest.validate(),
                    Err(ManifestError::UnsafePackageFilePath { .. })
                ),
                "must reject {unsafe_path}"
            );
        }
    }

    #[test]
    fn intact_directory_package_verifies() {
        let (package, manifest) = directory_package_fixture("intact");
        manifest.validate().expect("manifest must be valid");
        manifest
            .verify_package_integrity(&package)
            .expect("intact package must verify");
        let _ = std::fs::remove_dir_all(package.parent().expect("fixture root"));
    }

    #[test]
    fn tampered_file_in_directory_package_is_rejected() {
        let (package, manifest) = directory_package_fixture("tampered");
        std::fs::write(package.join("vocab.json"), b"tampered").expect("tamper must be written");
        assert!(matches!(
            manifest.verify_package_integrity(&package),
            Err(ManifestError::PackageHashMismatch { .. })
        ));
        let _ = std::fs::remove_dir_all(package.parent().expect("fixture root"));
    }

    #[test]
    fn undeclared_file_in_directory_package_is_rejected() {
        let (package, manifest) = directory_package_fixture("undeclared");
        std::fs::write(package.join("smuggled.bin"), b"extra").expect("extra must be written");
        assert!(matches!(
            manifest.verify_package_integrity(&package),
            Err(ManifestError::UndeclaredPackageEntry { .. })
        ));
        let _ = std::fs::remove_dir_all(package.parent().expect("fixture root"));
    }

    #[test]
    fn missing_file_in_directory_package_is_rejected() {
        let (package, manifest) = directory_package_fixture("missing");
        std::fs::remove_file(package.join("vocab.json")).expect("file must be removable");
        assert!(matches!(
            manifest.verify_package_integrity(&package),
            Err(ManifestError::PackageNotFound { .. })
        ));
        let _ = std::fs::remove_dir_all(package.parent().expect("fixture root"));
    }

    #[test]
    fn directory_manifest_rejects_a_single_file_package_shape() {
        let (package, manifest) = directory_package_fixture("shape");
        let file_package = package.join("weights.bin");
        assert!(matches!(
            manifest.verify_package_integrity(&file_package),
            Err(ManifestError::PackageShapeMismatch { .. })
        ));
        let _ = std::fs::remove_dir_all(package.parent().expect("fixture root"));
    }

    #[test]
    fn empty_worker_compat_is_rejected() {
        let mut manifest = example_manifest();
        manifest.worker_compat = "".to_owned();
        assert_eq!(manifest.validate(), Err(ManifestError::WorkerCompatEmpty));
    }

    #[test]
    fn empty_license_spdx_id_is_rejected() {
        let mut manifest = example_manifest();
        manifest.license.spdx_id = "".to_owned();
        assert_eq!(manifest.validate(), Err(ManifestError::LicenseSpdxIdEmpty));
    }

    #[test]
    fn manifest_serializes_to_json() {
        let manifest = example_manifest();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"model_id\": \"qwen3-asr-0.6b-v1\""));
        assert!(json.contains("\"engine\": \"qwen\""));
    }

    #[test]
    fn manifest_deserializes_from_json() {
        let json = r#"{
            "schema_version": 1,
            "model_id": "whisper-large-v3-turbo-q5_0-v1",
            "engine": "whisper",
            "architecture": "large-v3-turbo",
            "quantization": "q5_0",
            "version": "1.0.0",
            "platform": "macos-arm64",
            "package_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "worker_compat": ">=0.1.0",
            "license": {
                "spdx_id": "MIT",
                "url": "https://opensource.org/licenses/MIT"
            }
        }"#;

        let manifest: ModelManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.model_id, "whisper-large-v3-turbo-q5_0-v1");
        assert_eq!(manifest.engine, ModelEngine::Whisper);
        assert_eq!(manifest.quantization, Quantization::Q5_0);
        assert!(manifest.validate().is_ok());
    }
}
