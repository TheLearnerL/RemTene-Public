import {
  useEffect,
  useState,
} from "react";

import { useBackendGateway } from "@/backend/useBackendGateway";
import { userFacingGatewayError } from "@/backend/user-facing-error";
import { Feedback } from "@/components/ui/feedback";
import { SettingRow } from "@/components/ui/rows";
import { Section } from "@/components/ui/surface";
import { Switch } from "@/components/ui/switch";
import { type SettingsView } from "@/lib/ipc";

export function DeliverySettings() {
  const gateway = useBackendGateway();
  const [settings, setSettings] = useState<SettingsView | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    void gateway
      .getSettings()
      .then((next) => {
        if (active) setSettings(next);
      })
      .catch((error: unknown) => {
        if (active) {
          setMessage(
            userFacingGatewayError(
              gateway,
              error,
              "无法读取兼容粘贴设置，请稍后重试。",
            ),
          );
        }
      });
    return () => {
      active = false;
    };
  }, [gateway]);

  const toggle = async (allowed: boolean) => {
    setBusy(true);
    setMessage(null);
    try {
      setSettings(await gateway.setClipboardBridgeAllowed(allowed));
    } catch (error) {
      setMessage(
        userFacingGatewayError(
          gateway,
          error,
          "兼容粘贴设置未更新，请稍后重试。",
        ),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Section
      title="兼容粘贴"
      description="无法直接写入时，可以选择使用兼容粘贴。"
    >
      <SettingRow
        title="允许兼容粘贴"
        description="直接写入失败时，会在当前输入位置粘贴一次。可能替换选中的文字，请确认输入位置。"
        control={
          <Switch
            aria-label="允许兼容粘贴"
            checked={settings?.clipboard_bridge_allowed ?? false}
            disabled={busy || settings === null}
            onCheckedChange={(checked) => void toggle(checked)}
          />
        }
      />
      {message ? (
        <div className="p-4 pt-0">
          <Feedback title="兼容粘贴设置未更新" tone="error">
            {message}
          </Feedback>
        </div>
      ) : null}
    </Section>
  );
}
