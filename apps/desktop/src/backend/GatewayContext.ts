import { createContext } from "react";

import { type BackendGateway } from "@/backend/gateway";

export const GatewayContext = createContext<BackendGateway | null>(null);
