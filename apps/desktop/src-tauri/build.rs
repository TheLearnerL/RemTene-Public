fn main() {
    // Only the formal packaging entry may migrate an installed LaunchAgent.
    // Plain Tauri/CI bundles are deliberately marked unverified, while the
    // ad-hoc helper build is explicitly non-production.
    println!("cargo:rerun-if-env-changed=REMTENE_MACOS_BUILD_FLAVOR");
    let build_flavor =
        std::env::var("REMTENE_MACOS_BUILD_FLAVOR").unwrap_or_else(|_| "unverified".to_owned());
    assert!(
        matches!(build_flavor.as_str(), "formal" | "adhoc" | "unverified"),
        "REMTENE_MACOS_BUILD_FLAVOR must be formal, adhoc or unset"
    );
    println!("cargo:rustc-env=REMTENE_COMPILED_MACOS_BUILD_FLAVOR={build_flavor}");

    // ADR-0008 keeps the Bundle ID suffix as a persistent Data ABI, while the
    // Apple Team ID is signing-owner data and must never be hard-coded into
    // public source. Formal packaging injects the full App Group explicitly;
    // CI and ordinary unsigned builds use a clearly non-production prefix.
    const UNVERIFIED_MACOS_APP_GROUP_ID: &str = "UNSIGNDEV0.io.github.TheLearnerL.bard.asr";
    println!("cargo:rerun-if-env-changed=REMTENE_MACOS_APP_GROUP_ID");
    let app_group_id = std::env::var("REMTENE_MACOS_APP_GROUP_ID").unwrap_or_else(|_| {
        assert_ne!(
            build_flavor, "formal",
            "REMTENE_MACOS_APP_GROUP_ID is required for a formal macOS build"
        );
        UNVERIFIED_MACOS_APP_GROUP_ID.to_owned()
    });
    assert!(
        valid_identifier(&app_group_id),
        "REMTENE_MACOS_APP_GROUP_ID must be a valid application-group identifier"
    );
    println!("cargo:rustc-env=REMTENE_COMPILED_MACOS_APP_GROUP_ID={app_group_id}");

    let app_manifest = tauri_build::AppManifest::new().commands(&[
        "app_get_snapshot",
        "model_check_health",
        "model_switch_engine",
        "model_open_directory",
        "recording_hud_get_state",
        "recording_finish",
        "recording_cancel",
        "session_start",
        "session_finish",
        "session_cancel",
        "permission_get_status",
        "permission_request_microphone",
        "permission_request_accessibility",
        "permission_open_accessibility_settings",
        "permission_open_microphone_settings",
        "settings_get",
        "settings_set_clipboard_bridge",
        "settings_set_recording_preferences",
        "settings_set_recording_shortcut",
        "settings_set_history_enabled",
        "settings_set_history_limit",
        "settings_set_history_retention",
        "settings_set_auto_copy_result",
        "settings_set_local_diagnostics",
        "diagnostics_open_directory",
        "settings_set_text_processing",
        "settings_set_llm",
        "autostart_get_status",
        "autostart_set_enabled",
        "history_list",
        "history_copy",
        "history_clear_all",
        "secret_get_llm_api_key_status",
        "secret_set_llm_api_key",
        "secret_reveal_llm_api_key",
        "secret_delete_llm_api_key",
        "secret_reset_unrecoverable_llm_secrets",
        "llm_test_connection",
        "temporary_text_get_pending",
        "temporary_text_dismiss",
        "temporary_text_copy_all",
        "notification_get_pending",
        "notification_apply_action",
    ]);
    let attributes = tauri_build::Attributes::new().app_manifest(app_manifest);

    tauri_build::try_build(attributes).expect("failed to build Tauri application metadata");
}

fn valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    (3..=255).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && !value.contains("..")
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}
