import { useContext } from "react";

import { GatewayContext } from "@/backend/GatewayContext";
import { type BackendGateway } from "@/backend/gateway";

export function useBackendGateway(): BackendGateway {
  const gateway = useContext(GatewayContext);
  if (gateway === null) {
    throw new Error("GatewayProvider is required");
  }
  return gateway;
}
