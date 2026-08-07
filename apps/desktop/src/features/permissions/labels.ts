import {
  type MicrophonePermission,
  type SystemPermission,
} from "@/lib/ipc";

export function permissionLabel(
  value: MicrophonePermission | SystemPermission | undefined,
): string {
  switch (value) {
    case "granted":
      return "已授权";
    case "denied":
      return "已拒绝";
    case "not_determined":
      return "尚未授权";
    case "not_required":
      return "当前平台不需要";
    case "inherited_from_launcher":
      return "借用启动器授权";
    default:
      return "状态未知";
  }
}
