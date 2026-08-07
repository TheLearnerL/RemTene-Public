import { type BackendGateway } from "@/backend/gateway";

/**
 * 只有结构化 AppError 的稳定 code／message key 可以直接进入 UI。
 * 浏览器、Runtime 或第三方对象的任意异常文本可能包含实现细节，统一退回场景文案。
 */
export function userFacingGatewayError(
  gateway: BackendGateway,
  error: unknown,
  fallback: string,
): string {
  const formatted = gateway.formatError(error);
  return formatted.startsWith("[") ? formatted : fallback;
}
