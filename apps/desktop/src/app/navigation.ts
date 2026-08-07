export const APP_DOMAINS = [
  "status",
  "recording",
  "model",
  "output",
  "system",
] as const;

export type AppDomain = (typeof APP_DOMAINS)[number];

export function parseAppDomain(hash: string): AppDomain {
  const candidate = hash.replace(/^#\/?/, "");
  return APP_DOMAINS.includes(candidate as AppDomain)
    ? (candidate as AppDomain)
    : "status";
}

export function domainHref(domain: AppDomain): string {
  return `#${domain}`;
}
