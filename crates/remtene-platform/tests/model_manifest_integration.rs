//! Integration test for model manifest and integrity verification

use remtene_platform::model_registry::ModelRegistry;
use std::path::PathBuf;

/// 校验一个由本用例自己组装的目录型模型包能被扫描并逐文件验证。
///
/// 这条用例过去写成相对路径 `pocs/asr/artifacts/...`，而集成测试的工作目录是 crate 根，
/// 路径永远不存在，于是静默 early-return——它从未真正验证过任何 manifest。现在改为自建
/// 夹具，不依赖未提交的大模型产物，也不会再无声通过。
#[test]
fn directory_package_is_scanned_and_verified_per_file() {
    use remtene_platform::model_manifest::{ModelEngine, Quantization};

    let root = std::env::temp_dir().join(format!(
        "remtene-registry-dir-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock must be after the epoch")
            .as_nanos()
    ));
    let package = root.join("qwen3-asr-0.6b-v1");
    std::fs::create_dir_all(&package).expect("package directory must be creatable");
    std::fs::write(package.join("model.safetensors"), b"weights").expect("weights");
    std::fs::write(package.join("vocab.json"), b"vocab").expect("vocab");

    let manifest = format!(
        r#"{{
  "schema_version": 1,
  "model_id": "qwen3-asr-0.6b-v1",
  "engine": "qwen",
  "architecture": "0.6b",
  "quantization": "int8",
  "version": "1.0.0",
  "platform": "any",
  "package_files": [
    {{ "path": "model.safetensors", "sha256": "{weights}" }},
    {{ "path": "vocab.json", "sha256": "{vocab}" }}
  ],
  "worker_compat": ">=0.1.0",
  "license": {{ "spdx_id": "Apache-2.0", "url": "https://www.apache.org/licenses/LICENSE-2.0" }}
}}"#,
        weights = sha256_hex(&package.join("model.safetensors")),
        vocab = sha256_hex(&package.join("vocab.json")),
    );
    std::fs::write(root.join("qwen3-asr-0.6b-v1.manifest.json"), manifest).expect("manifest");

    let mut registry = ModelRegistry::new();
    let loaded = registry
        .scan_directory(&root, true)
        .expect("directory package must pass per-file verification");
    assert_eq!(loaded, 1);

    let entry = registry
        .get("qwen3-asr-0.6b-v1")
        .expect("scanned model must resolve by ID");
    assert_eq!(entry.manifest.engine, ModelEngine::Qwen);
    assert_eq!(entry.manifest.quantization, Quantization::Int8);
    assert!(entry.manifest.is_directory_package());
    assert_eq!(entry.package_path, package);

    // 篡改包内任意文件后必须失败关闭。
    std::fs::write(package.join("vocab.json"), b"tampered").expect("tamper");
    assert!(
        ModelRegistry::new().scan_directory(&root, true).is_err(),
        "a tampered directory package must be rejected"
    );

    std::fs::remove_dir_all(&root).ok();
}

fn sha256_hex(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).expect("fixture file must be readable");
    format!("{:x}", Sha256::digest(&bytes))
}

/// 校验本机 App Group 中已激活的模型包确实通过 SHA-256 完整性验证。
///
/// 这条用例读取真实用户目录，因此默认门控；QA-VS1-002 前显式运行它，可以在启动应用之前
/// 证明 `models/active` 里的包不是靠跳过校验才被接受的。
#[test]
#[ignore = "requires REMTENE_RUN_LIVE_MODEL_SCAN=1 and installed packages in the App Group"]
fn live_active_models_pass_integrity_verification() {
    assert_eq!(
        std::env::var("REMTENE_RUN_LIVE_MODEL_SCAN").as_deref(),
        Ok("1"),
        "set REMTENE_RUN_LIVE_MODEL_SCAN=1 explicitly before reading the real App Group"
    );

    let app_group_id = std::env::var("REMTENE_MACOS_APP_GROUP_ID")
        .expect("REMTENE_MACOS_APP_GROUP_ID must name the signed App Group");
    assert!(
        !app_group_id.contains('/') && !app_group_id.contains(".."),
        "App Group must be one safe path component"
    );

    let active = PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
        .join("Library/Group Containers")
        .join(app_group_id)
        .join("RemTene/ASR/models/active");

    let mut registry = ModelRegistry::new();
    let loaded = registry
        .scan_directory(&active, true)
        .expect("every active package must pass manifest and SHA-256 verification");

    assert!(loaded > 0, "no verified package in {}", active.display());
    for model_id in registry.list_model_ids() {
        let entry = registry.get(&model_id).expect("scanned model must resolve");
        println!(
            "✓ {} ({:?}) verified at {}",
            entry.manifest.model_id,
            entry.manifest.engine,
            entry.package_path.display()
        );
    }
}

#[test]
fn test_corrupted_model_rejected() {
    // Create a temporary directory with a corrupted model
    let temp_dir = std::env::temp_dir().join("remtene-test-corrupted");
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Create a manifest with incorrect hash
    let manifest_content = r#"{
  "schema_version": 1,
  "model_id": "test-corrupted",
  "engine": "qwen",
  "architecture": "test",
  "quantization": "int8",
  "version": "1.0.0",
  "platform": "any",
  "package_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "worker_compat": ">=0.1.0",
  "license": {
    "spdx_id": "Apache-2.0",
    "url": "https://example.com"
  },
  "description": "Test model"
}"#;

    std::fs::write(
        temp_dir.join("test-corrupted.manifest.json"),
        manifest_content,
    )
    .unwrap();

    // Create a dummy model file
    std::fs::write(temp_dir.join("test-corrupted.gguf"), b"fake model data").unwrap();

    let mut registry = ModelRegistry::new();

    // Try to scan with integrity verification - should fail
    let result = registry.scan_directory(&temp_dir, true);

    assert!(result.is_err(), "Should reject corrupted model");
    println!("✓ Corrupted model correctly rejected");

    // Cleanup
    std::fs::remove_dir_all(&temp_dir).ok();
}

#[test]
fn test_manifest_without_integrity_check() {
    let temp_dir = std::env::temp_dir().join("remtene-test-no-verify");
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Create a manifest with incorrect hash
    let manifest_content = r#"{
  "schema_version": 1,
  "model_id": "test-no-verify",
  "engine": "qwen",
  "architecture": "test",
  "quantization": "int8",
  "version": "1.0.0",
  "platform": "any",
  "package_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
  "worker_compat": ">=0.1.0",
  "license": {
    "spdx_id": "Apache-2.0",
    "url": "https://example.com"
  },
  "description": "Test model"
}"#;

    std::fs::write(
        temp_dir.join("test-no-verify.manifest.json"),
        manifest_content,
    )
    .unwrap();
    std::fs::write(temp_dir.join("test-no-verify.gguf"), b"fake model data").unwrap();

    let mut registry = ModelRegistry::new();

    // Scan without integrity verification - should succeed
    let result = registry.scan_directory(&temp_dir, false);

    assert!(result.is_ok(), "Should load without integrity check");
    assert_eq!(result.unwrap(), 1, "Should load 1 model");
    println!("✓ Model loaded without integrity check (development mode)");

    // Cleanup
    std::fs::remove_dir_all(&temp_dir).ok();
}
