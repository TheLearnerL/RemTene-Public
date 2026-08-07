//! Model registry and lifecycle management.
//!
//! This module provides the `ModelRegistry` that discovers, loads, and validates
//! ASR models from the shared data directories.
//!
//! ## Directory Structure
//!
//! ```text
//! RemTene/ASR/models/
//!   ├── active/              # Currently active models
//!   │   ├── qwen3-asr-0.6b-v1.manifest.json
//!   │   ├── qwen3-asr-0.6b-v1.gguf
//!   │   ├── whisper-large-v3-turbo-q5_0-v1.manifest.json
//!   │   └── whisper-large-v3-turbo-q5_0-v1.bin
//!   └── candidates/          # Candidate models (not yet active)
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::model_manifest::{ManifestError, ModelEngine, ModelManifest};

/// Model registry entry containing manifest and file path.
#[derive(Clone, Debug)]
pub struct ModelEntry {
    /// Parsed and validated manifest
    pub manifest: ModelManifest,
    /// Absolute path to the model package file
    pub package_path: PathBuf,
    /// Absolute path to the manifest file
    pub manifest_path: PathBuf,
}

/// Registry of discovered and validated models.
#[derive(Clone, Debug, Default)]
pub struct ModelRegistry {
    /// Models indexed by model_id
    models: HashMap<String, ModelEntry>,
}

#[derive(Clone, Debug, Error)]
pub enum RegistryError {
    #[error("manifest error: {0}")]
    Manifest(#[from] ManifestError),

    #[error("no models found in directory: {path}")]
    NoModelsFound { path: String },

    #[error("model not found: {model_id}")]
    ModelNotFound { model_id: String },

    #[error("no {engine:?} model found")]
    NoEngineModel { engine: ModelEngine },

    #[error("model manifest identity mismatch: expected {expected}, got {actual}")]
    ModelIdentityMismatch { expected: String, actual: String },

    #[error("model engine mismatch for {model_id}: expected {expected:?}, got {actual:?}")]
    ModelEngineMismatch {
        model_id: String,
        expected: ModelEngine,
        actual: ModelEngine,
    },

    #[error("I/O error: {message}")]
    Io { message: String },
}

impl ModelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// Scan a directory for model manifests and load them.
    ///
    /// Only manifests ending with `.manifest.json` are considered.
    /// For each manifest, the corresponding model package file is expected
    /// to have the same base name (e.g., `model.manifest.json` → `model.gguf`).
    ///
    /// ## Package File Extensions
    ///
    /// - `.gguf` for GGUF models (Qwen, Whisper)
    /// - `.bin` for other binary formats
    ///
    /// ## Errors
    ///
    /// Returns an error if:
    /// - The directory cannot be read
    /// - A manifest fails to parse or validate
    /// - Package integrity verification fails
    ///
    /// ## Package Integrity
    ///
    /// By default, package integrity is verified. Pass `verify_integrity: false`
    /// to skip hash verification (useful for development).
    pub fn scan_directory(
        &mut self,
        dir: &Path,
        verify_integrity: bool,
    ) -> Result<usize, RegistryError> {
        let entries = std::fs::read_dir(dir).map_err(|e| RegistryError::Io {
            message: format!("failed to read directory {}: {}", dir.display(), e),
        })?;

        let mut loaded_count = 0;

        for entry in entries {
            let entry = entry.map_err(|e| RegistryError::Io {
                message: format!("failed to read directory entry: {}", e),
            })?;

            let path = entry.path();

            // Only consider .manifest.json files
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".manifest.json"))
            {
                continue;
            }

            let manifest = ModelManifest::load_from_file(&path)?;

            // Find the corresponding package: a directory for per-file manifests,
            // a single file otherwise.
            let package_path = find_package_path(&path, &manifest)?;

            // Verify package integrity if requested
            if verify_integrity {
                manifest.verify_package_integrity(&package_path)?;
            }

            let entry = ModelEntry {
                manifest: manifest.clone(),
                package_path,
                manifest_path: path,
            };

            self.models.insert(manifest.model_id.clone(), entry);
            loaded_count += 1;
        }

        Ok(loaded_count)
    }

    /// Load one fixed model package without allowing an unrelated bad manifest to block it.
    ///
    /// The manifest path is derived from `model_id`; the manifest must repeat that exact identity
    /// and the expected engine before its package path or hashes are trusted.
    pub fn load_model(
        &mut self,
        dir: &Path,
        model_id: &str,
        expected_engine: ModelEngine,
        verify_integrity: bool,
    ) -> Result<ModelEntry, RegistryError> {
        let manifest_path = dir.join(format!("{model_id}.manifest.json"));
        let manifest = ModelManifest::load_from_file(&manifest_path)?;
        if manifest.model_id != model_id {
            return Err(RegistryError::ModelIdentityMismatch {
                expected: model_id.to_owned(),
                actual: manifest.model_id,
            });
        }
        if manifest.engine != expected_engine {
            return Err(RegistryError::ModelEngineMismatch {
                model_id: model_id.to_owned(),
                expected: expected_engine,
                actual: manifest.engine,
            });
        }

        let package_path = find_package_path(&manifest_path, &manifest)?;
        if verify_integrity {
            manifest.verify_package_integrity(&package_path)?;
        }
        let entry = ModelEntry {
            manifest,
            package_path,
            manifest_path,
        };
        self.models.insert(model_id.to_owned(), entry.clone());
        Ok(entry)
    }

    /// Get a model by its ID.
    pub fn get(&self, model_id: &str) -> Option<&ModelEntry> {
        self.models.get(model_id)
    }

    /// Get the first available model for a given engine.
    pub fn get_by_engine(&self, engine: ModelEngine) -> Option<&ModelEntry> {
        self.models
            .values()
            .find(|entry| entry.manifest.engine == engine)
    }

    /// List all registered model IDs.
    pub fn list_model_ids(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Number of registered models.
    pub fn len(&self) -> usize {
        self.models.len()
    }
}

/// Find the package a manifest describes.
///
/// Directory packages live in a sibling directory named exactly `{model_id}`; single-file
/// packages use `{model_id}.gguf` or `{model_id}.bin`. The manifest decides which shape is
/// expected, so a directory can never be accepted under a single-hash proof.
fn find_package_path(
    manifest_path: &Path,
    manifest: &ModelManifest,
) -> Result<PathBuf, ManifestError> {
    let dir = manifest_path.parent().ok_or_else(|| ManifestError::Io {
        message: format!("manifest path has no parent: {}", manifest_path.display()),
    })?;
    let model_id = manifest.model_id.as_str();

    if manifest.is_directory_package() {
        let candidate = dir.join(model_id);
        return if candidate.is_dir() {
            Ok(candidate)
        } else {
            Err(ManifestError::PackageNotFound {
                path: candidate.display().to_string(),
            })
        };
    }

    // Try common extensions
    for ext in &["gguf", "bin"] {
        let candidate = dir.join(format!("{}.{}", model_id, ext));
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(ManifestError::PackageNotFound {
        path: format!("{}/{}.{{gguf,bin}}", dir.display(), model_id),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use sha2::{Digest, Sha256};

    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("remtene-model-registry-{name}-{nonce}"));
        std::fs::create_dir_all(&root).expect("test model root");
        root
    }

    fn write_single_file_model(root: &Path, model_id: &str, engine: &str, bytes: &[u8]) {
        let hash = format!("{:x}", Sha256::digest(bytes));
        std::fs::write(root.join(format!("{model_id}.gguf")), bytes).expect("model bytes");
        let manifest = serde_json::json!({
            "schema_version": 1,
            "model_id": model_id,
            "engine": engine,
            "architecture": "test",
            "quantization": "q5_0",
            "version": "1.0.0",
            "platform": "any",
            "package_sha256": hash,
            "worker_compat": ">=0.1.0",
            "license": {
                "spdx_id": "MIT",
                "url": "https://opensource.org/licenses/MIT"
            }
        });
        std::fs::write(
            root.join(format!("{model_id}.manifest.json")),
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("manifest bytes");
    }

    #[test]
    fn empty_registry_has_no_models() {
        let registry = ModelRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.list_model_ids(), Vec::<String>::new());
    }

    #[test]
    fn get_nonexistent_model_returns_none() {
        let registry = ModelRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn get_by_engine_on_empty_registry_returns_none() {
        let registry = ModelRegistry::new();
        assert!(registry.get_by_engine(ModelEngine::Qwen).is_none());
    }

    #[test]
    fn targeted_load_ignores_an_unrelated_broken_manifest() {
        let root = test_root("targeted");
        write_single_file_model(&root, "fixed-qwen", "qwen", b"verified model");
        std::fs::write(root.join("broken.manifest.json"), b"not-json")
            .expect("broken manifest fixture");

        let mut registry = ModelRegistry::new();
        let entry = registry
            .load_model(&root, "fixed-qwen", ModelEngine::Qwen, true)
            .expect("targeted model must load");

        assert_eq!(entry.manifest.model_id, "fixed-qwen");
        assert_eq!(registry.len(), 1);
        std::fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn targeted_load_preserves_hash_mismatch_as_a_distinct_error() {
        let root = test_root("hash-mismatch");
        write_single_file_model(&root, "fixed-whisper", "whisper", b"original");
        std::fs::write(root.join("fixed-whisper.gguf"), b"tampered").expect("tamper model");

        let error = ModelRegistry::new()
            .load_model(&root, "fixed-whisper", ModelEngine::Whisper, true)
            .expect_err("tampered model must fail");

        assert!(matches!(
            error,
            RegistryError::Manifest(ManifestError::PackageHashMismatch { .. })
        ));
        std::fs::remove_dir_all(root).expect("remove test root");
    }
}
