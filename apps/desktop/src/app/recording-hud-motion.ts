export const RECORDING_HUD_EXIT_DURATION_MS = 210;

export function recordingHudExitDelay(prefersReducedMotion: boolean): number {
  return prefersReducedMotion ? 0 : RECORDING_HUD_EXIT_DURATION_MS;
}

export async function waitForRecordingHudExit(
  prefersReducedMotion: boolean,
): Promise<void> {
  const delay = recordingHudExitDelay(prefersReducedMotion);
  if (delay === 0) return;
  await new Promise<void>((resolve) => {
    globalThis.setTimeout(resolve, delay);
  });
}
