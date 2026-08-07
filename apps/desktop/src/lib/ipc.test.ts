import assert from "node:assert/strict";
import test from "node:test";

import {
  assertCorrelatedContract,
  CONTRACT_VERSION,
  createHistoryClearAllCommand,
  createHistoryCopyCommand,
  createHistoryQuery,
  createLlmTestConnectionCommand,
  createResetUnrecoverableLlmSecretsCommand,
  createSetAutostartCommand,
  createSetAutoCopyResultCommand,
  createSetLlmApiKeyCommand,
  createSetHistoryEnabledCommand,
  createSetHistoryLimitCommand,
  createSetHistoryRetentionCommand,
  createSetLlmSettingsCommand,
  createSetLocalDiagnosticsCommand,
  createSetRecordingPreferencesCommand,
  createSetRecordingShortcutCommand,
  createSetTextProcessingSettingsCommand,
  createTemporaryTextCopyCommand,
  type ControlPanelNavigation,
  type HistoryPage,
  type LlmApiKeyStatusView,
  type LlmConnectionTestResult,
  type SettingsView,
  type SessionPublicSnapshot,
  type SessionTerminalEvent,
  type UserNotification,
  validateControlPanelNavigation,
  validateAutostartStatus,
  validateHistoryPage,
  validateHistoryClearAllResult,
  validateHistoryCopyResult,
  validateHistoryQuery,
  validateLlmConnectionTestResult,
  validateSessionFinishView,
  validateSessionSnapshot,
  validateSessionTerminalEvent,
  validateTemporaryTextCopyCommand,
  validateTemporaryTextCopyResult,
  validateUserNotification,
} from "./ipc.ts";

void test("recording settings commands are versioned, correlated, and content-free", () => {
  const preferences = createSetRecordingPreferencesCommand(
    7,
    "push_to_talk",
    1_200,
    "00000000-0000-4000-8000-000000000031",
  );
  assert.deepEqual(preferences, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000031",
    expected_version: 7,
    recording_mode: "push_to_talk",
    max_recording_duration_seconds: 1_200,
  });

  const shortcut = createSetRecordingShortcutCommand(
    8,
    "Command+Shift+KeyR",
    "00000000-0000-4000-8000-000000000032",
  );
  assert.deepEqual(shortcut, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000032",
    expected_version: 8,
    recording_shortcut: "Command+Shift+KeyR",
  });
  assert.equal(JSON.stringify({ preferences, shortcut }).includes("audio"), false);
});

void test("history enabled command is correlated and carries no history content", () => {
  const command = createSetHistoryEnabledCommand(
    9,
    false,
    "00000000-0000-4000-8000-000000000033",
  );
  assert.deepEqual(command, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000033",
    expected_version: 9,
    enabled: false,
  });
  assert.equal(JSON.stringify(command).includes("final_text"), false);
});

void test("history limit command carries only policy data and explicit acknowledgement", () => {
  const command = createSetHistoryLimitCommand(
    10,
    25,
    true,
    "00000000-0000-4000-8000-000000000034",
  );
  assert.deepEqual(command, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000034",
    expected_version: 10,
    limit: 25,
    acknowledge_data_loss: true,
  });
  assert.equal(JSON.stringify(command).includes("final_text"), false);
});

void test("history retention command is versioned and makes data loss explicit", () => {
  const command = createSetHistoryRetentionCommand(
    11,
    30,
    true,
    "00000000-0000-4000-8000-000000000036",
  );
  assert.deepEqual(command, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000036",
    expected_version: 11,
    retention_days: 30,
    acknowledge_data_loss: true,
  });
  assert.equal(JSON.stringify(command).includes("final_text"), false);
  assert.equal(
    createSetHistoryRetentionCommand(
      11,
      null,
      false,
      "00000000-0000-4000-8000-000000000037",
    ).retention_days,
    null,
  );
});

void test("auto-copy and local-log switches are independent content-free settings", () => {
  const autoCopy = createSetAutoCopyResultCommand(
    12,
    true,
    "00000000-0000-4000-8000-000000000038",
  );
  const diagnostics = createSetLocalDiagnosticsCommand(
    13,
    false,
    "00000000-0000-4000-8000-000000000039",
  );

  assert.deepEqual(autoCopy, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000038",
    expected_version: 12,
    enabled: true,
  });
  assert.deepEqual(diagnostics, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000039",
    expected_version: 13,
    enabled: false,
  });
  assert.equal(JSON.stringify({ autoCopy, diagnostics }).includes("text"), false);
});

void test("autostart commands are correlated and status never guesses availability", () => {
  const command = createSetAutostartCommand(
    true,
    "00000000-0000-4000-8000-000000000035",
  );
  assert.deepEqual(command, {
    contract_version: CONTRACT_VERSION,
    request_id: "00000000-0000-4000-8000-000000000035",
    enabled: true,
  });
  assert.deepEqual(
    validateAutostartStatus({
      contract_version: CONTRACT_VERSION,
      enabled: false,
    }),
    { contract_version: CONTRACT_VERSION, enabled: false },
  );
  assert.throws(
    () =>
      validateAutostartStatus({
        contract_version: CONTRACT_VERSION,
        enabled: "unknown",
      }),
    /Invalid autostart status/,
  );
  assert.throws(
    () =>
      validateAutostartStatus({
        contract_version: CONTRACT_VERSION,
        enabled: false,
        launch_path: "/tmp/must-not-cross",
      }),
    /Invalid autostart status/,
  );
});

void test("history copy carries only correlated opaque identities", () => {
  const command = createHistoryCopyCommand(
    "00000000-0000-4000-8000-000000000002",
    "00000000-0000-4000-8000-000000000001",
  );
  assert.deepEqual(Object.keys(command).sort(), [
    "contract_version",
    "record_id",
    "request_id",
  ]);
  assert.deepEqual(
    validateHistoryCopyResult(
      {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        record_id: command.record_id,
      },
      command,
    ),
    command,
  );
  assert.throws(
    () =>
      validateHistoryCopyResult(
        {
          contract_version: CONTRACT_VERSION,
          request_id: command.request_id,
          record_id: command.record_id,
          final_text: "正文不得跨入复制命令结果",
        },
        command,
      ),
    /Invalid history copy result/,
  );
  assert.throws(
    () =>
      validateHistoryCopyResult(
        {
          contract_version: CONTRACT_VERSION,
          request_id: "00000000-0000-4000-8000-000000000003",
          record_id: command.record_id,
        },
        command,
      ),
    /correlation mismatch/,
  );
});

void test("history clear requires explicit acknowledgement and a correlated safe count", () => {
  const command = createHistoryClearAllCommand(
    "00000000-0000-4000-8000-000000000001",
  );
  assert.equal(command.acknowledge_data_loss, true);
  assert.deepEqual(Object.keys(command).sort(), [
    "acknowledge_data_loss",
    "contract_version",
    "request_id",
  ]);
  assert.equal(
    validateHistoryClearAllResult(
      {
        contract_version: CONTRACT_VERSION,
        request_id: command.request_id,
        cleared_count: 2,
      },
      command.request_id,
    ).cleared_count,
    2,
  );
  assert.throws(
    () =>
      validateHistoryClearAllResult(
        {
          contract_version: CONTRACT_VERSION,
          request_id: command.request_id,
          cleared_count: -1,
        },
        command.request_id,
      ),
    /Invalid cleared history count/,
  );
});

function snapshot(
  overrides: Partial<SessionPublicSnapshot> = {},
): SessionPublicSnapshot {
  return {
    contract_version: CONTRACT_VERSION,
    session_id: "00000000-0000-4000-8000-000000000001",
    user_state: "preparing",
    phase: "preparing",
    recording_elapsed_ms: null,
    recording_limit_ms: null,
    can_finish: false,
    can_cancel: true,
    status_code: "session.preparing",
    ...overrides,
  };
}

void test("Preparing HUD allows cancellation but not finish", () => {
  assert.doesNotThrow(() => validateSessionSnapshot(snapshot()));
});

void test("Recording HUD allows both controls", () => {
  assert.doesNotThrow(() =>
    validateSessionSnapshot(
      snapshot({
        user_state: "recording",
        phase: "recording",
        can_finish: true,
        status_code: "session.recording",
      }),
    ),
  );
});

void test("invalid HUD control projections fail closed", () => {
  assert.throws(
    () => validateSessionSnapshot(snapshot({ can_finish: true })),
    /Invalid Recording HUD control projection/,
  );
  assert.throws(
    () => validateSessionSnapshot(snapshot({ can_cancel: false })),
    /Invalid Recording HUD control projection/,
  );
});

void test("all content-free processing phases use the processing user state", () => {
  for (const phase of [
    "recognizing",
    "processing",
    "delivering",
    "finalizing",
  ] as const) {
    assert.doesNotThrow(() =>
      validateSessionSnapshot(
        snapshot({
          user_state: "processing",
          phase,
          can_cancel: false,
          status_code: `session.${phase}`,
        }),
      ),
    );
  }
});

void test("mismatched HUD phase and status projections fail closed", () => {
  assert.throws(
    () =>
      validateSessionSnapshot(
        snapshot({
          user_state: "processing",
          phase: "delivering",
          can_cancel: false,
          status_code: "session.processing",
        }),
      ),
    /Invalid Recording HUD phase projection/,
  );
});

void test("session terminal events require stable content-free outcome projections", () => {
  const completed: SessionTerminalEvent = {
    contract_version: CONTRACT_VERSION,
    session_id: "00000000-0000-4000-8000-000000000001",
    outcome: "completed",
    error_code: null,
  };
  const failed: SessionTerminalEvent = {
    ...completed,
    outcome: "failed",
    error_code: "session.failed.asr",
  };

  assert.deepEqual(validateSessionTerminalEvent(completed), completed);
  assert.deepEqual(validateSessionTerminalEvent(failed), failed);
  assert.throws(
    () => validateSessionTerminalEvent({ ...completed, error_code: "session.failed.asr" }),
    /Invalid Session terminal error projection/,
  );
  assert.throws(
    () => validateSessionTerminalEvent({ ...failed, error_code: null }),
    /Invalid Session terminal error projection/,
  );
  assert.throws(
    () =>
      validateSessionTerminalEvent({
        ...failed,
        error_code: "session.failed.typo",
      } as unknown as SessionTerminalEvent),
    /Invalid Session terminal error projection/,
  );
  assert.throws(
    () =>
      validateSessionTerminalEvent({
        ...failed,
        final_text: "must never cross this event",
      } as SessionTerminalEvent),
    /Invalid Session terminal projection/,
  );
});

void test("session finish result rejects an incompatible contract version", () => {
  const finish = {
    contract_version: CONTRACT_VERSION,
    status: "delivered" as const,
    delivery: "inserted" as const,
    notice: null,
    failure: null,
  };

  assert.deepEqual(validateSessionFinishView(finish), finish);
  assert.throws(
    () =>
      validateSessionFinishView({
        ...finish,
        contract_version: CONTRACT_VERSION + 1,
      }),
    /IPC contract mismatch/,
  );
});

void test("user notifications are versioned, content-free, and closed to unknown codes", () => {
  const notification: UserNotification = {
    contract_version: CONTRACT_VERSION,
    session_id: "00000000-0000-4000-8000-000000000001",
    code: "notification.asr",
  };

  assert.deepEqual(validateUserNotification(notification), notification);
  assert.deepEqual(Object.keys(notification).sort(), [
    "code",
    "contract_version",
    "session_id",
  ]);
  assert.throws(
    () =>
      validateUserNotification({
        ...notification,
        final_text: "must never cross this event",
      }),
    /Invalid user notification projection/,
  );
  assert.throws(
    () =>
      validateUserNotification({
        ...notification,
        code: "notification.typo",
      }),
    /Invalid user notification code/,
  );
  assert.throws(
    () =>
      validateUserNotification({
        ...notification,
        session_id: "not-a-session-id",
      }),
    /Invalid user notification session ID/,
  );
  assert.throws(
    () =>
      validateUserNotification({
        ...notification,
        contract_version: CONTRACT_VERSION + 1,
      }),
    /IPC contract mismatch/,
  );
});

void test("control panel navigation only accepts the two model destinations", () => {
  for (const target of ["model.asr", "model.text_service"] as const) {
    const navigation: ControlPanelNavigation = {
      contract_version: CONTRACT_VERSION,
      target,
    };
    assert.deepEqual(validateControlPanelNavigation(navigation), navigation);
  }

  assert.throws(
    () =>
      validateControlPanelNavigation({
        contract_version: CONTRACT_VERSION,
        target: "model",
      }),
    /Invalid control panel navigation target/,
  );
  assert.throws(
    () =>
      validateControlPanelNavigation({
        contract_version: CONTRACT_VERSION,
        target: "model.asr",
        arbitrary_path: "#system",
      }),
    /Invalid control panel navigation projection/,
  );
});

void test("temporary text copy is closed, versioned, and correlated by delivery ID", () => {
  const deliveryId = "00000000-0000-4000-8000-000000000007";
  const command = createTemporaryTextCopyCommand(deliveryId);
  const result = {
    contract_version: CONTRACT_VERSION,
    delivery_id: deliveryId,
  };

  assert.deepEqual(command, result);
  assert.deepEqual(validateTemporaryTextCopyCommand(command), command);
  assert.deepEqual(
    validateTemporaryTextCopyResult(result, deliveryId),
    result,
  );
  assert.throws(
    () =>
      validateTemporaryTextCopyCommand({
        ...command,
        final_text: "must never be sent back to the copy command",
      }),
    /Invalid temporary text copy command/,
  );
  assert.throws(
    () =>
      validateTemporaryTextCopyResult(
        { ...result, clipboard_text: "must never return" },
        deliveryId,
      ),
    /Invalid temporary text copy result/,
  );
  assert.throws(
    () =>
      validateTemporaryTextCopyResult(
        {
          ...result,
          delivery_id: "00000000-0000-4000-8000-000000000008",
        },
        deliveryId,
      ),
    /Temporary text copy correlation mismatch/,
  );
  assert.throws(
    () => createTemporaryTextCopyCommand("not-a-delivery-id"),
    /Invalid temporary text delivery ID/,
  );
});

void test("history query and page accept only the exact read-only schema", () => {
  const query = createHistoryQuery();
  const page: HistoryPage = {
    contract_version: CONTRACT_VERSION,
    records: [
      {
        record_id: "00000000-0000-4000-8000-000000000021",
        final_text: "第一条真实历史",
        created_at: "2026-07-31T02:24:00Z",
      },
      {
        record_id: "00000000-0000-4000-8000-000000000022",
        final_text: "第二条真实历史",
        created_at: "2026-07-30T10:40:00.123Z",
      },
    ],
  };

  assert.deepEqual(query, { contract_version: CONTRACT_VERSION });
  assert.deepEqual(validateHistoryQuery(query), query);
  assert.deepEqual(validateHistoryPage(page), page);
  assert.equal(validateHistoryPage(page).records[0]?.final_text, "第一条真实历史");
  assert.equal(validateHistoryPage(page).records[1]?.final_text, "第二条真实历史");

  assert.throws(
    () => validateHistoryQuery({ ...query, cursor: "not-approved" }),
    /Invalid history query/,
  );
  assert.throws(
    () =>
      validateHistoryQuery({
        contract_version: CONTRACT_VERSION + 1,
      }),
    /IPC contract mismatch/,
  );
  assert.throws(
    () => validateHistoryPage({ ...page, total: 2 }),
    /Invalid history page projection/,
  );
  assert.throws(
    () =>
      validateHistoryPage({
        ...page,
        contract_version: CONTRACT_VERSION + 1,
      }),
    /IPC contract mismatch/,
  );

  const versionSevenPage: HistoryPage = {
    contract_version: CONTRACT_VERSION,
    records: [
      {
        record_id: "01890f31-a3c2-7f4a-8a77-9f4c0176f821",
        final_text: "未来 UUID 版本仍是合法不透明标识",
        created_at: "2026-07-31T02:24:00Z",
      },
    ],
  };
  assert.deepEqual(validateHistoryPage(versionSevenPage), versionSevenPage);
});

void test("history records reject invalid identity, content, time, and unknown fields", () => {
  const record = {
    record_id: "00000000-0000-4000-8000-000000000023",
    final_text: "有效正文",
    created_at: "2026-07-31T02:24:00Z",
  };
  const page = {
    contract_version: CONTRACT_VERSION,
    records: [record],
  };

  assert.throws(
    () =>
      validateHistoryPage({
        ...page,
        records: [{ ...record, record_id: "not-a-uuid" }],
      }),
    /Invalid history record ID/,
  );
  for (const finalText of ["", "   "]) {
    assert.throws(
      () =>
        validateHistoryPage({
          ...page,
          records: [{ ...record, final_text: finalText }],
        }),
      /Invalid history record text/,
    );
  }
  for (const createdAt of [
    "2026-07-31T10:24:00+08:00",
    "2026-07-31 02:24:00Z",
    "2026-02-30T02:24:00Z",
    "not-a-date",
  ]) {
    assert.throws(
      () =>
        validateHistoryPage({
          ...page,
          records: [{ ...record, created_at: createdAt }],
        }),
      /Invalid history record timestamp/,
    );
  }
  assert.throws(
    () =>
      validateHistoryPage({
        ...page,
        records: [{ ...record, audio_path: "/must/not/cross" }],
      }),
    /Invalid history record projection/,
  );

  assert.throws(
    () =>
      validateHistoryPage({
        contract_version: CONTRACT_VERSION,
        records: [
          record,
          {
            ...record,
            record_id: "00000000-0000-4000-8000-000000000024",
            created_at: "2026-08-01T02:24:00Z",
          },
        ],
      }),
    /Invalid history record order/,
  );
  assert.throws(
    () =>
      validateHistoryPage({
        contract_version: CONTRACT_VERSION,
        records: [record, record],
      }),
    /Duplicate history record ID/,
  );
});

void test("LLM settings and API key status serialize without secret metadata", () => {
  const settings: SettingsView = {
    contract_version: CONTRACT_VERSION,
    version: 9,
    recording_mode: "push_to_talk",
    max_recording_duration_seconds: 600,
    recording_shortcut: "command+shift+KeyR",
    processing_mode: "faithful",
    read_selected_text: true,
    clipboard_bridge_allowed: true,
    auto_copy_result: false,
    local_diagnostics_enabled: true,
    history_policy: { enabled: true, limit: 10, retention_days: null },
    llm: {
      base_url: "https://provider.invalid/v1",
      model: "test-model",
    },
  };
  const status: LlmApiKeyStatusView = {
    contract_version: CONTRACT_VERSION,
    state: "configured",
    storage: "encrypted_local",
  };

  const serialized = JSON.stringify({ settings, status });
  assert.match(serialized, /"recording_mode":"push_to_talk"/);
  assert.match(serialized, /"max_recording_duration_seconds":600/);
  assert.match(serialized, /"recording_shortcut":"command\+shift\+KeyR"/);
  assert.match(serialized, /"clipboard_bridge_allowed":true/);
  assert.match(serialized, /"processing_mode":"faithful"/);
  assert.match(serialized, /"read_selected_text":true/);
  assert.match(serialized, /"storage":"encrypted_local"/);
  for (const forbidden of [
    "secret_value",
    "api_key",
    "secret_id",
    "fingerprint",
    "prefix",
    "suffix",
    "length",
    "updated_at",
  ]) {
    assert.equal(serialized.includes(forbidden), false, `${forbidden} leaked`);
  }
});

void test("secret mutation commands are versioned, correlated, and explicit", () => {
  const requestId = "00000000-0000-4000-8000-000000000002";
  const marker = "sk-plain-text-test-marker";
  const set = createSetLlmApiKeyCommand(marker, requestId);
  const reset = createResetUnrecoverableLlmSecretsCommand(true, requestId);

  assert.deepEqual(set, {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    secret_value: marker,
  });
  assert.deepEqual(reset, {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    acknowledge_data_loss: true,
  });
});

void test("LLM settings mutation carries optimistic version and no API key", () => {
  const requestId = "00000000-0000-4000-8000-000000000003";
  const command = createSetLlmSettingsCommand(
    12,
    {
      base_url: "https://provider.invalid/v1",
      model: "test-model",
    },
    requestId,
  );

  assert.equal(command.expected_version, 12);
  assert.equal(command.request_id, requestId);
  assert.equal(JSON.stringify(command).includes("secret"), false);
  assert.equal(JSON.stringify(command).includes("api_key"), false);
});

void test("text processing settings mutation is correlated and atomic", () => {
  const requestId = "00000000-0000-4000-8000-000000000004";
  const command = createSetTextProcessingSettingsCommand(
    8,
    "structured",
    true,
    requestId,
  );

  assert.deepEqual(command, {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    expected_version: 8,
    processing_mode: "structured",
    read_selected_text: true,
  });
});

void test("connection test input cannot contain key or user content", () => {
  const requestId = "00000000-0000-4000-8000-000000000005";
  const command = createLlmTestConnectionCommand(requestId);
  assert.deepEqual(Object.keys(command).sort(), ["contract_version", "request_id"]);

  const serialized = JSON.stringify(command);
  for (const forbidden of [
    "secret",
    "api_key",
    "prompt",
    "text",
    "transcript",
    "selection",
    "base_url",
    "model",
  ]) {
    assert.equal(serialized.includes(forbidden), false, `${forbidden} leaked`);
  }
});

void test("new IPC results reject contract and request correlation mismatches", () => {
  const requestId = "00000000-0000-4000-8000-000000000006";
  const valid = {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
  };

  assert.doesNotThrow(() => assertCorrelatedContract(valid, requestId, "test"));
  assert.throws(
    () =>
      assertCorrelatedContract(
        { ...valid, contract_version: CONTRACT_VERSION + 1 },
        requestId,
        "test",
      ),
    /IPC contract mismatch/,
  );
  assert.throws(
    () => assertCorrelatedContract(valid, "00000000-0000-4000-8000-000000000099", "test"),
    /correlation mismatch/,
  );
});

void test("connection test result enforces stable status and error codes", () => {
  const requestId = "00000000-0000-4000-8000-000000000006";
  const succeeded: LlmConnectionTestResult = {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    status: "succeeded",
    error_code: null,
    upstream_error: null,
  };
  const failed: LlmConnectionTestResult = {
    contract_version: CONTRACT_VERSION,
    request_id: requestId,
    status: "failed",
    error_code: "authentication_failed",
    upstream_error: null,
  };
  const busy: LlmConnectionTestResult = {
    ...failed,
    error_code: "busy",
  };
  const permissionDenied: LlmConnectionTestResult = {
    ...failed,
    error_code: "permission_denied",
  };
  const withUpstream: LlmConnectionTestResult = {
    ...failed,
    upstream_error: {
      http_status: 401,
      response_body: '{"error":{"message":"invalid token"}}',
      truncated: false,
    },
  };

  assert.deepEqual(validateLlmConnectionTestResult(succeeded, requestId), succeeded);
  assert.deepEqual(validateLlmConnectionTestResult(failed, requestId), failed);
  assert.deepEqual(validateLlmConnectionTestResult(busy, requestId), busy);
  assert.deepEqual(validateLlmConnectionTestResult(withUpstream, requestId), withUpstream);
  assert.deepEqual(
    validateLlmConnectionTestResult(permissionDenied, requestId),
    permissionDenied,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        { ...succeeded, error_code: "timeout" },
        requestId,
      ),
    /Invalid LLM connection test result/,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        { ...failed, error_code: null },
        requestId,
      ),
    /Invalid LLM connection test result/,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        { ...failed, status: "unknown" as "failed" },
        requestId,
      ),
    /Invalid LLM connection test result/,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        { ...succeeded, upstream_error: withUpstream.upstream_error },
        requestId,
      ),
    /Invalid LLM connection test result/,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        {
          ...failed,
          upstream_error: {
            http_status: 99,
            response_body: "invalid",
            truncated: false,
          },
        },
        requestId,
      ),
    /Invalid LLM upstream error response/,
  );
  assert.throws(
    () =>
      validateLlmConnectionTestResult(
        {
          ...failed,
          upstream_error: {
            http_status: 401,
            response_body: "x".repeat(16 * 1024 + 1),
            truncated: true,
          },
        },
        requestId,
      ),
    /Invalid LLM upstream error response/,
  );
});
