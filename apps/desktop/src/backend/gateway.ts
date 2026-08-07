import {
  type AppSnapshot,
  type AutostartStatusView,
  type CommandAccepted,
  type ControlPanelNavigation,
  type HistoryPage,
  type HistoryCopyResult,
  type HistoryClearAllResult,
  type LlmApiKeyStatusView,
  type LlmConnectionTestResult,
  type LlmSettingsView,
  type LocalAsrModel,
  type PermissionStatusView,
  type ProcessingMode,
  type RevealLlmApiKeyResult,
  type SessionEnded,
  type SessionFinishView,
  type SessionPublicSnapshot,
  type SessionTerminalEvent,
  type SettingsView,
  type TemporaryTextDelivery,
  type TemporaryTextCopyResult,
  type UserNotification,
  applyNotificationAction,
  cancelRecording,
  cancelSession,
  checkAsrHealth,
  switchAsrModel,
  copyTemporaryText,
  deleteLlmApiKey,
  dismissTemporaryText,
  finishRecording,
  finishSession,
  formatIpcError,
  getAppSnapshot,
  getAutostartStatus,
  getLlmApiKeyStatus,
  listHistory,
  copyHistoryRecord,
  clearAllHistory,
  getPendingTemporaryText,
  getPendingNotification,
  getPermissionStatus,
  getRecordingHudState,
  getSettings,
  listenToRecordingHudState,
  listenToSessionEnded,
  listenToSessionTerminal,
  listenToTemporaryText,
  listenToControlPanelNavigation,
  listenToAppSnapshotChanged,
  listenToNotificationRaised,
  openAccessibilitySettings,
  openModelDirectory,
  openMicrophoneSettings,
  requestAccessibilityPermission,
  requestMicrophonePermission,
  resetUnrecoverableLlmSecrets,
  revealLlmApiKey,
  setClipboardBridgeAllowed,
  setHistoryEnabled,
  setHistoryLimit,
  setHistoryRetention,
  setAutoCopyResult,
  setLocalDiagnosticsEnabled,
  openDiagnosticsDirectory,
  setAutostartEnabled,
  setLlmApiKey,
  setLlmSettings,
  setRecordingPreferences,
  setRecordingShortcut,
  setTextProcessingSettings,
  startSession,
  testLlmConnection,
} from "@/lib/ipc";

export type Unsubscribe = () => void;

/**
 * React 只依赖这组面向功能的能力。Tauri 命令名、事件名和 invoke 细节继续由
 * `lib/ipc.ts` 持有，测试或 Story（组件场景）可以注入同一接口的受控实现。
 */
export interface BackendGateway {
  formatError(error: unknown): string;
  getAppSnapshot(): Promise<AppSnapshot>;
  getAutostartStatus(): Promise<AutostartStatusView>;
  setAutostartEnabled(enabled: boolean): Promise<AutostartStatusView>;
  checkAsrHealth(): Promise<AppSnapshot>;
  switchAsrModel(engine: LocalAsrModel): Promise<AppSnapshot>;
  listenToAppSnapshotChanged(
    onSnapshot: (snapshot: AppSnapshot) => void,
  ): Promise<Unsubscribe>;
  openModelDirectory(): Promise<void>;
  listenToRecordingState(
    onState: (snapshot: SessionPublicSnapshot) => void,
  ): Promise<Unsubscribe>;
  listenToSessionEnded(onEnded: (event: SessionEnded) => void): Promise<Unsubscribe>;
  listenToSessionTerminal(
    onTerminal: (event: SessionTerminalEvent) => void,
  ): Promise<Unsubscribe>;

  startSession(): Promise<string>;
  finishSession(sessionId: string): Promise<SessionFinishView>;
  cancelSession(sessionId: string): Promise<void>;

  getPermissionStatus(): Promise<PermissionStatusView>;
  requestMicrophonePermission(): Promise<PermissionStatusView>;
  requestAccessibilityPermission(): Promise<PermissionStatusView>;
  openMicrophoneSettings(): Promise<void>;
  openAccessibilitySettings(): Promise<void>;

  getSettings(): Promise<SettingsView>;
  setRecordingPreferences(
    expectedVersion: number,
    recordingMode: SettingsView["recording_mode"],
    maxRecordingDurationSeconds: number,
  ): Promise<SettingsView>;
  setRecordingShortcut(
    expectedVersion: number,
    recordingShortcut: string | null,
  ): Promise<SettingsView>;
  setHistoryEnabled(
    expectedVersion: number,
    enabled: boolean,
  ): Promise<SettingsView>;
  setHistoryLimit(
    expectedVersion: number,
    limit: number,
    acknowledgeDataLoss: boolean,
  ): Promise<SettingsView>;
  setHistoryRetention(
    expectedVersion: number,
    retentionDays: number | null,
    acknowledgeDataLoss: boolean,
  ): Promise<SettingsView>;
  setAutoCopyResult(
    expectedVersion: number,
    enabled: boolean,
  ): Promise<SettingsView>;
  setLocalDiagnosticsEnabled(
    expectedVersion: number,
    enabled: boolean,
  ): Promise<SettingsView>;
  openDiagnosticsDirectory(): Promise<void>;
  setTextProcessingSettings(
    expectedVersion: number,
    processingMode: ProcessingMode,
    readSelectedText: boolean,
  ): Promise<SettingsView>;
  setClipboardBridgeAllowed(allowed: boolean): Promise<SettingsView>;
  setLlmSettings(
    expectedVersion: number,
    llm: LlmSettingsView | null,
  ): Promise<SettingsView>;
  getLlmApiKeyStatus(): Promise<LlmApiKeyStatusView>;
  setLlmApiKey(secretValue: string): Promise<LlmApiKeyStatusView>;
  revealLlmApiKey(): Promise<RevealLlmApiKeyResult>;
  deleteLlmApiKey(): Promise<LlmApiKeyStatusView>;
  resetUnrecoverableLlmSecrets(
    acknowledgeDataLoss: true,
  ): Promise<LlmApiKeyStatusView>;
  testLlmConnection(): Promise<LlmConnectionTestResult>;
  listHistory(): Promise<HistoryPage>;
  copyHistoryRecord(recordId: string): Promise<HistoryCopyResult>;
  clearAllHistory(): Promise<HistoryClearAllResult>;

  getRecordingHudState(): Promise<SessionPublicSnapshot | null>;
  finishRecording(sessionId: string): Promise<CommandAccepted>;
  cancelRecording(sessionId: string): Promise<CommandAccepted>;

  getPendingTemporaryText(): Promise<TemporaryTextDelivery | null>;
  listenToTemporaryText(
    onDelivery: (delivery: TemporaryTextDelivery) => void,
  ): Promise<Unsubscribe>;
  dismissTemporaryText(): Promise<void>;
  copyTemporaryText(deliveryId: string): Promise<TemporaryTextCopyResult>;

  getPendingNotification(): Promise<UserNotification | null>;
  listenToNotificationRaised(
    onNotification: (notification: UserNotification) => void,
  ): Promise<Unsubscribe>;
  applyNotificationAction(notification: UserNotification): Promise<void>;
  listenToControlPanelNavigation(
    onNavigation: (navigation: ControlPanelNavigation) => void,
  ): Promise<Unsubscribe>;
}

export const tauriBackendGateway: BackendGateway = {
  formatError: formatIpcError,
  getAppSnapshot,
  getAutostartStatus,
  setAutostartEnabled,
  checkAsrHealth,
  switchAsrModel,
  listenToAppSnapshotChanged,
  openModelDirectory,
  listenToRecordingState: listenToRecordingHudState,
  listenToSessionEnded,
  listenToSessionTerminal,
  startSession,
  finishSession,
  cancelSession,
  getPermissionStatus,
  requestMicrophonePermission,
  requestAccessibilityPermission,
  openMicrophoneSettings,
  openAccessibilitySettings,
  getSettings,
  setRecordingPreferences,
  setRecordingShortcut,
  setHistoryEnabled,
  setHistoryLimit,
  setHistoryRetention,
  setAutoCopyResult,
  setLocalDiagnosticsEnabled,
  openDiagnosticsDirectory,
  setTextProcessingSettings,
  setClipboardBridgeAllowed,
  setLlmSettings,
  getLlmApiKeyStatus,
  setLlmApiKey,
  revealLlmApiKey,
  deleteLlmApiKey,
  resetUnrecoverableLlmSecrets,
  testLlmConnection,
  listHistory,
  copyHistoryRecord,
  clearAllHistory,
  getRecordingHudState,
  finishRecording,
  cancelRecording,
  getPendingTemporaryText,
  listenToTemporaryText,
  dismissTemporaryText,
  copyTemporaryText,
  getPendingNotification,
  listenToNotificationRaised,
  applyNotificationAction,
  listenToControlPanelNavigation,
};
