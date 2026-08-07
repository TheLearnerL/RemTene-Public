import { Button } from "@/components/ui/button";
import { Feedback } from "@/components/ui/feedback";
import { SettingRow } from "@/components/ui/rows";
import { Section } from "@/components/ui/surface";
import { permissionLabel } from "@/features/permissions/labels";
import { usePermissions } from "@/features/permissions/usePermissions";

export function PermissionPanel() {
  const { status, busyAction, message, run } = usePermissions();
  const busy = busyAction !== null;

  return (
    <Section
      title="系统权限"
      description={`请在系统设置中找到“${status?.app_display_name ?? "辑语"}”；部分系统可能显示为“${status?.process_name ?? "remtene-desktop"}”。`}
    >
      <SettingRow
        title="麦克风"
        description={`当前状态：${permissionLabel(status?.microphone)}。只在明确触发的录音期间开启。`}
        control={
          <div className="flex flex-wrap justify-end gap-2">
            <Button
              disabled={busy}
              onClick={() => void run("request_microphone")}
            >
              请求授权
            </Button>
            <Button
              variant="ghost"
              disabled={busy}
              onClick={() => void run("open_microphone")}
            >
              打开设置
            </Button>
          </div>
        }
      />
      <SettingRow
        title="辅助功能"
        description={`当前状态：${permissionLabel(status?.accessibility)}。用于直接写入文字，不影响本地原始转录。`}
        control={
          <div className="flex flex-wrap justify-end gap-2">
            <Button
              disabled={busy}
              onClick={() => void run("request_accessibility")}
            >
              请求授权
            </Button>
            <Button
              variant="ghost"
              disabled={busy}
              onClick={() => void run("open_accessibility")}
            >
              打开设置
            </Button>
          </div>
        }
      />
      {message ? (
        <div className="p-4 pt-0">
          <Feedback title="权限操作未完成" tone="error">
            {message}
          </Feedback>
        </div>
      ) : null}
    </Section>
  );
}
