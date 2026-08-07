import {
  useEffect,
  useState,
} from "react";

import {
  type AppDomain,
  domainHref,
  parseAppDomain,
} from "@/app/navigation";
import { useBackendGateway } from "@/backend/useBackendGateway";
import { classes } from "@/lib/classes";
import { type ControlPanelNavigationTarget } from "@/lib/ipc";
import { ModelPage } from "@/pages/ModelPage";
import { OutputPage } from "@/pages/OutputPage";
import { RecordingPage } from "@/pages/RecordingPage";
import { StatusPage } from "@/pages/StatusPage";
import { SystemPage } from "@/pages/SystemPage";

interface NavItem {
  domain: AppDomain;
  label: string;
}

const navItems: NavItem[] = [
  { domain: "status", label: "状态" },
  { domain: "recording", label: "录音" },
  { domain: "model", label: "模型" },
  { domain: "output", label: "输出" },
  { domain: "system", label: "系统" },
];

export function AppShell() {
  const gateway = useBackendGateway();
  const [domain, setDomain] = useState<AppDomain>(() =>
    parseAppDomain(window.location.hash),
  );
  const [statusRevision, setStatusRevision] = useState(0);
  const [modelNavigation, setModelNavigation] = useState<{
    target: ControlPanelNavigationTarget;
    revision: number;
  } | null>(null);

  useEffect(() => {
    const update = () => setDomain(parseAppDomain(window.location.hash));
    window.addEventListener("hashchange", update);
    return () => window.removeEventListener("hashchange", update);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void gateway
      .listenToControlPanelNavigation(({ target }) => {
        if (!active) return;
        setModelNavigation((current) => ({
          target,
          revision: (current?.revision ?? 0) + 1,
        }));
        window.location.hash = "model";
        setDomain("model");
      })
      .then((stopListening) => {
        if (!active) {
          stopListening();
          return;
        }
        unlisten = stopListening;
      })
      .catch(() => undefined);

    return () => {
      active = false;
      unlisten?.();
    };
  }, [gateway]);

  const navigate = (next: AppDomain) => {
    if (next === domain) return;
    window.location.hash = next;
    setDomain(next);
  };

  return (
    <div className="app-shell">
      <div
        className="app-window-drag-region"
        data-tauri-drag-region
        aria-hidden="true"
      />
      <aside className="app-sidebar">
        <div className="app-brand" aria-label="辑语">
          <span className="app-brand-copy">
            <strong>辑语</strong>
            <small>本地语音输入</small>
          </span>
        </div>

        <nav className="app-navigation" aria-label="控制面板">
          {navItems.map((item) => {
            const selected = item.domain === domain;
            return (
              <a
                key={item.domain}
                href={domainHref(item.domain)}
                className={classes("app-nav-item", selected && "is-current")}
                aria-current={selected ? "page" : undefined}
                aria-label={item.label}
                title={item.label}
                onClick={() => setDomain(item.domain)}
              >
                <span className="app-nav-dot" aria-hidden="true" />
                <span className="app-nav-label">{item.label}</span>
              </a>
            );
          })}
        </nav>

      </aside>

      <main className="app-content" id={`page-${domain}`}>
        <div className="app-page">
          {domain === "status" ? (
            <StatusPage key={statusRevision} onNavigate={navigate} />
          ) : null}
          {domain === "recording" ? (
            <RecordingPage onNavigate={navigate} />
          ) : null}
          {domain === "model" ? (
            <ModelPage
              key={`model-${modelNavigation?.revision ?? 0}`}
              onChanged={() => setStatusRevision((current) => current + 1)}
              navigationTarget={modelNavigation?.target}
            />
          ) : null}
          {domain === "output" ? <OutputPage /> : null}
          {domain === "system" ? <SystemPage /> : null}
        </div>
      </main>
    </div>
  );
}
