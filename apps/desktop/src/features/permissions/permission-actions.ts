import {
  type MicrophonePermission,
  type SystemPermission,
} from "../../lib/ipc.ts";

export type PermissionRepairAction = "request" | "open-settings" | null;

/** Maps only verified OS states to a user-initiated repair action. */
export function permissionRepairAction(
  permission: MicrophonePermission | SystemPermission | undefined,
): PermissionRepairAction {
  switch (permission) {
    case "not_determined":
      return "request";
    case "denied":
      return "open-settings";
    default:
      return null;
  }
}
