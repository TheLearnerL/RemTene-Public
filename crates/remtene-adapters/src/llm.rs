//! OpenAI-compatible text provider adapter.
//!
//! Provider HTTP DTOs and credentials stay inside this module. Audio, history,
//! target handles and secret values are never accepted through the canonical
//! text-processing request.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::Poll;
use std::time::Duration;

use futures::future::{Either, poll_fn, select};
use futures::task::AtomicWaker;
use remtene_application::ports::{
    LlmConnectionProbeError, LlmProvider, LlmRouteCandidate, LlmRouteResolution, LlmUpstreamError,
    PortError, PortFuture, ResolvedLlmRoute, SecretMaterialState, SecretStore,
    TextProcessingRequest, TextProcessingResult,
};
use remtene_application::{LLM_OUTPUT_SCHEMA_JSON, compose_llm_prompt};
use remtene_domain::RequestId;
use reqwest::{Client, StatusCode, Url, redirect};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::llm_provider::parse_canonical_response;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_UPSTREAM_DIAGNOSTIC_BYTES: usize = 16 * 1024;
const MAX_DIAGNOSTIC_API_KEY_BYTES: usize = 8 * 1024;
const OPENAI_COMPATIBLE_PROVIDER_REF: &str = "primary";
const OPENAI_COMPATIBLE_SECRET_PREFIX: &str = "llm.openai_compatible";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StructuredOutputMode {
    /// Send only the strict four-part prompt and validate the returned content locally.
    ///
    /// This is the widest OpenAI-Compatible surface and remains fail-closed because the
    /// canonical parser accepts exactly one closed JSON object.
    #[default]
    PromptOnly,
    /// Also request OpenAI JSON Schema structured output.
    JsonSchema,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenAiCompatiblePolicy {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_response_bytes: usize,
}

impl Default for OpenAiCompatiblePolicy {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

pub struct OpenAiCompatibleLlmProvider {
    client: Client,
    direct_client: Client,
    secrets: Arc<dyn SecretStore>,
    policy: OpenAiCompatiblePolicy,
    structured_output: StructuredOutputMode,
    active: Mutex<HashMap<RequestId, Arc<CancellationSignal>>>,
}

impl OpenAiCompatibleLlmProvider {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Result<Self, PortError> {
        Self::with_policy(secrets, OpenAiCompatiblePolicy::default())
    }

    pub fn with_policy(
        secrets: Arc<dyn SecretStore>,
        policy: OpenAiCompatiblePolicy,
    ) -> Result<Self, PortError> {
        Self::with_policy_and_structured_output(secrets, policy, StructuredOutputMode::PromptOnly)
    }

    pub fn with_policy_and_structured_output(
        secrets: Arc<dyn SecretStore>,
        policy: OpenAiCompatiblePolicy,
        structured_output: StructuredOutputMode,
    ) -> Result<Self, PortError> {
        if policy.request_timeout.is_zero()
            || policy.connect_timeout.is_zero()
            || policy.max_response_bytes == 0
        {
            return Err(port_error("llm.invalid_config", false));
        }

        let client = build_http_client(policy, false)?;
        // A loopback route is a local trust boundary, not merely another URL.
        // It must never inherit HTTP_PROXY/ALL_PROXY and disclose its Bearer
        // credential or transcript to an intermediary.
        let direct_client = build_http_client(policy, true)?;

        Ok(Self {
            client,
            direct_client,
            secrets,
            policy,
            structured_output,
            active: Mutex::new(HashMap::new()),
        })
    }

    async fn resolve_candidate(&self, candidate: Option<LlmRouteCandidate>) -> LlmRouteResolution {
        let Some(candidate) = candidate else {
            return LlmRouteResolution::NoConfiguration;
        };
        let route = match resolve_openai_compatible_route(&candidate) {
            Ok(route) => route,
            Err(error) => {
                return LlmRouteResolution::Unavailable { route: None, error };
            }
        };

        match self.secrets.inspect(route.secret_id()).await {
            Ok(SecretMaterialState::Configured) => LlmRouteResolution::Ready(route),
            Ok(SecretMaterialState::NotConfigured) => LlmRouteResolution::MissingSecret(route),
            Ok(SecretMaterialState::RecoveryRequired) => LlmRouteResolution::Unavailable {
                route: Some(route),
                error: port_error("llm.secret_recovery_required", false),
            },
            Err(_) => LlmRouteResolution::Unavailable {
                route: Some(route),
                error: port_error("llm.secret_unavailable", true),
            },
        }
    }

    fn validate_frozen_route(route: &ResolvedLlmRoute) -> Result<Url, PortError> {
        if route.provider_ref() != OPENAI_COMPATIBLE_PROVIDER_REF || route.model().trim().is_empty()
        {
            return Err(port_error("llm.invalid_request", false));
        }

        let endpoint = chat_completions_endpoint(route.endpoint())?;
        if endpoint.as_str() != route.endpoint()
            || route.secret_id() != secret_id_for_endpoint(&endpoint)
        {
            return Err(port_error("llm.invalid_config", false));
        }
        Ok(endpoint)
    }

    async fn execute(
        &self,
        route: ResolvedLlmRoute,
        request: TextProcessingRequest,
        signal: Arc<CancellationSignal>,
        include_upstream_diagnostic: bool,
    ) -> Result<TextProcessingResult, LlmConnectionProbeError> {
        let endpoint =
            Self::validate_frozen_route(&route).map_err(LlmConnectionProbeError::from_port)?;
        let prompt = compose_llm_prompt(&request)
            .map_err(|_| probe_port_error("llm.invalid_request", false))?;
        let secret = self
            .secrets
            .read(route.secret_id())
            .await
            .map_err(|_| probe_port_error("llm.secret_unavailable", true))?
            .ok_or_else(|| probe_port_error("llm.api_key_missing", false))?;

        if signal.is_cancelled() {
            return Err(probe_port_error("llm.cancelled", false));
        }

        let body = ChatCompletionsRequest {
            model: route.model().to_owned(),
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: prompt.system_message,
                },
                ChatMessage {
                    role: "user",
                    content: prompt.user_message,
                },
            ],
            response_format: match self.structured_output {
                StructuredOutputMode::PromptOnly => None,
                StructuredOutputMode::JsonSchema => Some(JsonSchemaResponseFormat {
                    kind: "json_schema",
                    json_schema: JsonSchemaDefinition {
                        name: "remtene_text_processing_v1",
                        strict: true,
                        schema: serde_json::from_str(LLM_OUTPUT_SCHEMA_JSON)
                            .map_err(|_| probe_port_error("llm.invalid_request", false))?,
                    },
                }),
            },
        };

        let http = self
            .send_request(
                endpoint,
                secret.expose(),
                &body,
                include_upstream_diagnostic,
            )
            .await?;
        if signal.is_cancelled() {
            return Err(probe_port_error("llm.cancelled", false));
        }

        let envelope: ChatCompletionsResponse =
            serde_json::from_slice(&http.body).map_err(|_| {
                response_probe_error(
                    "llm.invalid_response",
                    include_upstream_diagnostic,
                    &http,
                    secret.expose(),
                )
            })?;
        let [choice] = envelope.choices.as_slice() else {
            return Err(response_probe_error(
                "llm.invalid_response",
                include_upstream_diagnostic,
                &http,
                secret.expose(),
            ));
        };
        let content = choice.message.content.as_deref().ok_or_else(|| {
            response_probe_error(
                "llm.invalid_response",
                include_upstream_diagnostic,
                &http,
                secret.expose(),
            )
        })?;
        if content.trim().is_empty() {
            return Err(response_probe_error(
                "llm.invalid_response",
                include_upstream_diagnostic,
                &http,
                secret.expose(),
            ));
        }
        parse_canonical_response(request.session_id, request.request_id, content)
            .map_err(|_| probe_port_error("llm.invalid_canonical_response", false))
    }

    async fn send_request(
        &self,
        endpoint: Url,
        api_key: &str,
        body: &ChatCompletionsRequest,
        include_upstream_diagnostic: bool,
    ) -> Result<ProviderHttpResponse, LlmConnectionProbeError> {
        let client = if is_loopback(&endpoint) {
            &self.direct_client
        } else {
            &self.client
        };
        let mut response = client
            .post(endpoint)
            .bearer_auth(api_key)
            .timeout(self.policy.request_timeout)
            .json(body)
            .send()
            .await
            .map_err(|error| LlmConnectionProbeError::from_port(map_reqwest_error(error)))?;

        let status = response.status();
        if !status.is_success() {
            let error = map_status(status);
            if !include_upstream_diagnostic {
                return Err(LlmConnectionProbeError::from_port(error));
            }
            return Err(LlmConnectionProbeError::with_upstream(
                error,
                read_upstream_diagnostic(&mut response, status, api_key).await,
            ));
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| LlmConnectionProbeError::from_port(map_reqwest_error(error)))?
        {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| probe_port_error("llm.response_too_large", false))?;
            if next_len > self.policy.max_response_bytes {
                return Err(probe_port_error("llm.response_too_large", false));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(ProviderHttpResponse {
            status,
            body: bytes,
        })
    }

    fn register(&self, request_id: RequestId) -> Result<Arc<CancellationSignal>, PortError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| port_error("llm.internal", false))?;
        if active.contains_key(&request_id) {
            return Err(port_error("llm.request_duplicate", false));
        }
        let signal = Arc::new(CancellationSignal::default());
        active.insert(request_id, Arc::clone(&signal));
        Ok(signal)
    }

    fn unregister(&self, request_id: RequestId, signal: &Arc<CancellationSignal>) {
        if let Ok(mut active) = self.active.lock()
            && active
                .get(&request_id)
                .is_some_and(|current| Arc::ptr_eq(current, signal))
        {
            active.remove(&request_id);
        }
    }
}

impl LlmProvider for OpenAiCompatibleLlmProvider {
    fn resolve_route(
        &self,
        candidate: Option<LlmRouteCandidate>,
    ) -> PortFuture<'_, LlmRouteResolution> {
        Box::pin(async move { self.resolve_candidate(candidate).await })
    }

    fn process(
        &self,
        route: ResolvedLlmRoute,
        request: TextProcessingRequest,
    ) -> PortFuture<'_, Result<TextProcessingResult, PortError>> {
        Box::pin(async move {
            let request_id = request.request_id;
            let signal = self.register(request_id)?;
            let _active_request = ActiveRequestGuard {
                provider: self,
                request_id,
                signal: Arc::clone(&signal),
            };
            let request_future = Box::pin(self.execute(route, request, Arc::clone(&signal), false));
            let cancellation = Box::pin(signal.wait());

            match select(request_future, cancellation).await {
                Either::Left((_result, _)) if signal.is_cancelled() => {
                    Err(port_error("llm.cancelled", false))
                }
                Either::Left((result, _)) => {
                    result.map_err(LlmConnectionProbeError::into_port_error)
                }
                Either::Right(((), _)) => Err(port_error("llm.cancelled", false)),
            }
        })
    }

    fn probe_connection(
        &self,
        route: ResolvedLlmRoute,
        request: TextProcessingRequest,
    ) -> PortFuture<'_, Result<TextProcessingResult, LlmConnectionProbeError>> {
        Box::pin(async move {
            let request_id = request.request_id;
            let signal = self
                .register(request_id)
                .map_err(LlmConnectionProbeError::from_port)?;
            let _active_request = ActiveRequestGuard {
                provider: self,
                request_id,
                signal: Arc::clone(&signal),
            };
            let request_future = Box::pin(self.execute(route, request, Arc::clone(&signal), true));
            let cancellation = Box::pin(signal.wait());

            match select(request_future, cancellation).await {
                Either::Left((_result, _)) if signal.is_cancelled() => {
                    Err(probe_port_error("llm.cancelled", false))
                }
                Either::Left((result, _)) => result,
                Either::Right(((), _)) => Err(probe_port_error("llm.cancelled", false)),
            }
        })
    }

    fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let signal = self
                .active
                .lock()
                .map_err(|_| port_error("llm.internal", false))?
                .get(&request_id)
                .cloned();
            if let Some(signal) = signal {
                signal.cancel();
            }
            Ok(())
        })
    }
}

struct ActiveRequestGuard<'a> {
    provider: &'a OpenAiCompatibleLlmProvider,
    request_id: RequestId,
    signal: Arc<CancellationSignal>,
}

struct ProviderHttpResponse {
    status: StatusCode,
    body: Vec<u8>,
}

impl Drop for ActiveRequestGuard<'_> {
    fn drop(&mut self) {
        self.provider.unregister(self.request_id, &self.signal);
    }
}

#[derive(Default)]
struct CancellationSignal {
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

impl CancellationSignal {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.waker.wake();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        poll_fn(|context| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            self.waker.register(context.waker());
            if self.is_cancelled() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

#[derive(Serialize)]
struct ChatCompletionsRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<JsonSchemaResponseFormat>,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Serialize)]
struct JsonSchemaResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: JsonSchemaDefinition,
}

#[derive(Serialize)]
struct JsonSchemaDefinition {
    name: &'static str,
    strict: bool,
    schema: Value,
}

#[derive(Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    message: ChatCompletionMessage,
}

#[derive(Deserialize)]
struct ChatCompletionMessage {
    content: Option<String>,
}

fn build_http_client(
    policy: OpenAiCompatiblePolicy,
    disable_system_proxy: bool,
) -> Result<Client, PortError> {
    let builder = Client::builder()
        .connect_timeout(policy.connect_timeout)
        .redirect(redirect::Policy::none());
    let builder = if disable_system_proxy {
        builder.no_proxy()
    } else {
        builder
    };
    builder
        .build()
        .map_err(|_| port_error("llm.client_unavailable", false))
}

fn chat_completions_endpoint(base_url: &str) -> Result<Url, PortError> {
    let mut url = Url::parse(base_url).map_err(|_| port_error("llm.invalid_config", false))?;
    if url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(port_error("llm.invalid_config", false));
    }

    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&url) => {}
        _ => return Err(port_error("llm.invalid_config", false)),
    }

    let current = url.path().trim_end_matches('/');
    if !current.ends_with("/chat/completions") {
        let path = if current.is_empty() {
            "/chat/completions".to_owned()
        } else {
            format!("{current}/chat/completions")
        };
        url.set_path(&path);
    }
    Ok(url)
}

pub(crate) fn resolve_openai_compatible_route(
    candidate: &LlmRouteCandidate,
) -> Result<ResolvedLlmRoute, PortError> {
    let model = candidate.model().trim();
    if model.is_empty() {
        return Err(port_error("llm.invalid_config", false));
    }
    let endpoint = chat_completions_endpoint(candidate.base_url().trim())?;
    Ok(ResolvedLlmRoute::new(
        OPENAI_COMPATIBLE_PROVIDER_REF,
        endpoint.as_str(),
        model,
        secret_id_for_endpoint(&endpoint),
    ))
}

fn secret_id_for_endpoint(endpoint: &Url) -> String {
    let fingerprint = Sha256::digest(endpoint.as_str().as_bytes());
    format!("{OPENAI_COMPATIBLE_SECRET_PREFIX}.{fingerprint:x}")
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn map_reqwest_error(error: reqwest::Error) -> PortError {
    if error.is_timeout() {
        port_error("llm.timeout", true)
    } else if error.is_builder() {
        port_error("llm.invalid_request", false)
    } else {
        port_error("llm.network", true)
    }
}

fn map_status(status: StatusCode) -> PortError {
    match status.as_u16() {
        401 => port_error("llm.authentication_failed", false),
        403 => port_error("llm.permission_denied", false),
        408 => port_error("llm.timeout", true),
        429 => port_error("llm.rate_limited", true),
        500..=599 => port_error("llm.provider_unavailable", true),
        _ => port_error("llm.request_rejected", false),
    }
}

async fn read_upstream_diagnostic(
    response: &mut reqwest::Response,
    status: StatusCode,
    api_key: &str,
) -> LlmUpstreamError {
    // Read enough overlap to remove a key that begins immediately before the
    // visible limit. API key input is capped at 8 KiB by the Application
    // controller; the defensive min keeps this diagnostic allocation bounded
    // even if a malformed SecretStore implementation violates that contract.
    let read_limit = MAX_UPSTREAM_DIAGNOSTIC_BYTES
        .saturating_add(api_key.len().min(MAX_DIAGNOSTIC_API_KEY_BYTES));
    let mut raw = Vec::with_capacity(read_limit.min(32 * 1024));
    let mut source_truncated = false;

    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = read_limit.saturating_sub(raw.len());
                if chunk.len() > remaining {
                    raw.extend_from_slice(&chunk[..remaining]);
                    source_truncated = true;
                    break;
                }
                raw.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                source_truncated = true;
                break;
            }
        }
    }

    upstream_error_from_bytes(status, &raw, api_key, source_truncated)
}

fn response_probe_error(
    code: &str,
    include_upstream_diagnostic: bool,
    response: &ProviderHttpResponse,
    api_key: &str,
) -> LlmConnectionProbeError {
    let error = port_error(code, false);
    if include_upstream_diagnostic {
        LlmConnectionProbeError::with_upstream(
            error,
            upstream_error_from_bytes(response.status, &response.body, api_key, false),
        )
    } else {
        LlmConnectionProbeError::from_port(error)
    }
}

fn upstream_error_from_bytes(
    status: StatusCode,
    raw: &[u8],
    api_key: &str,
    source_truncated: bool,
) -> LlmUpstreamError {
    let read_limit = MAX_UPSTREAM_DIAGNOSTIC_BYTES
        .saturating_add(api_key.len().min(MAX_DIAGNOSTIC_API_KEY_BYTES));
    let visible_raw = &raw[..raw.len().min(read_limit)];
    let decoded = String::from_utf8_lossy(visible_raw);
    let sanitized = sanitize_upstream_diagnostic(&decoded, api_key);
    let (bounded, output_truncated) =
        truncate_utf8_bytes(&sanitized, MAX_UPSTREAM_DIAGNOSTIC_BYTES);
    LlmUpstreamError::new(
        status.as_u16(),
        bounded,
        source_truncated || raw.len() > read_limit || output_truncated,
    )
}

fn sanitize_upstream_diagnostic(input: &str, api_key: &str) -> String {
    let normalized = normalize_diagnostic_controls(input);
    let mut redacted = normalized;
    if !api_key.is_empty() {
        redacted = redacted.replace(api_key, "[REDACTED_API_KEY]");
        if let Ok(encoded) = serde_json::to_string(api_key)
            && let Some(encoded_inner) = encoded
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
            && encoded_inner != api_key
        {
            redacted = redacted.replace(encoded_inner, "[REDACTED_API_KEY]");
        }
    }
    redact_bearer_tokens(&redacted)
}

fn normalize_diagnostic_controls(input: &str) -> String {
    let normalized_newlines = input.replace("\r\n", "\n").replace('\r', "\n");
    normalized_newlines
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || !character.is_control() {
                character
            } else {
                '\u{fffd}'
            }
        })
        .collect()
}

fn redact_bearer_tokens(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_start) = lower[cursor..].find("bearer") {
        let start = cursor + relative_start;
        let prefix_is_boundary = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        let whitespace_start = start + "bearer".len();
        if !prefix_is_boundary
            || whitespace_start >= input.len()
            || !input.as_bytes()[whitespace_start].is_ascii_whitespace()
        {
            let next = whitespace_start.min(input.len());
            output.push_str(&input[cursor..next]);
            cursor = next;
            continue;
        }

        let mut token_start = whitespace_start;
        while token_start < input.len() && input.as_bytes()[token_start].is_ascii_whitespace() {
            token_start += 1;
        }
        let token_end = input[token_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace()
                    || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']' | ')' | '>'))
                .then_some(token_start + offset)
            })
            .unwrap_or(input.len());
        if token_end == token_start {
            output.push_str(&input[cursor..token_start]);
            cursor = token_start;
            continue;
        }

        output.push_str(&input[cursor..token_start]);
        output.push_str("[REDACTED_BEARER_TOKEN]");
        cursor = token_end;
    }

    output.push_str(&input[cursor..]);
    output
}

fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_owned(), false);
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    (input[..end].to_owned(), true)
}

fn probe_port_error(code: &str, retryable: bool) -> LlmConnectionProbeError {
    LlmConnectionProbeError::from_port(port_error(code, retryable))
}

fn port_error(code: &str, retryable: bool) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: code.to_owned(),
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use remtene_application::ports::SecretValue;
    use remtene_domain::{ProcessingMode, SessionId};
    use serde_json::json;

    use super::*;

    struct MemorySecretStore {
        values: Mutex<HashMap<String, String>>,
    }

    impl MemorySecretStore {
        fn empty() -> Self {
            Self {
                values: Mutex::new(HashMap::new()),
            }
        }

        fn with(secret_id: &str, value: &str) -> Self {
            Self {
                values: Mutex::new(HashMap::from([(secret_id.to_owned(), value.to_owned())])),
            }
        }
    }

    struct FailingSecretStore;

    impl SecretStore for FailingSecretStore {
        fn is_configured(&self, _secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
            Box::pin(async { Err(port_error("secret.authentication_failed", false)) })
        }

        fn read(&self, _secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
            Box::pin(async { Err(port_error("secret.authentication_failed", false)) })
        }

        fn replace(
            &self,
            _secret_id: &str,
            _value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Err(port_error("secret.authentication_failed", false)) })
        }

        fn delete(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Err(port_error("secret.authentication_failed", false)) })
        }
    }

    struct PendingSecretStore;

    impl SecretStore for PendingSecretStore {
        fn is_configured(&self, _secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
            Box::pin(std::future::pending())
        }

        fn read(&self, _secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
            Box::pin(std::future::pending())
        }

        fn replace(
            &self,
            _secret_id: &str,
            _value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(std::future::pending())
        }

        fn delete(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(std::future::pending())
        }
    }

    impl SecretStore for MemorySecretStore {
        fn is_configured(&self, secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
            let secret_id = secret_id.to_owned();
            Box::pin(async move {
                Ok(self
                    .values
                    .lock()
                    .map_err(|_| port_error("secret.unavailable", false))?
                    .contains_key(&secret_id))
            })
        }

        fn read(&self, secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
            let secret_id = secret_id.to_owned();
            Box::pin(async move {
                Ok(self
                    .values
                    .lock()
                    .map_err(|_| port_error("secret.unavailable", false))?
                    .get(&secret_id)
                    .map(|value| SecretValue::new(value.clone())))
            })
        }

        fn replace(
            &self,
            secret_id: &str,
            value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            let secret_id = secret_id.to_owned();
            Box::pin(async move {
                self.values
                    .lock()
                    .map_err(|_| port_error("secret.unavailable", false))?
                    .insert(secret_id, value.expose().to_owned());
                Ok(())
            })
        }

        fn delete(&self, secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
            let secret_id = secret_id.to_owned();
            Box::pin(async move {
                self.values
                    .lock()
                    .map_err(|_| port_error("secret.unavailable", false))?
                    .remove(&secret_id);
                Ok(())
            })
        }
    }

    fn text_request(request_id: RequestId) -> TextProcessingRequest {
        TextProcessingRequest {
            session_id: SessionId::new(),
            request_id,
            processing_mode: ProcessingMode::Faithful,
            raw_transcript: "请保留数字 12.5，并且不要改变否定含义。".to_owned(),
            selected_text: None,
        }
    }

    struct TestProvider {
        inner: OpenAiCompatibleLlmProvider,
        route: ResolvedLlmRoute,
    }

    impl TestProvider {
        fn process(
            &self,
            request: TextProcessingRequest,
        ) -> PortFuture<'_, Result<TextProcessingResult, PortError>> {
            self.inner.process(self.route.clone(), request)
        }

        fn probe_connection(
            &self,
            request: TextProcessingRequest,
        ) -> PortFuture<'_, Result<TextProcessingResult, LlmConnectionProbeError>> {
            self.inner.probe_connection(self.route.clone(), request)
        }

        fn cancel(&self, request_id: RequestId) -> PortFuture<'_, Result<(), PortError>> {
            self.inner.cancel(request_id)
        }
    }

    struct OneShotServer {
        base_url: String,
        request: mpsc::Receiver<Vec<u8>>,
    }

    fn one_shot_server(
        status_line: &str,
        response_body: String,
        delay: Duration,
        extra_headers: &[(&str, &str)],
    ) -> OneShotServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let (request_tx, request_rx) = mpsc::channel();
        let status_line = status_line.to_owned();
        let extra_headers = extra_headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let request = read_http_request(&mut stream);
            request_tx.send(request).expect("send captured request");
            if !delay.is_zero() {
                thread::sleep(delay);
            }

            let mut response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                response_body.len()
            );
            for (name, value) in extra_headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&response_body);
            let _ = stream.write_all(response.as_bytes());
        });

        OneShotServer {
            base_url: format!("http://{address}/v1"),
            request: request_rx,
        }
    }

    fn read_http_request(stream: &mut impl Read) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("read request");
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map(str::trim)
            .map(|value| value.parse::<usize>().expect("content length"))
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).expect("read request body");
            assert!(read > 0, "request ended before body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        bytes
    }

    fn provider(
        base_url: &str,
        policy: OpenAiCompatiblePolicy,
        structured_output: StructuredOutputMode,
    ) -> TestProvider {
        let endpoint = chat_completions_endpoint(base_url).expect("provider endpoint");
        let secret_id = secret_id_for_endpoint(&endpoint);
        let inner = OpenAiCompatibleLlmProvider::with_policy_and_structured_output(
            Arc::new(MemorySecretStore::with(&secret_id, "sk-test-VERY-SECRET")),
            policy,
            structured_output,
        )
        .expect("provider");
        TestProvider {
            inner,
            route: ResolvedLlmRoute::new(
                OPENAI_COMPATIBLE_PROVIDER_REF,
                endpoint.as_str(),
                "compatible-model",
                secret_id,
            ),
        }
    }

    fn success_envelope(final_text: &str) -> String {
        let canonical = json!({
            "schema_version": 1,
            "intent": "dictation",
            "final_text": final_text,
        })
        .to_string();
        json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": canonical,
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        })
        .to_string()
    }

    #[test]
    fn endpoint_accepts_https_and_loopback_http_but_rejects_credential_leaks() {
        let https = chat_completions_endpoint("https://api.example/v1/").expect("https");
        assert_eq!(https.as_str(), "https://api.example/v1/chat/completions");

        let localhost = chat_completions_endpoint("http://127.0.0.1:11434/v1").expect("loopback");
        assert_eq!(
            localhost.as_str(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );

        for invalid in [
            "http://api.example/v1",
            "https://user:password@api.example/v1",
            "https://api.example/v1?token=secret",
            "file:///tmp/provider",
        ] {
            assert!(chat_completions_endpoint(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn route_resolution_distinguishes_absent_key_ready_and_unavailable_storage() {
        let secrets = Arc::new(MemorySecretStore::empty());
        let provider = OpenAiCompatibleLlmProvider::new(secrets.clone()).expect("provider");

        assert_eq!(
            provider.resolve_route(None).await,
            LlmRouteResolution::NoConfiguration
        );

        let candidate = LlmRouteCandidate::new("https://private.example/v1/", "private-model");
        let missing = provider.resolve_route(Some(candidate.clone())).await;
        let LlmRouteResolution::MissingSecret(route) = missing else {
            panic!("complete settings without a key must return a validated route");
        };
        assert_eq!(
            route.endpoint(),
            "https://private.example/v1/chat/completions"
        );
        assert_eq!(route.provider_ref(), OPENAI_COMPATIBLE_PROVIDER_REF);
        assert_eq!(route.model(), "private-model");
        assert_eq!(
            route.secret_id(),
            secret_id_for_endpoint(
                &chat_completions_endpoint("https://private.example/v1").expect("endpoint")
            )
        );

        secrets
            .replace(route.secret_id(), SecretValue::new("sk-private-secret"))
            .await
            .expect("save key");
        assert!(matches!(
            provider.resolve_route(Some(candidate)).await,
            LlmRouteResolution::Ready(ready) if ready == route
        ));

        let unavailable =
            OpenAiCompatibleLlmProvider::new(Arc::new(FailingSecretStore)).expect("provider");
        let resolution = unavailable
            .resolve_route(Some(LlmRouteCandidate::new(
                "https://private.example/v1",
                "private-model",
            )))
            .await;
        assert!(matches!(
            resolution,
            LlmRouteResolution::Unavailable {
                route: Some(route),
                error,
            } if route.endpoint() == "https://private.example/v1/chat/completions"
                && error.code == "llm.secret_unavailable"
                && error.retryable
        ));
    }

    #[tokio::test]
    async fn invalid_candidate_fails_closed_before_secret_resolution() {
        let provider = OpenAiCompatibleLlmProvider::new(Arc::new(MemorySecretStore::empty()))
            .expect("provider");
        for candidate in [
            LlmRouteCandidate::new("http://api.example/v1", "model"),
            LlmRouteCandidate::new("https://api.example/v1?token=secret", "model"),
            LlmRouteCandidate::new("https://api.example/v1", "   "),
        ] {
            assert!(matches!(
                provider.resolve_route(Some(candidate)).await,
                LlmRouteResolution::Unavailable { route: None, error }
                    if error.code == "llm.invalid_config"
            ));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_provider_uses_each_frozen_route_without_a_mutable_config_registry() {
        let first_server =
            one_shot_server("200 OK", success_envelope("第一条"), Duration::ZERO, &[]);
        let second_server =
            one_shot_server("200 OK", success_envelope("第二条"), Duration::ZERO, &[]);
        let secrets = Arc::new(MemorySecretStore::empty());
        let provider = OpenAiCompatibleLlmProvider::new(secrets.clone()).expect("provider");

        let first_candidate = LlmRouteCandidate::new(&first_server.base_url, "compatible-model");
        let second_candidate = LlmRouteCandidate::new(&second_server.base_url, "compatible-model");
        let LlmRouteResolution::MissingSecret(first_route) =
            provider.resolve_route(Some(first_candidate.clone())).await
        else {
            panic!("first route must be validated before its key exists");
        };
        let LlmRouteResolution::MissingSecret(second_route) =
            provider.resolve_route(Some(second_candidate.clone())).await
        else {
            panic!("second route must be validated before its key exists");
        };
        assert_ne!(first_route.secret_id(), second_route.secret_id());

        secrets
            .replace(first_route.secret_id(), SecretValue::new("sk-first"))
            .await
            .expect("first key");
        secrets
            .replace(second_route.secret_id(), SecretValue::new("sk-second"))
            .await
            .expect("second key");

        let LlmRouteResolution::Ready(first_route) =
            provider.resolve_route(Some(first_candidate)).await
        else {
            panic!("first route ready");
        };
        let LlmRouteResolution::Ready(second_route) =
            provider.resolve_route(Some(second_candidate)).await
        else {
            panic!("second route ready");
        };
        assert_eq!(
            provider
                .process(first_route, text_request(RequestId::new()))
                .await
                .expect("first request")
                .final_text,
            "第一条"
        );
        assert_eq!(
            provider
                .process(second_route, text_request(RequestId::new()))
                .await
                .expect("second request")
                .final_text,
            "第二条"
        );

        let first = String::from_utf8(
            first_server
                .request
                .recv_timeout(Duration::from_secs(1))
                .expect("first captured"),
        )
        .expect("first utf8");
        let second = String::from_utf8(
            second_server
                .request
                .recv_timeout(Duration::from_secs(1))
                .expect("second captured"),
        )
        .expect("second utf8");
        assert!(first.to_ascii_lowercase().contains("bearer sk-first"));
        assert!(second.to_ascii_lowercase().contains("bearer sk-second"));
        assert!(!first.contains("sk-second"));
        assert!(!second.contains("sk-first"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_route_uses_the_explicit_no_proxy_client() {
        let server = one_shot_server("200 OK", success_envelope("本机直连"), Duration::ZERO, &[]);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy sentinel");
        proxy_listener
            .set_nonblocking(true)
            .expect("nonblocking proxy sentinel");
        let proxy_address = proxy_listener.local_addr().expect("proxy address");
        let (proxy_hit_tx, proxy_hit_rx) = mpsc::channel();
        let proxy_worker = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while std::time::Instant::now() < deadline {
                match proxy_listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_http_request(&mut stream);
                        proxy_hit_tx.send(request).expect("report proxy hit");
                        let _ = stream.write_all(
                            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("proxy sentinel accept failed: {error}"),
                }
            }
        });

        let policy = OpenAiCompatiblePolicy {
            request_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        };
        let endpoint = chat_completions_endpoint(&server.base_url).expect("loopback endpoint");
        let secret_id = secret_id_for_endpoint(&endpoint);
        let proxied_client = Client::builder()
            .connect_timeout(policy.connect_timeout)
            .redirect(redirect::Policy::none())
            .proxy(
                reqwest::Proxy::all(format!("http://{proxy_address}"))
                    .expect("explicit test proxy"),
            )
            .build()
            .expect("proxied client");
        let inner = OpenAiCompatibleLlmProvider {
            client: proxied_client,
            direct_client: build_http_client(policy, true).expect("direct client"),
            secrets: Arc::new(MemorySecretStore::with(&secret_id, "sk-loopback")),
            policy,
            structured_output: StructuredOutputMode::PromptOnly,
            active: Mutex::new(HashMap::new()),
        };
        let provider = TestProvider {
            inner,
            route: ResolvedLlmRoute::new(
                OPENAI_COMPATIBLE_PROVIDER_REF,
                endpoint.as_str(),
                "compatible-model",
                secret_id,
            ),
        };

        let result = provider
            .process(text_request(RequestId::new()))
            .await
            .expect("loopback request must bypass the configured proxy");
        assert_eq!(result.final_text, "本机直连");
        server
            .request
            .recv_timeout(Duration::from_secs(1))
            .expect("loopback server receives the request");
        proxy_worker.join().expect("proxy sentinel worker");
        assert!(
            proxy_hit_rx.try_recv().is_err(),
            "loopback credentials and content must never reach a proxy"
        );
    }

    #[test]
    fn endpoint_fingerprint_binds_secret_id_and_debug_redacts_route() {
        let first =
            chat_completions_endpoint("https://private.example/v1/").expect("first endpoint");
        let equivalent = chat_completions_endpoint("https://private.example/v1/chat/completions")
            .expect("equivalent endpoint");
        let other = chat_completions_endpoint("https://other.example/v1").expect("other endpoint");
        let first_id = secret_id_for_endpoint(&first);
        assert_eq!(first_id, secret_id_for_endpoint(&equivalent));
        assert_ne!(first_id, secret_id_for_endpoint(&other));
        assert!(!first_id.contains("private.example"));

        let route = ResolvedLlmRoute::new(
            "private-provider",
            first.as_str(),
            "private-model",
            first_id.clone(),
        );
        let route_debug = format!("{route:?}");
        let candidate_debug = format!(
            "{:?}",
            LlmRouteCandidate::new("https://private.example/v1", "private-model")
        );
        for forbidden in [
            "private-provider",
            "private.example",
            "private-model",
            first_id.as_str(),
        ] {
            assert!(!route_debug.contains(forbidden));
            assert!(!candidate_debug.contains(forbidden));
        }
    }

    #[test]
    fn frozen_route_rejects_endpoint_or_secret_id_tampering() {
        let endpoint = chat_completions_endpoint("https://private.example/v1").expect("endpoint");
        for route in [
            ResolvedLlmRoute::new(
                OPENAI_COMPATIBLE_PROVIDER_REF,
                endpoint.as_str(),
                "compatible-model",
                "llm.openai_compatible.forged",
            ),
            ResolvedLlmRoute::new(
                OPENAI_COMPATIBLE_PROVIDER_REF,
                "https://other.example/v1",
                "compatible-model",
                secret_id_for_endpoint(&endpoint),
            ),
        ] {
            let error = OpenAiCompatibleLlmProvider::validate_frozen_route(&route)
                .expect_err("tampered route");
            assert_eq!(error.code, "llm.invalid_config");
        }

        let empty_model = ResolvedLlmRoute::new(
            OPENAI_COMPATIBLE_PROVIDER_REF,
            endpoint.as_str(),
            "   ",
            secret_id_for_endpoint(&endpoint),
        );
        let error = OpenAiCompatibleLlmProvider::validate_frozen_route(&empty_model)
            .expect_err("empty frozen model");
        assert_eq!(error.code, "llm.invalid_request");
        assert!(!error.code.contains("private.example"));
        assert!(!error.safe_message_key.contains("private.example"));
    }

    #[test]
    fn route_debug_does_not_expose_provider_metadata() {
        let route = provider(
            "http://127.0.0.1:11434/v1",
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        )
        .route;
        let debug = format!("{route:?}");
        for forbidden in [
            route.provider_ref(),
            route.endpoint(),
            route.model(),
            route.secret_id(),
        ] {
            assert!(!debug.contains(forbidden));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sends_minimal_compatible_request_and_parses_canonical_content() {
        let server = one_shot_server(
            "200 OK",
            success_envelope("保留数字 12.5，并且不要改变否定含义。"),
            Duration::ZERO,
            &[],
        );
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::JsonSchema,
        );
        let request_id = RequestId::new();

        let result = provider
            .process(text_request(request_id))
            .await
            .expect("processed");
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.final_text, "保留数字 12.5，并且不要改变否定含义。");

        let captured = server
            .request
            .recv_timeout(Duration::from_secs(1))
            .expect("captured request");
        let captured_text = String::from_utf8(captured).expect("http utf8");
        let (headers, body) = captured_text.split_once("\r\n\r\n").expect("http request");
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-test-very-secret")
        );

        let body: Value = serde_json::from_str(body).expect("request JSON");
        assert_eq!(body["model"], "compatible-model");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["strict"],
            Value::Bool(true)
        );
        let serialized = body.to_string();
        for forbidden in [
            "sk-test-VERY-SECRET",
            "llm.primary.api_key",
            "\"primary\"",
            "audio",
            "history",
            "target_handle",
        ] {
            assert!(!serialized.contains(forbidden), "{forbidden}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_only_mode_omits_provider_specific_structured_output_field() {
        let server = one_shot_server("200 OK", success_envelope("结果"), Duration::ZERO, &[]);
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        );
        provider
            .process(text_request(RequestId::new()))
            .await
            .expect("processed");

        let captured = server
            .request
            .recv_timeout(Duration::from_secs(1))
            .expect("captured request");
        let body_start = captured
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("headers")
            + 4;
        let body: Value = serde_json::from_slice(&captured[body_start..]).expect("request JSON");
        assert!(body.get("response_format").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_errors_are_stable_and_do_not_copy_provider_body() {
        for (status, expected, retryable) in [
            ("401 Unauthorized", "llm.authentication_failed", false),
            ("403 Forbidden", "llm.permission_denied", false),
            ("429 Too Many Requests", "llm.rate_limited", true),
            ("503 Service Unavailable", "llm.provider_unavailable", true),
            ("302 Found", "llm.request_rejected", false),
        ] {
            let server = one_shot_server(
                status,
                r#"{"secret":"PROVIDER_RAW_SECRET_MARKER"}"#.to_owned(),
                Duration::ZERO,
                &[("Location", "http://127.0.0.1:1/should-not-follow")],
            );
            let provider = provider(
                &server.base_url,
                OpenAiCompatiblePolicy::default(),
                StructuredOutputMode::PromptOnly,
            );

            let error = provider
                .process(text_request(RequestId::new()))
                .await
                .expect_err("status must fail");
            assert_eq!(error.code, expected);
            assert_eq!(error.retryable, retryable);
            assert!(!error.code.contains("PROVIDER_RAW_SECRET_MARKER"));
            assert!(
                !error
                    .safe_message_key
                    .contains("PROVIDER_RAW_SECRET_MARKER")
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_probe_returns_real_upstream_body_with_credentials_redacted() {
        let body = r#"{"error":{"message":"gateway rejected sk-test-VERY-SECRET; Authorization: Bearer upstream-token-123","type":"invalid_request_error"}}"#;
        let server = one_shot_server("401 Unauthorized", body.to_owned(), Duration::ZERO, &[]);
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        );

        let failure = provider
            .probe_connection(text_request(RequestId::new()))
            .await
            .expect_err("probe must preserve the upstream HTTP failure");
        assert_eq!(failure.error.code, "llm.authentication_failed");
        let upstream = failure.upstream.as_ref().expect("upstream diagnostic");
        assert_eq!(upstream.http_status(), 401);
        assert!(!upstream.truncated());
        assert!(upstream.response_body().contains("gateway rejected"));
        assert!(upstream.response_body().contains("invalid_request_error"));
        assert!(upstream.response_body().contains("[REDACTED_API_KEY]"));
        assert!(upstream.response_body().contains("[REDACTED_BEARER_TOKEN]"));
        assert!(!upstream.response_body().contains("sk-test-VERY-SECRET"));
        assert!(!upstream.response_body().contains("upstream-token-123"));

        let debug = format!("{failure:?}");
        assert!(!debug.contains("gateway rejected"));
        assert!(!debug.contains("sk-test-VERY-SECRET"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_probe_bounds_large_upstream_error_body() {
        let server = one_shot_server(
            "503 Service Unavailable",
            "x".repeat(MAX_UPSTREAM_DIAGNOSTIC_BYTES + 4096),
            Duration::ZERO,
            &[],
        );
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        );

        let failure = provider
            .probe_connection(text_request(RequestId::new()))
            .await
            .expect_err("probe must fail");
        let upstream = failure.upstream.expect("upstream diagnostic");
        assert_eq!(upstream.http_status(), 503);
        assert!(upstream.truncated());
        assert!(upstream.response_body().len() <= MAX_UPSTREAM_DIAGNOSTIC_BYTES);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_probe_includes_a_successful_but_incompatible_response() {
        let body = r#"{"message":"gateway used a non-compatible success envelope"}"#;
        let server = one_shot_server("200 OK", body.to_owned(), Duration::ZERO, &[]);
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        );

        let failure = provider
            .probe_connection(text_request(RequestId::new()))
            .await
            .expect_err("incompatible success envelope must fail");
        assert_eq!(failure.error.code, "llm.invalid_response");
        let upstream = failure.upstream.expect("upstream diagnostic");
        assert_eq!(upstream.http_status(), 200);
        assert_eq!(upstream.response_body(), body);
        assert!(!upstream.truncated());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_canonical_assistant_content_is_distinct_from_transport_failure() {
        let response = json!({
            "choices": [{"message": {"content": "RemTene connection test."}}]
        })
        .to_string();
        let server = one_shot_server("200 OK", response, Duration::ZERO, &[]);
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        );

        let error = provider
            .process(text_request(RequestId::new()))
            .await
            .expect_err("formal processing still requires canonical content");
        assert_eq!(error.code, "llm.invalid_canonical_response");
        assert!(!error.retryable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incompatible_provider_envelopes_fail_closed() {
        for response in [
            json!({
                "completion": {
                    "schema_version": 1,
                    "intent": "dictation",
                    "final_text": "wrong protocol"
                }
            })
            .to_string(),
            json!({
                "choices": [
                    {"message": {"content": "{\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"one\"}"}},
                    {"message": {"content": "{\"schema_version\":1,\"intent\":\"dictation\",\"final_text\":\"two\"}"}}
                ]
            })
            .to_string(),
            json!({"choices": [{"message": {"content": null}}]}).to_string(),
        ] {
            let server = one_shot_server("200 OK", response, Duration::ZERO, &[]);
            let provider = provider(
                &server.base_url,
                OpenAiCompatiblePolicy::default(),
                StructuredOutputMode::PromptOnly,
            );
            let error = provider
                .process(text_request(RequestId::new()))
                .await
                .expect_err("incompatible envelope");
            assert_eq!(error.code, "llm.invalid_response");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_secret_fails_before_any_network_request() {
        let endpoint = chat_completions_endpoint("http://127.0.0.1:1/v1").expect("endpoint");
        let route = ResolvedLlmRoute::new(
            OPENAI_COMPATIBLE_PROVIDER_REF,
            endpoint.as_str(),
            "compatible-model",
            secret_id_for_endpoint(&endpoint),
        );
        let provider = OpenAiCompatibleLlmProvider::new(Arc::new(MemorySecretStore::empty()))
            .expect("provider");

        let error = provider
            .process(route, text_request(RequestId::new()))
            .await
            .expect_err("missing key");
        assert_eq!(error.code, "llm.api_key_missing");
        assert!(!error.retryable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_process_future_unregisters_the_request() {
        let endpoint = chat_completions_endpoint("https://provider.invalid/v1").expect("endpoint");
        let route = ResolvedLlmRoute::new(
            OPENAI_COMPATIBLE_PROVIDER_REF,
            endpoint.as_str(),
            "compatible-model",
            secret_id_for_endpoint(&endpoint),
        );
        let provider = Arc::new(
            OpenAiCompatibleLlmProvider::new(Arc::new(PendingSecretStore)).expect("provider"),
        );
        let request_id = RequestId::new();
        let task_provider = Arc::clone(&provider);
        let task =
            tokio::spawn(
                async move { task_provider.process(route, text_request(request_id)).await },
            );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if provider
                    .active
                    .lock()
                    .expect("active requests")
                    .contains_key(&request_id)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("request registered");

        task.abort();
        let join_error = task.await.expect_err("task aborted");
        assert!(join_error.is_cancelled());
        assert!(
            !provider
                .active
                .lock()
                .expect("active requests")
                .contains_key(&request_id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn response_body_limit_fails_closed() {
        let server = one_shot_server("200 OK", "x".repeat(1024), Duration::ZERO, &[]);
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy {
                max_response_bytes: 64,
                ..OpenAiCompatiblePolicy::default()
            },
            StructuredOutputMode::PromptOnly,
        );

        let error = provider
            .process(text_request(RequestId::new()))
            .await
            .expect_err("oversized response");
        assert_eq!(error.code, "llm.response_too_large");
        assert!(!error.retryable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hard_timeout_releases_the_request_with_a_stable_error() {
        let server = one_shot_server(
            "200 OK",
            success_envelope("迟到结果"),
            Duration::from_millis(250),
            &[],
        );
        let provider = provider(
            &server.base_url,
            OpenAiCompatiblePolicy {
                request_timeout: Duration::from_millis(50),
                ..OpenAiCompatiblePolicy::default()
            },
            StructuredOutputMode::PromptOnly,
        );

        let error = provider
            .process(text_request(RequestId::new()))
            .await
            .expect_err("timeout");
        assert_eq!(error.code, "llm.timeout");
        assert!(error.retryable);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_wakes_the_in_flight_request_and_prevents_success() {
        let server = one_shot_server(
            "200 OK",
            success_envelope("不应交付"),
            Duration::from_secs(1),
            &[],
        );
        let provider = Arc::new(provider(
            &server.base_url,
            OpenAiCompatiblePolicy::default(),
            StructuredOutputMode::PromptOnly,
        ));
        let request_id = RequestId::new();
        let task_provider = Arc::clone(&provider);
        let task =
            tokio::spawn(async move { task_provider.process(text_request(request_id)).await });

        server
            .request
            .recv_timeout(Duration::from_secs(1))
            .expect("request reached server");
        provider.cancel(request_id).await.expect("cancel");

        let error = tokio::time::timeout(Duration::from_millis(250), task)
            .await
            .expect("cancel must wake request")
            .expect("task join")
            .expect_err("cancelled request");
        assert_eq!(error.code, "llm.cancelled");
        assert!(!error.retryable);
    }
}
