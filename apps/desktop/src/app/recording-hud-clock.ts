export function recordingElapsedFromAnchor(
  anchorElapsedMs: number,
  anchorTimeMs: number,
  currentTimeMs: number,
  limitMs: number | null,
): number {
  const elapsedSinceAnchor = Math.max(0, currentTimeMs - anchorTimeMs);
  const elapsed = anchorElapsedMs + elapsedSinceAnchor;
  return limitMs === null ? elapsed : Math.min(elapsed, limitMs);
}

export function recordingClockLabel(durationMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(durationMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}
