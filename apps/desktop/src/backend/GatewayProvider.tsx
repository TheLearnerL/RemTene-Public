import { type ReactNode } from "react";

import { GatewayContext } from "@/backend/GatewayContext";
import {
  type BackendGateway,
  tauriBackendGateway,
} from "@/backend/gateway";

export function GatewayProvider({
  children,
  gateway = tauriBackendGateway,
}: {
  children: ReactNode;
  gateway?: BackendGateway;
}) {
  return (
    <GatewayContext.Provider value={gateway}>{children}</GatewayContext.Provider>
  );
}
