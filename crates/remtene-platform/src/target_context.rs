#![cfg_attr(not(test), allow(dead_code))]

//! Secure-input detection, target snapshots, selection reads, and safe insertion.
//!
//! The native backend deliberately exposes only three content-related operations:
//! read an exact selection, collapse that selection to its end, and dispatch an
//! insertion. There is no replace, delete, or full-value write capability.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use remtene_application::ports::{
    CapturedTarget, InsertOutcome, LifecycleFence, OutputAdapter, PortError, PortFuture,
    SelectionSnapshot, TargetContextPort, TargetDisplayHint, TargetRevalidation, TargetSecurity,
    TargetSnapshotRef, ValidatedTargetRef,
};
use remtene_domain::DeliveryId;

use crate::clipboard::{ClipboardPasteOutcome, ClipboardTargetStatus};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{AccessibilityTrust, MacTargetContext};

const DEFAULT_SNAPSHOT_TTL_MS: u64 = 15 * 60 * 1_000;
const DEFAULT_VALIDATION_TTL_MS: u64 = 2_000;
const DEFAULT_DELIVERY_TTL_MS: u64 = 15 * 60 * 1_000;
const DEFAULT_REGISTRY_CAPACITY: usize = 64;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// A platform-native character range. Units are chosen by the native backend
/// and must remain consistent for capture, comparison, collapse, and verify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TextRange {
    pub(crate) location: usize,
    pub(crate) length: usize,
}

impl TextRange {
    fn end(self) -> Option<usize> {
        self.location.checked_add(self.length)
    }

    fn caret_at(location: usize) -> Self {
        Self {
            location,
            length: 0,
        }
    }
}

/// Evidence needed to distinguish a known value from an API or permission gap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Probe<T> {
    Known(T),
    Unknown,
}

/// Classification of the currently focused native control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlKind {
    EditableText,
    SecureText,
    Unsupported,
    Unknown,
}

/// A complete observation made by the platform backend at one point in time.
///
/// `identity` is platform-private and should contain enough native handles to
/// compare the application, window, and control independently.
#[derive(Clone, Debug)]
pub(crate) struct TargetObservation<I> {
    pub(crate) identity: Option<I>,
    pub(crate) display_hint: Option<TargetDisplayHint>,
    pub(crate) accessibility_trusted: Probe<bool>,
    pub(crate) secure_event_input: Probe<bool>,
    pub(crate) control_kind: ControlKind,
    pub(crate) selected_range: Option<TextRange>,
    pub(crate) belongs_to_this_process: bool,
}

/// Result of comparing one native identity component.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdentityComparison {
    Same,
    Different,
    Unknown,
}

/// Native identity comparison is intentionally split into the process plus all
/// three required AX components, so a matching PID or matching attributes
/// cannot masquerade as an exact target match.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TargetIdentityComparison {
    pub(crate) process: IdentityComparison,
    pub(crate) application: IdentityComparison,
    pub(crate) window: IdentityComparison,
    pub(crate) control: IdentityComparison,
}

impl TargetIdentityComparison {
    fn disposition(self) -> RevalidationDisposition {
        let components = [self.process, self.application, self.window, self.control];
        if components.contains(&IdentityComparison::Different) {
            RevalidationDisposition::Invalid
        } else if components.contains(&IdentityComparison::Unknown) {
            RevalidationDisposition::Indeterminate
        } else {
            RevalidationDisposition::Valid
        }
    }
}

/// Dispatch separates a proven pre-dispatch failure from a call whose external
/// effect can no longer be ruled out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InsertDispatch<R> {
    NotDispatched,
    Dispatched(R),
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InsertVerification {
    Verified,
    /// 写入未生效，且有证据证明文档未被改动：光标仍在原锚点、字符总数仍等于
    /// 写入前的值。区分它与 `Unverified` 的意义在于回退是否安全——只有确知
    /// 什么都没写进去，才可以走剪贴板重投而不会造成重复插入。
    ///
    /// Chromium／Electron 类应用会稳定落在这里：它们声明 `AXSelectedText`
    /// 可写，但实际丢弃写入，控件因此纹丝不动。
    ProvenNotInserted,
    Unverified,
}

/// One content-free observation of a dispatched clipboard paste.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClipboardInsertObservation {
    /// The exact intended text was read back at the original anchor, and at
    /// least one independent insertion marker reached its expected state.
    Verified,
    /// The target still exposes the pre-paste caret/count state; its run loop
    /// may not have consumed the synthesized event yet.
    Pending,
    /// Insertion markers changed consistently, but the target has not made
    /// the exact inserted range readable yet. This is distinct from an
    /// unexpected state: some Chromium/Electron controls publish caret/count
    /// metadata before their parameterized text range becomes readable.
    ReadbackPending,
    /// The target became unreadable or exposed a state that cannot prove either
    /// success or absence of mutation.
    Unverified,
}

/// Narrow native interface used by the safety state machine.
///
/// Implementations must not change document content from `collapse_selection`.
/// `dispatch_insert` may only insert at the currently collapsed caret. No API
/// for replacing, deleting, or writing the full control value exists here.
pub(crate) trait TargetContextBackend: Send + Sync + 'static {
    type Identity: Clone + Send + Sync + 'static;
    type Receipt: Send + Sync + 'static;

    fn observe_focused_target(&self) -> TargetObservation<Self::Identity>;

    fn compare_identity(
        &self,
        expected: &Self::Identity,
        actual: &Self::Identity,
    ) -> TargetIdentityComparison;

    fn read_selected_text(&self, target: &Self::Identity, range: TextRange) -> Result<String, ()>;

    /// Collapses an existing selection to `anchor` without modifying content.
    fn collapse_selection(
        &self,
        target: &Self::Identity,
        expected_range: TextRange,
        anchor: usize,
    ) -> Result<(), ()>;

    /// Dispatches one insertion to the already focused and revalidated target.
    fn dispatch_insert(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
    ) -> InsertDispatch<Self::Receipt>;

    /// Character count of `target`'s control, or `None` when unreadable.
    ///
    /// Used to prove whether a fire-and-forget paste actually landed.
    fn character_count(&self, target: &Self::Identity) -> Option<usize>;

    /// Observes the exact postcondition of a synthesized clipboard paste.
    ///
    /// Implementations may read only the intended output range, and only after
    /// either the caret or character count indicates a possible insertion.
    fn clipboard_insert_observation(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
        before_character_count: Option<usize>,
    ) -> ClipboardInsertObservation;

    /// Proves the postcondition for the exact target, anchor, and inserted text.
    fn verify_insert(
        &self,
        target: &Self::Identity,
        anchor: usize,
        text: &str,
        receipt: &Self::Receipt,
    ) -> InsertVerification;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TargetContextPolicy {
    snapshot_ttl_ms: u64,
    validation_ttl_ms: u64,
    delivery_ttl_ms: u64,
    registry_capacity: usize,
    selection_limit_chars: Option<usize>,
}

impl Default for TargetContextPolicy {
    fn default() -> Self {
        Self {
            snapshot_ttl_ms: DEFAULT_SNAPSHOT_TTL_MS,
            validation_ttl_ms: DEFAULT_VALIDATION_TTL_MS,
            delivery_ttl_ms: DEFAULT_DELIVERY_TTL_MS,
            registry_capacity: DEFAULT_REGISTRY_CAPACITY,
            // The product threshold is still a POC decision. `None` means this
            // adapter never invents or silently applies a limit.
            selection_limit_chars: None,
        }
    }
}

trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug)]
struct MonotonicClock {
    origin: Instant,
}

impl MonotonicClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Debug)]
struct TargetSnapshot<I> {
    identity: Option<I>,
    security: TargetSecurity,
    captured_range: Option<TextRange>,
    anchor: Option<usize>,
    created_at_ms: u64,
    created_order: u64,
    expires_at_ms: u64,
}

#[derive(Clone, Debug)]
struct ValidatedCapability<I> {
    source_target_ref: String,
    identity: I,
    captured_range: TextRange,
    anchor: usize,
    created_at_ms: u64,
    created_order: u64,
    expires_at_ms: u64,
}

#[derive(Debug)]
struct Registry<I> {
    snapshots: HashMap<String, TargetSnapshot<I>>,
    validations: HashMap<String, ValidatedCapability<I>>,
    consumed_deliveries: HashMap<DeliveryId, DeliveryRecord>,
}

impl<I> Default for Registry<I> {
    fn default() -> Self {
        Self {
            snapshots: HashMap::new(),
            validations: HashMap::new(),
            consumed_deliveries: HashMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DeliveryRecord {
    consumed_at_ms: u64,
    consumed_order: u64,
    expires_at_ms: u64,
}

/// Platform-private implementation shared by `TargetContextPort` and
/// `OutputAdapter`. A native backend can be added later without changing the
/// application contracts or exposing native handles across IPC.
pub(crate) struct SafeTargetContext<B>
where
    B: TargetContextBackend,
{
    backend: Arc<B>,
    policy: TargetContextPolicy,
    clock: Arc<dyn Clock>,
    registry: Mutex<Registry<B::Identity>>,
}

impl<B> SafeTargetContext<B>
where
    B: TargetContextBackend,
{
    pub(crate) fn new(backend: Arc<B>) -> Self {
        Self::with_policy_and_clock(
            backend,
            TargetContextPolicy::default(),
            Arc::new(MonotonicClock::new()),
        )
    }

    fn with_policy_and_clock(
        backend: Arc<B>,
        policy: TargetContextPolicy,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            backend,
            policy,
            clock,
            registry: Mutex::new(Registry::default()),
        }
    }

    fn capture_sync(&self) -> CapturedTarget {
        let now_ms = self.clock.now_ms();
        let observation = self.backend.observe_focused_target();
        let display_hint = observation.display_hint;
        let security = classify_security(&observation);
        // 捕获阶段决定精确 AX 路径是否可用；非 Safe 目标仍可由 Application
        // 转入用户显式开启的当前焦点兼容贴上。具体是哪一项探针不合格由
        // macOS 后端各出口自报，此处只记录汇总判定。
        crate::trace::delivery(
            "capture",
            &format!("{security:?}"),
            &format!(
                "control={:?} 有身份={} 有选区={} 属本进程={}",
                observation.control_kind,
                observation.identity.is_some(),
                observation.selected_range.is_some(),
                observation.belongs_to_this_process
            ),
        );
        let captured_range = if security == TargetSecurity::Safe {
            observation.selected_range
        } else {
            None
        };
        let anchor = captured_range.and_then(TextRange::end);
        let (target_token, created_order) = next_token("target");
        let target_ref = TargetSnapshotRef::new(target_token);
        let snapshot = TargetSnapshot {
            identity: observation.identity,
            security,
            captured_range,
            anchor,
            created_at_ms: now_ms,
            created_order,
            expires_at_ms: now_ms.saturating_add(self.policy.snapshot_ttl_ms),
        };

        let has_selection = snapshot
            .captured_range
            .is_some_and(|range| range.length > 0);
        let mut registry = self.lock_registry();
        purge_expired(&mut registry, now_ms);
        make_snapshot_capacity(&mut registry, self.policy.registry_capacity.max(1));
        registry
            .snapshots
            .insert(target_ref.as_str().to_owned(), snapshot);

        CapturedTarget {
            target_ref,
            security,
            has_selection,
            display_hint,
        }
    }

    fn read_selected_text_sync(
        &self,
        target: &TargetSnapshotRef,
    ) -> Result<SelectionSnapshot, PortError> {
        let now_ms = self.clock.now_ms();
        let snapshot = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            registry.snapshots.get(target.as_str()).cloned()
        }
        .ok_or_else(|| port_error("target_snapshot_unavailable", false))?;

        if snapshot.security != TargetSecurity::Safe {
            return Err(port_error("target_context_not_safe", false));
        }

        let Some(expected_identity) = snapshot.identity.as_ref() else {
            return Err(port_error("target_context_not_safe", false));
        };
        let Some(range) = snapshot.captured_range else {
            return Err(port_error("target_context_not_safe", false));
        };
        let Some(_anchor) = snapshot.anchor else {
            return Err(port_error("target_context_not_safe", false));
        };

        let current = self.backend.observe_focused_target();
        match self
            .revalidation_disposition(&snapshot, &current)
            .disposition
        {
            RevalidationDisposition::Valid => {}
            RevalidationDisposition::Invalid | RevalidationDisposition::Indeterminate => {
                return Err(port_error("target_context_changed", false));
            }
        }

        if range.length == 0 {
            return Ok(SelectionSnapshot {
                text: None,
                anchor_normalized_to_end: true,
                exceeded_limit: false,
            });
        }

        let text = self
            .backend
            .read_selected_text(expected_identity, range)
            .map_err(|()| port_error("selection_read_failed", true))?;
        let exceeded_limit = self
            .policy
            .selection_limit_chars
            .is_some_and(|limit| text.chars().count() > limit);

        Ok(SelectionSnapshot {
            text: (!exceeded_limit).then_some(text),
            anchor_normalized_to_end: true,
            exceeded_limit,
        })
    }

    fn revalidate_sync(&self, target: &TargetSnapshotRef) -> Result<TargetRevalidation, PortError> {
        let now_ms = self.clock.now_ms();
        crate::trace::checkpoint("revalidate.begin", "");
        let snapshot = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            registry.snapshots.get(target.as_str()).cloned()
        };

        let Some(snapshot) = snapshot else {
            crate::trace::delivery("revalidate", "不确定", "快照不存在或已过期");
            return Ok(TargetRevalidation::Indeterminate);
        };
        let current = self.backend.observe_focused_target();
        let assessment = self.revalidation_disposition(&snapshot, &current);
        match assessment.disposition {
            RevalidationDisposition::Invalid => {
                crate::trace::delivery("revalidate", "无效", &assessment.diagnostic_detail());
                Ok(TargetRevalidation::Invalid)
            }
            RevalidationDisposition::Indeterminate => {
                crate::trace::delivery("revalidate", "不确定", &assessment.diagnostic_detail());
                Ok(TargetRevalidation::Indeterminate)
            }
            RevalidationDisposition::Valid => {
                let Some(identity) = snapshot.identity.clone() else {
                    crate::trace::delivery("revalidate", "不确定", "快照无身份信息");
                    return Ok(TargetRevalidation::Indeterminate);
                };
                let Some(captured_range) = snapshot.captured_range else {
                    crate::trace::delivery("revalidate", "不确定", "快照无选区信息");
                    return Ok(TargetRevalidation::Indeterminate);
                };
                let Some(anchor) = snapshot.anchor else {
                    crate::trace::delivery("revalidate", "不确定", "快照无锚点");
                    return Ok(TargetRevalidation::Indeterminate);
                };

                let (validated_token, created_order) = next_token("validated");
                let validated_ref = ValidatedTargetRef::new(validated_token);
                let capability = ValidatedCapability {
                    source_target_ref: target.as_str().to_owned(),
                    identity,
                    captured_range,
                    anchor,
                    created_at_ms: now_ms,
                    created_order,
                    expires_at_ms: now_ms.saturating_add(self.policy.validation_ttl_ms),
                };
                let mut registry = self.lock_registry();
                purge_expired(&mut registry, now_ms);
                registry
                    .validations
                    .retain(|_, entry| entry.source_target_ref != target.as_str());
                make_validation_capacity(&mut registry, self.policy.registry_capacity.max(1));
                registry
                    .validations
                    .insert(validated_ref.as_str().to_owned(), capability);
                crate::trace::delivery("revalidate", "有效", "凭证已颁发");
                Ok(TargetRevalidation::Valid(validated_ref))
            }
        }
    }

    fn insert_sync(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> Result<InsertOutcome, PortError> {
        let now_ms = self.clock.now_ms();
        crate::trace::checkpoint(
            "insert.begin",
            &format!("text_len={}", text.chars().count()),
        );
        let capability = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            let capability = registry.validations.remove(target.as_str());
            let Some(capability) = capability else {
                // 凭证在校验与插入之间过期或被顶替。TTL 太短、或两步之间隔了
                // 一次耗时的转录时会走到这里。
                crate::trace::delivery("insert", "未插入", "凭证不存在或已被消费");
                return Ok(InsertOutcome::NotInserted);
            };
            if registry.consumed_deliveries.contains_key(&delivery_id) {
                crate::trace::delivery("insert", "未插入", "本次交付已被消费过（重复投递）");
                return Ok(InsertOutcome::NotInserted);
            }
            make_delivery_capacity(&mut registry, self.policy.registry_capacity.max(1));
            let consumed_order = next_sequence();
            registry.consumed_deliveries.insert(
                delivery_id,
                DeliveryRecord {
                    consumed_at_ms: now_ms,
                    consumed_order,
                    expires_at_ms: now_ms.saturating_add(self.policy.delivery_ttl_ms),
                },
            );
            capability
        };

        if capability.expires_at_ms <= now_ms {
            crate::trace::delivery(
                "insert",
                "未插入",
                &format!(
                    "凭证已过期 {}ms",
                    now_ms.saturating_sub(capability.expires_at_ms)
                ),
            );
            return Ok(InsertOutcome::NotInserted);
        }
        crate::trace::checkpoint("insert.capability", "凭证有效");

        let precommit = self.backend.observe_focused_target();
        let precommit_disposition = self.capability_disposition(&capability, &precommit);
        if precommit_disposition != RevalidationDisposition::Valid {
            // 提交前复核失败：从捕获到插入之间，焦点、窗口或选区变了。
            crate::trace::delivery(
                "insert",
                "未插入",
                &format!("提交前复核 {precommit_disposition:?}"),
            );
            return Ok(InsertOutcome::NotInserted);
        }
        crate::trace::checkpoint("insert.precommit", "目标未变");

        let Some(_commit_guard) = lifecycle.begin_commit() else {
            crate::trace::delivery("insert", "未插入", "生命周期栅栏拒绝提交（会话已终止）");
            return Ok(InsertOutcome::NotInserted);
        };

        if capability.captured_range.length > 0 {
            if self
                .backend
                .collapse_selection(
                    &capability.identity,
                    capability.captured_range,
                    capability.anchor,
                )
                .is_err()
            {
                crate::trace::delivery("insert", "未插入", "收起选区失败");
                return Ok(InsertOutcome::NotInserted);
            }

            let collapsed = self.backend.observe_focused_target();
            let collapsed_disposition = self.collapsed_disposition(&capability, &collapsed);
            if collapsed_disposition != RevalidationDisposition::Valid {
                crate::trace::delivery(
                    "insert",
                    "未插入",
                    &format!("收起选区后复核 {collapsed_disposition:?}"),
                );
                return Ok(InsertOutcome::NotInserted);
            }
            crate::trace::checkpoint("insert.collapse", "选区已收起");
        }

        let dispatch = self
            .backend
            .dispatch_insert(&capability.identity, capability.anchor, &text);
        match dispatch {
            InsertDispatch::NotDispatched => {
                // AX 写入调用本身被目标应用拒绝：多半是控件不接受
                // AXSelectedText 写入。这是 AX 直写路线的核心失效点。
                crate::trace::delivery("insert", "未插入", "AX 写入被目标应用拒绝");
                Ok(InsertOutcome::NotInserted)
            }
            InsertDispatch::Indeterminate => {
                crate::trace::delivery("insert", "不确定", "AX 写入结果无法判定");
                Ok(InsertOutcome::Indeterminate)
            }
            InsertDispatch::Dispatched(receipt) => {
                crate::trace::checkpoint("insert.dispatch", "AX 写入已发出");
                match self.backend.verify_insert(
                    &capability.identity,
                    capability.anchor,
                    &text,
                    &receipt,
                ) {
                    InsertVerification::Verified => {
                        crate::trace::delivery("insert", "已插入", "写入已回读验证");
                        Ok(InsertOutcome::Inserted)
                    }
                    InsertVerification::ProvenNotInserted => {
                        // 文档确证未被改动。这不是「不知道」，而是「确定没写」，
                        // 因此可以安全回退到剪贴板，不存在重复插入的风险。
                        crate::trace::delivery(
                            "insert",
                            "确定未插入",
                            "控件状态证明文档未被改动，可安全回退",
                        );
                        Ok(InsertOutcome::NotInserted)
                    }
                    InsertVerification::Unverified => {
                        // 写入发出了，但回读对不上。此时不能声称成功：
                        // 文本可能落在别处，也可能根本没写进去。
                        crate::trace::delivery("insert", "不确定", "写入后回读校验不通过");
                        Ok(InsertOutcome::Indeterminate)
                    }
                }
            }
        }
    }

    fn revalidation_disposition(
        &self,
        expected: &TargetSnapshot<B::Identity>,
        current: &TargetObservation<B::Identity>,
    ) -> RevalidationAssessment {
        match classify_security(current) {
            TargetSecurity::SecureInput => {
                return RevalidationAssessment::new(
                    RevalidationDisposition::Invalid,
                    RevalidationReason::SecureInput,
                )
                .with_security(current);
            }
            TargetSecurity::Unknown => {
                return RevalidationAssessment::new(
                    RevalidationDisposition::Indeterminate,
                    RevalidationReason::SecurityUnknown,
                )
                .with_security(current);
            }
            TargetSecurity::Safe => {}
        }

        let (Some(expected_identity), Some(current_identity)) =
            (expected.identity.as_ref(), current.identity.as_ref())
        else {
            return RevalidationAssessment::new(
                RevalidationDisposition::Indeterminate,
                RevalidationReason::SnapshotIdentityMissing,
            )
            .with_identity_presence(expected.identity.is_some(), current.identity.is_some());
        };
        let identity = self
            .backend
            .compare_identity(expected_identity, current_identity);
        let identity_disposition = identity.disposition();
        if identity_disposition != RevalidationDisposition::Valid {
            let reason = RevalidationReason::from_identity(identity);
            return RevalidationAssessment::new(identity_disposition, reason)
                .with_identity(identity);
        }
        if expected.captured_range != current.selected_range {
            return RevalidationAssessment::new(
                RevalidationDisposition::Invalid,
                RevalidationReason::SelectedRangeChanged,
            )
            .with_ranges(expected.captured_range, current.selected_range);
        }
        RevalidationAssessment::new(RevalidationDisposition::Valid, RevalidationReason::Stable)
    }

    fn capability_disposition(
        &self,
        expected: &ValidatedCapability<B::Identity>,
        current: &TargetObservation<B::Identity>,
    ) -> RevalidationDisposition {
        match classify_security(current) {
            TargetSecurity::SecureInput => return RevalidationDisposition::Invalid,
            TargetSecurity::Unknown => return RevalidationDisposition::Indeterminate,
            TargetSecurity::Safe => {}
        }
        let Some(current_identity) = current.identity.as_ref() else {
            return RevalidationDisposition::Indeterminate;
        };
        let identity = self
            .backend
            .compare_identity(&expected.identity, current_identity)
            .disposition();
        if identity != RevalidationDisposition::Valid {
            return identity;
        }
        if current.selected_range != Some(expected.captured_range) {
            return RevalidationDisposition::Invalid;
        }
        RevalidationDisposition::Valid
    }

    fn collapsed_disposition(
        &self,
        expected: &ValidatedCapability<B::Identity>,
        current: &TargetObservation<B::Identity>,
    ) -> RevalidationDisposition {
        match classify_security(current) {
            TargetSecurity::SecureInput => return RevalidationDisposition::Invalid,
            TargetSecurity::Unknown => return RevalidationDisposition::Indeterminate,
            TargetSecurity::Safe => {}
        }
        let Some(current_identity) = current.identity.as_ref() else {
            return RevalidationDisposition::Indeterminate;
        };
        let identity = self
            .backend
            .compare_identity(&expected.identity, current_identity)
            .disposition();
        if identity != RevalidationDisposition::Valid {
            return identity;
        }
        if current.selected_range != Some(TextRange::caret_at(expected.anchor)) {
            return RevalidationDisposition::Invalid;
        }
        RevalidationDisposition::Valid
    }

    /// Whether the capability behind `target` still describes the focused control.
    ///
    /// The clipboard backend needs this immediately before synthesizing ⌘V. It
    /// reuses the same disposition rules as the AX write path, so a paste can
    /// never be aimed at a target the AX path would have refused; it is exposed
    /// separately only because a keystroke cannot carry the element reference.
    pub fn clipboard_insert_status(&self, target: &ValidatedTargetRef) -> ClipboardTargetStatus {
        let now_ms = self.clock.now_ms();
        let capability = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            registry.validations.get(target.as_str()).cloned()
        };
        // A missing capability means expired or never issued. Both are
        // indeterminate rather than invalid: the token proves nothing either way.
        let Some(capability) = capability else {
            return ClipboardTargetStatus::Indeterminate;
        };
        let current = self.backend.observe_focused_target();
        let disposition = match self.capability_disposition(&capability, &current) {
            // A collapsed caret is the normal state after a prior insert, so
            // accept it as an alternative to the originally captured range.
            RevalidationDisposition::Invalid => self.collapsed_disposition(&capability, &current),
            other => other,
        };
        match disposition {
            RevalidationDisposition::Valid => ClipboardTargetStatus::Valid,
            RevalidationDisposition::Invalid => ClipboardTargetStatus::Invalid,
            RevalidationDisposition::Indeterminate => ClipboardTargetStatus::Indeterminate,
        }
    }

    /// Revalidates at the real ⌘V boundary, dispatches once, and polls a cloned
    /// capability so verification is not invalidated merely because its
    /// pre-dispatch TTL expires while the target application's run loop works.
    pub(crate) fn dispatch_and_verify_clipboard_insert<E>(
        &self,
        target: &ValidatedTargetRef,
        text: &str,
        attempts: usize,
        interval: Duration,
        dispatch: impl FnOnce() -> Result<(), E>,
    ) -> Result<ClipboardPasteOutcome, E> {
        let now_ms = self.clock.now_ms();
        let capability = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            registry.validations.get(target.as_str()).cloned()
        };
        let Some(capability) = capability else {
            return Ok(ClipboardPasteOutcome::Indeterminate);
        };

        let current = self.backend.observe_focused_target();
        let disposition = match self.capability_disposition(&capability, &current) {
            RevalidationDisposition::Invalid => self.collapsed_disposition(&capability, &current),
            other => other,
        };
        match disposition {
            RevalidationDisposition::Valid => {}
            RevalidationDisposition::Invalid => {
                return Ok(ClipboardPasteOutcome::NotInserted);
            }
            RevalidationDisposition::Indeterminate => {
                return Ok(ClipboardPasteOutcome::Indeterminate);
            }
        }

        let before_character_count = self.backend.character_count(&capability.identity);
        dispatch()?;

        Ok(poll_clipboard_insert_effect(attempts, interval, || {
            self.backend.clipboard_insert_observation(
                &capability.identity,
                capability.anchor,
                text,
                before_character_count,
            )
        }))
    }

    /// Whether `target`'s snapshot still describes the focused control.
    pub fn clipboard_selection_status(&self, target: &TargetSnapshotRef) -> ClipboardTargetStatus {
        let now_ms = self.clock.now_ms();
        let snapshot = {
            let mut registry = self.lock_registry();
            purge_expired(&mut registry, now_ms);
            registry.snapshots.get(target.as_str()).cloned()
        };
        let Some(snapshot) = snapshot else {
            return ClipboardTargetStatus::Indeterminate;
        };
        let current = self.backend.observe_focused_target();
        match self
            .revalidation_disposition(&snapshot, &current)
            .disposition
        {
            RevalidationDisposition::Valid => ClipboardTargetStatus::Valid,
            RevalidationDisposition::Invalid => ClipboardTargetStatus::Invalid,
            RevalidationDisposition::Indeterminate => ClipboardTargetStatus::Indeterminate,
        }
    }

    fn lock_registry(&self) -> std::sync::MutexGuard<'_, Registry<B::Identity>> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn poll_clipboard_insert_effect(
    attempts: usize,
    interval: Duration,
    mut observe: impl FnMut() -> ClipboardInsertObservation,
) -> ClipboardPasteOutcome {
    let mut saw_readback_pending = false;
    for attempt in 0..attempts {
        match observe() {
            ClipboardInsertObservation::Verified => return ClipboardPasteOutcome::Inserted,
            ClipboardInsertObservation::Unverified => {
                return ClipboardPasteOutcome::Indeterminate;
            }
            ClipboardInsertObservation::Pending => {}
            ClipboardInsertObservation::ReadbackPending => {
                saw_readback_pending = true;
            }
        }
        if attempt + 1 < attempts {
            std::thread::sleep(interval);
        }
    }

    // A finite period of unchanged AX metadata cannot prove that an already
    // posted cross-process event will never be consumed. The user's live trace
    // demonstrated exactly that false negative, so timeout remains uncertain.
    if saw_readback_pending {
        crate::trace::delivery(
            "clipboard.verify",
            "不确定",
            "reason=readback_unavailable_after_poll",
        );
    }
    ClipboardPasteOutcome::Indeterminate
}

impl<B> TargetContextPort for SafeTargetContext<B>
where
    B: TargetContextBackend,
{
    fn capture(&self) -> PortFuture<'_, Result<CapturedTarget, PortError>> {
        let captured = self.capture_sync();
        Box::pin(async move { Ok(captured) })
    }

    fn read_selected_text(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<SelectionSnapshot, PortError>> {
        let result = self.read_selected_text_sync(target);
        Box::pin(async move { result })
    }

    fn revalidate(
        &self,
        target: &TargetSnapshotRef,
    ) -> PortFuture<'_, Result<TargetRevalidation, PortError>> {
        let result = self.revalidate_sync(target);
        Box::pin(async move { result })
    }
}

impl<B> OutputAdapter for SafeTargetContext<B>
where
    B: TargetContextBackend,
{
    fn insert(
        &self,
        target: ValidatedTargetRef,
        text: String,
        delivery_id: DeliveryId,
        lifecycle: LifecycleFence,
    ) -> PortFuture<'_, Result<InsertOutcome, PortError>> {
        Box::pin(async move { self.insert_sync(target, text, delivery_id, lifecycle) })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RevalidationDisposition {
    Valid,
    Invalid,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RevalidationReason {
    Stable,
    SecureInput,
    SecurityUnknown,
    SnapshotIdentityMissing,
    ApplicationPidChanged,
    ApplicationIdentityChanged,
    WindowIdentityChanged,
    ControlIdentityChanged,
    IdentityIndeterminate,
    SelectedRangeChanged,
}

impl RevalidationReason {
    const fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::SecureInput => "secure_input",
            Self::SecurityUnknown => "security_unknown",
            Self::SnapshotIdentityMissing => "snapshot_identity_missing",
            Self::ApplicationPidChanged => "application_pid_changed",
            Self::ApplicationIdentityChanged => "application_identity_changed",
            Self::WindowIdentityChanged => "window_identity_changed",
            Self::ControlIdentityChanged => "control_identity_changed",
            Self::IdentityIndeterminate => "identity_indeterminate",
            Self::SelectedRangeChanged => "selected_range_changed",
        }
    }

    fn from_identity(identity: TargetIdentityComparison) -> Self {
        if identity.process == IdentityComparison::Different {
            Self::ApplicationPidChanged
        } else if identity.application == IdentityComparison::Different {
            Self::ApplicationIdentityChanged
        } else if identity.window == IdentityComparison::Different {
            Self::WindowIdentityChanged
        } else if identity.control == IdentityComparison::Different {
            Self::ControlIdentityChanged
        } else {
            Self::IdentityIndeterminate
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RevalidationAssessment {
    disposition: RevalidationDisposition,
    reason: RevalidationReason,
    identity: Option<TargetIdentityComparison>,
    expected_identity_present: Option<bool>,
    current_identity_present: Option<bool>,
    expected_range: Option<TextRange>,
    current_range: Option<TextRange>,
    accessibility_trusted: Option<Probe<bool>>,
    secure_event_input: Option<Probe<bool>>,
    control_kind: Option<ControlKind>,
    current_range_present: Option<bool>,
    belongs_to_this_process: Option<bool>,
}

impl RevalidationAssessment {
    const fn new(disposition: RevalidationDisposition, reason: RevalidationReason) -> Self {
        Self {
            disposition,
            reason,
            identity: None,
            expected_identity_present: None,
            current_identity_present: None,
            expected_range: None,
            current_range: None,
            accessibility_trusted: None,
            secure_event_input: None,
            control_kind: None,
            current_range_present: None,
            belongs_to_this_process: None,
        }
    }

    const fn with_identity(mut self, identity: TargetIdentityComparison) -> Self {
        self.identity = Some(identity);
        self
    }

    const fn with_identity_presence(mut self, expected: bool, current: bool) -> Self {
        self.expected_identity_present = Some(expected);
        self.current_identity_present = Some(current);
        self
    }

    const fn with_ranges(
        mut self,
        expected: Option<TextRange>,
        current: Option<TextRange>,
    ) -> Self {
        self.expected_range = expected;
        self.current_range = current;
        self
    }

    fn with_security<I>(mut self, current: &TargetObservation<I>) -> Self {
        self.accessibility_trusted = Some(current.accessibility_trusted);
        self.secure_event_input = Some(current.secure_event_input);
        self.control_kind = Some(current.control_kind);
        self.current_identity_present = Some(current.identity.is_some());
        self.current_range_present = Some(current.selected_range.is_some());
        self.belongs_to_this_process = Some(current.belongs_to_this_process);
        self
    }

    fn diagnostic_detail(self) -> String {
        let reason = self.reason.label();
        if let Some(identity) = self.identity {
            return format!(
                "reason={reason} pid={} application={} window={} control={}",
                identity_label(identity.process),
                identity_label(identity.application),
                identity_label(identity.window),
                identity_label(identity.control),
            );
        }
        if self.reason == RevalidationReason::SnapshotIdentityMissing {
            return format!(
                "reason={reason} expected_identity_present={} current_identity_present={}",
                optional_bool_label(self.expected_identity_present),
                optional_bool_label(self.current_identity_present),
            );
        }
        if self.reason == RevalidationReason::SelectedRangeChanged {
            return format!(
                "reason={reason} expected_range_present={} current_range_present={} location_match={} length_match={}",
                self.expected_range.is_some(),
                self.current_range.is_some(),
                range_component_match(self.expected_range, self.current_range, |range| {
                    range.location
                }),
                range_component_match(self.expected_range, self.current_range, |range| {
                    range.length
                }),
            );
        }
        if matches!(
            self.reason,
            RevalidationReason::SecureInput | RevalidationReason::SecurityUnknown
        ) {
            return format!(
                "reason={reason} accessibility_trusted={} secure_event_input={} control={} identity_present={} range_present={} belongs_to_this_process={}",
                optional_probe_label(self.accessibility_trusted),
                optional_probe_label(self.secure_event_input),
                self.control_kind
                    .map(control_kind_label)
                    .unwrap_or("unavailable"),
                optional_bool_label(self.current_identity_present),
                optional_bool_label(self.current_range_present),
                optional_bool_label(self.belongs_to_this_process),
            );
        }
        format!("reason={reason}")
    }
}

const fn identity_label(comparison: IdentityComparison) -> &'static str {
    match comparison {
        IdentityComparison::Same => "same",
        IdentityComparison::Different => "different",
        IdentityComparison::Unknown => "unknown",
    }
}

const fn optional_bool_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "true",
        Some(false) => "false",
        None => "unavailable",
    }
}

const fn optional_probe_label(value: Option<Probe<bool>>) -> &'static str {
    match value {
        Some(Probe::Known(true)) => "known_true",
        Some(Probe::Known(false)) => "known_false",
        Some(Probe::Unknown) => "unknown",
        None => "unavailable",
    }
}

const fn control_kind_label(kind: ControlKind) -> &'static str {
    match kind {
        ControlKind::EditableText => "editable_text",
        ControlKind::SecureText => "secure_text",
        ControlKind::Unsupported => "unsupported",
        ControlKind::Unknown => "unknown",
    }
}

fn range_component_match(
    expected: Option<TextRange>,
    current: Option<TextRange>,
    component: impl Fn(TextRange) -> usize,
) -> &'static str {
    match (expected, current) {
        (Some(expected), Some(current)) if component(expected) == component(current) => "true",
        (Some(_), Some(_)) => "false",
        _ => "unavailable",
    }
}

fn classify_security<I>(observation: &TargetObservation<I>) -> TargetSecurity {
    if observation.secure_event_input == Probe::Known(true)
        || observation.control_kind == ControlKind::SecureText
    {
        return TargetSecurity::SecureInput;
    }

    let range_is_valid = observation
        .selected_range
        .and_then(TextRange::end)
        .is_some();
    if observation.accessibility_trusted == Probe::Known(true)
        && observation.secure_event_input == Probe::Known(false)
        && observation.control_kind == ControlKind::EditableText
        && observation.identity.is_some()
        && range_is_valid
        && !observation.belongs_to_this_process
    {
        TargetSecurity::Safe
    } else {
        TargetSecurity::Unknown
    }
}

fn purge_expired<I>(registry: &mut Registry<I>, now_ms: u64) {
    registry
        .snapshots
        .retain(|_, entry| entry.expires_at_ms > now_ms);
    registry
        .validations
        .retain(|_, entry| entry.expires_at_ms > now_ms);
    registry
        .consumed_deliveries
        .retain(|_, entry| entry.expires_at_ms > now_ms);

    registry.validations.retain(|_, entry| {
        registry
            .snapshots
            .contains_key(entry.source_target_ref.as_str())
    });
}

fn make_snapshot_capacity<I>(registry: &mut Registry<I>, capacity: usize) {
    while registry.snapshots.len() >= capacity {
        let oldest = registry
            .snapshots
            .iter()
            .min_by_key(|(_, entry)| (entry.created_at_ms, entry.created_order))
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else {
            break;
        };
        registry.snapshots.remove(&oldest);
        registry
            .validations
            .retain(|_, entry| entry.source_target_ref != oldest);
    }
}

fn make_validation_capacity<I>(registry: &mut Registry<I>, capacity: usize) {
    while registry.validations.len() >= capacity {
        let oldest = registry
            .validations
            .iter()
            .min_by_key(|(_, entry)| (entry.created_at_ms, entry.created_order))
            .map(|(key, _)| key.clone());
        let Some(oldest) = oldest else {
            break;
        };
        registry.validations.remove(&oldest);
    }
}

fn make_delivery_capacity<I>(registry: &mut Registry<I>, capacity: usize) {
    while registry.consumed_deliveries.len() >= capacity {
        let oldest = registry
            .consumed_deliveries
            .iter()
            .min_by_key(|(_, entry)| (entry.consumed_at_ms, entry.consumed_order))
            .map(|(key, _)| *key);
        let Some(oldest) = oldest else {
            break;
        };
        registry.consumed_deliveries.remove(&oldest);
    }
}

fn next_token(kind: &str) -> (String, u64) {
    let value = next_sequence();
    (format!("remtene-{kind}-{value}"), value)
}

fn next_sequence() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
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
    use std::{
        future::Future,
        pin::pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeIdentity {
        process: u64,
        application: u64,
        window: u64,
        control: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum DispatchMode {
        Dispatch,
        DoNotDispatch,
        Indeterminate,
    }

    #[derive(Clone, Debug)]
    struct FakeState {
        observation: TargetObservation<FakeIdentity>,
        selected_text: String,
        compare_unknown: bool,
        collapse_succeeds: bool,
        dispatch_mode: DispatchMode,
        verification: InsertVerification,
        selection_reads: usize,
        character_count: Option<usize>,
        collapse_calls: Vec<usize>,
        dispatched_texts: Vec<String>,
        inserted_texts: Vec<String>,
    }

    #[derive(Debug)]
    struct FakeBackend {
        state: Mutex<FakeState>,
    }

    impl FakeBackend {
        fn new(observation: TargetObservation<FakeIdentity>) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    observation,
                    selected_text: "selected".to_owned(),
                    compare_unknown: false,
                    collapse_succeeds: true,
                    dispatch_mode: DispatchMode::Dispatch,
                    verification: InsertVerification::Verified,
                    selection_reads: 0,
                    character_count: None,
                    collapse_calls: Vec::new(),
                    dispatched_texts: Vec::new(),
                    inserted_texts: Vec::new(),
                }),
            }
        }

        fn update(&self, update: impl FnOnce(&mut FakeState)) {
            update(&mut self.state.lock().expect("fake backend lock"));
        }

        fn snapshot(&self) -> FakeState {
            self.state.lock().expect("fake backend lock").clone()
        }
    }

    impl TargetContextBackend for FakeBackend {
        type Identity = FakeIdentity;
        type Receipt = usize;

        fn observe_focused_target(&self) -> TargetObservation<Self::Identity> {
            self.state
                .lock()
                .expect("fake backend lock")
                .observation
                .clone()
        }

        fn compare_identity(
            &self,
            expected: &Self::Identity,
            actual: &Self::Identity,
        ) -> TargetIdentityComparison {
            let state = self.state.lock().expect("fake backend lock");
            if state.compare_unknown {
                return TargetIdentityComparison {
                    process: IdentityComparison::Unknown,
                    application: IdentityComparison::Unknown,
                    window: IdentityComparison::Unknown,
                    control: IdentityComparison::Unknown,
                };
            }
            TargetIdentityComparison {
                process: comparison(expected.process, actual.process),
                application: comparison(expected.application, actual.application),
                window: comparison(expected.window, actual.window),
                control: comparison(expected.control, actual.control),
            }
        }

        fn read_selected_text(
            &self,
            target: &Self::Identity,
            range: TextRange,
        ) -> Result<String, ()> {
            let mut state = self.state.lock().expect("fake backend lock");
            if state.observation.identity.as_ref() != Some(target)
                || state.observation.selected_range != Some(range)
            {
                return Err(());
            }
            state.selection_reads += 1;
            Ok(state.selected_text.clone())
        }

        fn character_count(&self, target: &Self::Identity) -> Option<usize> {
            let state = self.state.lock().expect("fake backend lock");
            if state.observation.identity.as_ref() != Some(target) {
                return None;
            }
            state.character_count
        }

        fn clipboard_insert_observation(
            &self,
            target: &Self::Identity,
            _anchor: usize,
            _text: &str,
            _before_character_count: Option<usize>,
        ) -> ClipboardInsertObservation {
            let state = self.state.lock().expect("fake backend lock");
            if state.observation.identity.as_ref() != Some(target) {
                return ClipboardInsertObservation::Unverified;
            }
            match state.verification {
                InsertVerification::Verified => ClipboardInsertObservation::Verified,
                InsertVerification::ProvenNotInserted => ClipboardInsertObservation::Pending,
                InsertVerification::Unverified => ClipboardInsertObservation::Unverified,
            }
        }

        fn collapse_selection(
            &self,
            target: &Self::Identity,
            expected_range: TextRange,
            anchor: usize,
        ) -> Result<(), ()> {
            let mut state = self.state.lock().expect("fake backend lock");
            state.collapse_calls.push(anchor);
            if !state.collapse_succeeds
                || state.observation.identity.as_ref() != Some(target)
                || state.observation.selected_range != Some(expected_range)
            {
                return Err(());
            }
            state.observation.selected_range = Some(TextRange::caret_at(anchor));
            Ok(())
        }

        fn dispatch_insert(
            &self,
            target: &Self::Identity,
            anchor: usize,
            text: &str,
        ) -> InsertDispatch<Self::Receipt> {
            let mut state = self.state.lock().expect("fake backend lock");
            if state.observation.identity.as_ref() != Some(target)
                || state.observation.selected_range != Some(TextRange::caret_at(anchor))
            {
                return InsertDispatch::NotDispatched;
            }
            match state.dispatch_mode {
                DispatchMode::DoNotDispatch => InsertDispatch::NotDispatched,
                DispatchMode::Indeterminate => InsertDispatch::Indeterminate,
                DispatchMode::Dispatch => {
                    state.dispatched_texts.push(text.to_owned());
                    if state.verification == InsertVerification::Verified {
                        state.inserted_texts.push(text.to_owned());
                    }
                    InsertDispatch::Dispatched(state.dispatched_texts.len())
                }
            }
        }

        fn verify_insert(
            &self,
            target: &Self::Identity,
            _anchor: usize,
            _text: &str,
            _receipt: &Self::Receipt,
        ) -> InsertVerification {
            let state = self.state.lock().expect("fake backend lock");
            if state.observation.identity.as_ref() != Some(target) {
                return InsertVerification::Unverified;
            }
            state.verification
        }
    }

    #[derive(Debug, Default)]
    struct FakeClock {
        now_ms: AtomicU64,
    }

    impl FakeClock {
        fn advance(&self, milliseconds: u64) {
            self.now_ms.fetch_add(milliseconds, Ordering::Relaxed);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now_ms.load(Ordering::Relaxed)
        }
    }

    fn identity() -> FakeIdentity {
        FakeIdentity {
            process: 1,
            application: 1,
            window: 2,
            control: 3,
        }
    }

    fn safe_observation(range: TextRange) -> TargetObservation<FakeIdentity> {
        TargetObservation {
            identity: Some(identity()),
            display_hint: Some(TargetDisplayHint { x: 640, y: 360 }),
            accessibility_trusted: Probe::Known(true),
            secure_event_input: Probe::Known(false),
            control_kind: ControlKind::EditableText,
            selected_range: Some(range),
            belongs_to_this_process: false,
        }
    }

    fn adapter(
        range: TextRange,
    ) -> (
        Arc<FakeBackend>,
        SafeTargetContext<FakeBackend>,
        Arc<FakeClock>,
    ) {
        let backend = Arc::new(FakeBackend::new(safe_observation(range)));
        let clock = Arc::new(FakeClock::default());
        let context = SafeTargetContext::with_policy_and_clock(
            Arc::clone(&backend),
            TargetContextPolicy {
                snapshot_ttl_ms: 100,
                validation_ttl_ms: 20,
                delivery_ttl_ms: 100,
                registry_capacity: 4,
                selection_limit_chars: None,
            },
            clock.clone(),
        );
        (backend, context, clock)
    }

    fn block_on<F>(future: F) -> F::Output
    where
        F: Future,
    {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn capture(context: &SafeTargetContext<FakeBackend>) -> CapturedTarget {
        block_on(context.capture()).expect("capture target")
    }

    fn validated(
        context: &SafeTargetContext<FakeBackend>,
        target: &TargetSnapshotRef,
    ) -> ValidatedTargetRef {
        match block_on(context.revalidate(target)).expect("revalidate target") {
            TargetRevalidation::Valid(validated) => validated,
            other => panic!("expected valid target, got {other:?}"),
        }
    }

    fn assess(
        context: &SafeTargetContext<FakeBackend>,
        backend: &FakeBackend,
        target: &TargetSnapshotRef,
    ) -> RevalidationAssessment {
        let snapshot = context
            .lock_registry()
            .snapshots
            .get(target.as_str())
            .cloned()
            .expect("captured snapshot");
        let current = backend.observe_focused_target();
        context.revalidation_disposition(&snapshot, &current)
    }

    #[test]
    fn classifies_safe_secure_unknown_and_own_targets() {
        let safe = safe_observation(TextRange {
            location: 4,
            length: 2,
        });
        assert_eq!(classify_security(&safe), TargetSecurity::Safe);

        let mut secure = safe.clone();
        secure.control_kind = ControlKind::SecureText;
        assert_eq!(classify_security(&secure), TargetSecurity::SecureInput);

        let mut globally_secure = safe.clone();
        globally_secure.secure_event_input = Probe::Known(true);
        assert_eq!(
            classify_security(&globally_secure),
            TargetSecurity::SecureInput
        );

        let mut unknown = safe.clone();
        unknown.accessibility_trusted = Probe::Unknown;
        assert_eq!(classify_security(&unknown), TargetSecurity::Unknown);

        let mut unsupported = safe.clone();
        unsupported.control_kind = ControlKind::Unsupported;
        assert_eq!(classify_security(&unsupported), TargetSecurity::Unknown);

        let mut unknown_control = safe.clone();
        unknown_control.control_kind = ControlKind::Unknown;
        assert_eq!(classify_security(&unknown_control), TargetSecurity::Unknown);

        let mut own_target = safe;
        own_target.belongs_to_this_process = true;
        assert_eq!(classify_security(&own_target), TargetSecurity::Unknown);

        let default_backend = Arc::new(FakeBackend::new(own_target));
        let _default_context = SafeTargetContext::new(default_backend);
    }

    #[test]
    fn reads_only_a_still_safe_exact_selection_without_truncating() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 5,
            length: 3,
        });
        backend.update(|state| state.selected_text = "原始选区".to_owned());
        let captured = capture(&context);
        assert!(captured.has_selection);

        let selection =
            block_on(context.read_selected_text(&captured.target_ref)).expect("read selected text");
        assert_eq!(selection.text.as_deref(), Some("原始选区"));
        assert!(selection.anchor_normalized_to_end);
        assert!(!selection.exceeded_limit);
        assert_eq!(backend.snapshot().selection_reads, 1);

        backend.update(|state| state.observation.secure_event_input = Probe::Unknown);
        let error = block_on(context.read_selected_text(&captured.target_ref))
            .expect_err("unknown target must not be read");
        assert_eq!(error.code, "target_context_changed");
        assert_eq!(backend.snapshot().selection_reads, 1);
    }

    #[test]
    fn revalidation_names_every_identity_and_range_change_without_content() {
        #[derive(Clone, Copy)]
        enum Change {
            Process,
            Application,
            Window,
            Control,
            RangeLocation,
            RangeLength,
        }

        let cases = [
            (
                Change::Process,
                RevalidationReason::ApplicationPidChanged,
                "reason=application_pid_changed pid=different application=same window=same control=same",
            ),
            (
                Change::Application,
                RevalidationReason::ApplicationIdentityChanged,
                "reason=application_identity_changed pid=same application=different window=same control=same",
            ),
            (
                Change::Window,
                RevalidationReason::WindowIdentityChanged,
                "reason=window_identity_changed pid=same application=same window=different control=same",
            ),
            (
                Change::Control,
                RevalidationReason::ControlIdentityChanged,
                "reason=control_identity_changed pid=same application=same window=same control=different",
            ),
            (
                Change::RangeLocation,
                RevalidationReason::SelectedRangeChanged,
                "reason=selected_range_changed expected_range_present=true current_range_present=true location_match=false length_match=true",
            ),
            (
                Change::RangeLength,
                RevalidationReason::SelectedRangeChanged,
                "reason=selected_range_changed expected_range_present=true current_range_present=true location_match=true length_match=false",
            ),
        ];

        for (change, expected_reason, expected_detail) in cases {
            let (backend, context, _clock) = adapter(TextRange {
                location: 1,
                length: 0,
            });
            let captured = capture(&context);
            backend.update(|state| match change {
                Change::Process => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("fake identity")
                        .process += 1;
                }
                Change::Application => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("fake identity")
                        .application += 1;
                }
                Change::Window => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("fake identity")
                        .window += 1;
                }
                Change::Control => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("fake identity")
                        .control += 1;
                }
                Change::RangeLocation => {
                    state.observation.selected_range = Some(TextRange {
                        location: 2,
                        length: 0,
                    });
                }
                Change::RangeLength => {
                    state.observation.selected_range = Some(TextRange {
                        location: 1,
                        length: 1,
                    });
                }
            });

            let assessment = assess(&context, &backend, &captured.target_ref);
            assert_eq!(assessment.disposition, RevalidationDisposition::Invalid);
            assert_eq!(assessment.reason, expected_reason);
            assert_eq!(assessment.diagnostic_detail(), expected_detail);
            assert_eq!(
                block_on(context.revalidate(&captured.target_ref)).expect("revalidate"),
                TargetRevalidation::Invalid
            );
        }
    }

    #[test]
    fn revalidation_names_unknown_identity_and_security_without_content() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let captured = capture(&context);
        backend.update(|state| state.compare_unknown = true);
        let identity_unknown = assess(&context, &backend, &captured.target_ref);
        assert_eq!(
            identity_unknown.disposition,
            RevalidationDisposition::Indeterminate
        );
        assert_eq!(
            identity_unknown.reason,
            RevalidationReason::IdentityIndeterminate
        );
        assert_eq!(
            identity_unknown.diagnostic_detail(),
            "reason=identity_indeterminate pid=unknown application=unknown window=unknown control=unknown"
        );
        assert_eq!(
            block_on(context.revalidate(&captured.target_ref)).expect("revalidate"),
            TargetRevalidation::Indeterminate
        );

        backend.update(|state| {
            state.compare_unknown = false;
            state.observation.belongs_to_this_process = true;
        });
        let security_unknown = assess(&context, &backend, &captured.target_ref);
        assert_eq!(
            security_unknown.disposition,
            RevalidationDisposition::Indeterminate
        );
        assert_eq!(security_unknown.reason, RevalidationReason::SecurityUnknown);
        assert_eq!(
            security_unknown.diagnostic_detail(),
            "reason=security_unknown accessibility_trusted=known_true secure_event_input=known_false control=editable_text identity_present=true range_present=true belongs_to_this_process=true"
        );
        assert_eq!(
            block_on(context.revalidate(&captured.target_ref)).expect("revalidate"),
            TargetRevalidation::Indeterminate
        );
        assert!(backend.snapshot().dispatched_texts.is_empty());
    }

    #[test]
    fn revalidation_names_secure_input_and_missing_snapshot_identity() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let captured = capture(&context);
        backend.update(|state| {
            state.observation.control_kind = ControlKind::SecureText;
            state.observation.selected_range = None;
        });
        let secure = assess(&context, &backend, &captured.target_ref);
        assert_eq!(secure.disposition, RevalidationDisposition::Invalid);
        assert_eq!(secure.reason, RevalidationReason::SecureInput);
        assert_eq!(
            secure.diagnostic_detail(),
            "reason=secure_input accessibility_trusted=known_true secure_event_input=known_false control=secure_text identity_present=true range_present=false belongs_to_this_process=false"
        );

        let mut unknown_capture = safe_observation(TextRange {
            location: 1,
            length: 0,
        });
        unknown_capture.identity = None;
        let backend = Arc::new(FakeBackend::new(unknown_capture));
        let context = SafeTargetContext::new(Arc::clone(&backend));
        let captured = capture(&context);
        backend.update(|state| {
            state.observation = safe_observation(TextRange {
                location: 1,
                length: 0,
            });
        });
        let missing = assess(&context, &backend, &captured.target_ref);
        assert_eq!(missing.disposition, RevalidationDisposition::Indeterminate);
        assert_eq!(missing.reason, RevalidationReason::SnapshotIdentityMissing);
        assert_eq!(
            missing.diagnostic_detail(),
            "reason=snapshot_identity_missing expected_identity_present=false current_identity_present=true"
        );
    }

    #[test]
    fn stable_revalidation_keeps_the_existing_valid_result() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let captured = capture(&context);
        let assessment = assess(&context, &backend, &captured.target_ref);
        assert_eq!(assessment.disposition, RevalidationDisposition::Valid);
        assert_eq!(assessment.reason, RevalidationReason::Stable);
        assert_eq!(assessment.diagnostic_detail(), "reason=stable");
        assert!(matches!(
            block_on(context.revalidate(&captured.target_ref)).expect("revalidate"),
            TargetRevalidation::Valid(_)
        ));
    }

    #[test]
    fn unknown_capture_remains_available_for_local_fallback_but_exposes_no_selection() {
        let mut observation = safe_observation(TextRange {
            location: 2,
            length: 4,
        });
        observation.accessibility_trusted = Probe::Known(false);
        let backend = Arc::new(FakeBackend::new(observation));
        let context = SafeTargetContext::new(Arc::clone(&backend));

        let captured = capture(&context);
        assert_eq!(captured.security, TargetSecurity::Unknown);
        assert!(!captured.has_selection);
        assert!(
            block_on(context.read_selected_text(&captured.target_ref)).is_err(),
            "unknown targets must not expose selected text"
        );
        assert_eq!(
            block_on(context.revalidate(&captured.target_ref)).expect("revalidate unknown"),
            TargetRevalidation::Indeterminate
        );
        assert_eq!(backend.snapshot().selection_reads, 0);
    }

    #[test]
    fn clipboard_poll_accepts_a_late_exact_verification() {
        let mut observations = [
            ClipboardInsertObservation::Pending,
            ClipboardInsertObservation::Pending,
            ClipboardInsertObservation::Verified,
        ]
        .into_iter();

        let outcome = poll_clipboard_insert_effect(3, Duration::ZERO, || {
            observations
                .next()
                .expect("one observation per configured attempt")
        });

        assert_eq!(outcome, ClipboardPasteOutcome::Inserted);
    }

    #[test]
    fn unchanged_clipboard_metadata_is_not_proof_of_non_insertion() {
        let outcome =
            poll_clipboard_insert_effect(3, Duration::ZERO, || ClipboardInsertObservation::Pending);

        assert_eq!(outcome, ClipboardPasteOutcome::Indeterminate);
    }

    #[test]
    fn temporarily_unavailable_clipboard_readback_keeps_polling() {
        let mut observations = [
            ClipboardInsertObservation::ReadbackPending,
            ClipboardInsertObservation::ReadbackPending,
            ClipboardInsertObservation::Verified,
        ]
        .into_iter();

        let outcome = poll_clipboard_insert_effect(3, Duration::ZERO, || {
            observations
                .next()
                .expect("one observation per configured attempt")
        });

        assert_eq!(outcome, ClipboardPasteOutcome::Inserted);
    }

    #[test]
    fn unavailable_clipboard_readback_uses_the_full_poll_budget() {
        let mut polls = 0;
        let outcome = poll_clipboard_insert_effect(5, Duration::ZERO, || {
            polls += 1;
            ClipboardInsertObservation::ReadbackPending
        });

        assert_eq!(outcome, ClipboardPasteOutcome::Indeterminate);
        assert_eq!(polls, 5);
    }

    #[test]
    fn unexpected_clipboard_state_stops_as_indeterminate() {
        let mut polls = 0;
        let outcome = poll_clipboard_insert_effect(5, Duration::ZERO, || {
            polls += 1;
            ClipboardInsertObservation::Unverified
        });

        assert_eq!(outcome, ClipboardPasteOutcome::Indeterminate);
        assert_eq!(polls, 1);
    }

    #[test]
    fn output_rechecks_target_range_security_and_self_window_at_the_write_point() {
        enum Change {
            Application,
            Window,
            Control,
            Range,
            SecurityUnknown,
            OwnWindow,
        }

        for change in [
            Change::Application,
            Change::Window,
            Change::Control,
            Change::Range,
            Change::SecurityUnknown,
            Change::OwnWindow,
        ] {
            let (backend, context, _clock) = adapter(TextRange {
                location: 3,
                length: 0,
            });
            let captured = capture(&context);
            let validated = validated(&context, &captured.target_ref);
            backend.update(|state| match change {
                Change::Application => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("identity")
                        .application += 1;
                }
                Change::Window => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("identity")
                        .window += 1;
                }
                Change::Control => {
                    state
                        .observation
                        .identity
                        .as_mut()
                        .expect("identity")
                        .control += 1;
                }
                Change::Range => {
                    state.observation.selected_range = Some(TextRange {
                        location: 4,
                        length: 0,
                    });
                }
                Change::SecurityUnknown => {
                    state.observation.secure_event_input = Probe::Unknown;
                }
                Change::OwnWindow => state.observation.belongs_to_this_process = true,
            });

            assert_eq!(
                block_on(context.insert(
                    validated,
                    "must-not-write".to_owned(),
                    DeliveryId::new(),
                    LifecycleFence::new(),
                ))
                .expect("fail-closed insert"),
                InsertOutcome::NotInserted
            );
            assert!(backend.snapshot().dispatched_texts.is_empty());
        }
    }

    #[test]
    fn selection_is_collapsed_to_its_end_and_content_is_only_inserted() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 4,
            length: 3,
        });
        let captured = capture(&context);
        let validated = validated(&context, &captured.target_ref);
        let outcome = block_on(context.insert(
            validated,
            "新增".to_owned(),
            DeliveryId::new(),
            LifecycleFence::new(),
        ))
        .expect("insert");

        assert_eq!(outcome, InsertOutcome::Inserted);
        let state = backend.snapshot();
        assert_eq!(state.collapse_calls, vec![7]);
        assert_eq!(state.dispatched_texts, vec!["新增"]);
        assert_eq!(state.inserted_texts, vec!["新增"]);
    }

    #[test]
    fn lifecycle_invalidation_blocks_every_external_mutation() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 4,
            length: 3,
        });
        let captured = capture(&context);
        let validated = validated(&context, &captured.target_ref);
        let lifecycle = LifecycleFence::new();
        lifecycle.invalidate();

        let outcome = block_on(context.insert(
            validated,
            "blocked".to_owned(),
            DeliveryId::new(),
            lifecycle,
        ))
        .expect("insert outcome");
        assert_eq!(outcome, InsertOutcome::NotInserted);
        let state = backend.snapshot();
        assert!(state.collapse_calls.is_empty());
        assert!(state.dispatched_texts.is_empty());
    }

    #[test]
    fn validated_token_and_delivery_are_each_single_use() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let captured = capture(&context);
        let first_token = validated(&context, &captured.target_ref);
        let duplicate_token = first_token.clone();
        let delivery = DeliveryId::new();

        assert_eq!(
            block_on(context.insert(
                first_token,
                "one".to_owned(),
                delivery,
                LifecycleFence::new(),
            ))
            .expect("first insert"),
            InsertOutcome::Inserted
        );
        assert_eq!(
            block_on(context.insert(
                duplicate_token,
                "two".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("duplicate token"),
            InsertOutcome::NotInserted
        );

        let second_token = validated(&context, &captured.target_ref);
        assert_eq!(
            block_on(context.insert(
                second_token,
                "three".to_owned(),
                delivery,
                LifecycleFence::new(),
            ))
            .expect("duplicate delivery"),
            InsertOutcome::NotInserted
        );
        assert_eq!(backend.snapshot().dispatched_texts, vec!["one"]);
    }

    #[test]
    fn distinguishes_pre_dispatch_failure_from_post_dispatch_uncertainty() {
        let (backend, context, _clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let captured = capture(&context);

        backend.update(|state| state.dispatch_mode = DispatchMode::DoNotDispatch);
        let not_dispatched = validated(&context, &captured.target_ref);
        assert_eq!(
            block_on(context.insert(
                not_dispatched,
                "no".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("not dispatched"),
            InsertOutcome::NotInserted
        );

        backend.update(|state| state.dispatch_mode = DispatchMode::Indeterminate);
        let uncertain_dispatch = validated(&context, &captured.target_ref);
        assert_eq!(
            block_on(context.insert(
                uncertain_dispatch,
                "maybe".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("uncertain dispatch"),
            InsertOutcome::Indeterminate
        );

        backend.update(|state| {
            state.dispatch_mode = DispatchMode::Dispatch;
            state.verification = InsertVerification::Unverified;
        });
        let unverified = validated(&context, &captured.target_ref);
        assert_eq!(
            block_on(context.insert(
                unverified,
                "posted".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("unverified insert"),
            InsertOutcome::Indeterminate
        );
        assert_eq!(backend.snapshot().dispatched_texts, vec!["posted"]);

        // 确证未写入必须与「不知道」区分开：Chromium／Electron 会声明
        // `AXSelectedText` 可写却丢弃写入，控件纹丝不动。若把这种情况也算作
        // `Indeterminate`，剪贴板回退将永远走不到，交付只能退化为临时文本框。
        backend.update(|state| state.verification = InsertVerification::ProvenNotInserted);
        let proven = validated(&context, &captured.target_ref);
        assert_eq!(
            block_on(context.insert(
                proven,
                "dropped".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("proven not inserted"),
            InsertOutcome::NotInserted
        );
    }

    #[test]
    fn ttl_and_capacity_remove_stale_capabilities() {
        let (_backend, context, clock) = adapter(TextRange {
            location: 1,
            length: 0,
        });
        let expired_target = capture(&context);
        clock.advance(101);
        assert_eq!(
            block_on(context.revalidate(&expired_target.target_ref)).expect("expired target"),
            TargetRevalidation::Indeterminate
        );

        let current = capture(&context);
        let expired_validation = validated(&context, &current.target_ref);
        clock.advance(21);
        assert_eq!(
            block_on(context.insert(
                expired_validation,
                "late".to_owned(),
                DeliveryId::new(),
                LifecycleFence::new(),
            ))
            .expect("expired validation"),
            InsertOutcome::NotInserted
        );

        let oldest = capture(&context);
        let _second = capture(&context);
        let _third = capture(&context);
        let _fourth = capture(&context);
        let _fifth = capture(&context);
        assert_eq!(
            block_on(context.revalidate(&oldest.target_ref)).expect("evicted target"),
            TargetRevalidation::Indeterminate
        );
    }

    #[test]
    fn selection_limit_rejects_without_truncating_or_returning_partial_text() {
        let backend = Arc::new(FakeBackend::new(safe_observation(TextRange {
            location: 2,
            length: 5,
        })));
        backend.update(|state| state.selected_text = "12345".to_owned());
        let context = SafeTargetContext::with_policy_and_clock(
            Arc::clone(&backend),
            TargetContextPolicy {
                selection_limit_chars: Some(4),
                ..TargetContextPolicy::default()
            },
            Arc::new(FakeClock::default()),
        );
        let captured = capture(&context);
        let selection =
            block_on(context.read_selected_text(&captured.target_ref)).expect("selection result");
        assert!(selection.exceeded_limit);
        assert!(selection.text.is_none());
        assert!(selection.anchor_normalized_to_end);
    }

    fn comparison(expected: u64, actual: u64) -> IdentityComparison {
        if expected == actual {
            IdentityComparison::Same
        } else {
            IdentityComparison::Different
        }
    }
}
