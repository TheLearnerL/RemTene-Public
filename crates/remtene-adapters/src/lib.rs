//! Concrete non-platform adapters. Implementations arrive only after their POC contracts are proven.

pub mod asr_worker;
pub mod audio_resolver;
pub mod clipboard_bridge;
pub mod clipboard_text_writer;
pub mod clock;
pub mod diagnostics;
pub mod file_history_store;
pub mod file_settings_store;
pub mod history_store;
pub mod id_generator;
pub mod llm;
pub mod llm_provider;
pub mod local_encrypted_secret_store;
pub mod model;
pub mod output_adapter;
pub mod settings_store;
pub mod storage;
pub mod stub_ports;
pub mod temporary_text;
