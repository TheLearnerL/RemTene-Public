import { Feedback } from "@/components/ui/feedback";

export function UnavailableSection({
  title,
  description,
  kind = "not_implemented",
}: {
  title: string;
  description: string;
  kind?: "not_implemented" | "unavailable";
}) {
  return (
    <Feedback
      title={title}
      tone={kind === "unavailable" ? "error" : "neutral"}
    >
      <p>{description}</p>
      <p className="mt-1 font-medium">
        {kind === "unavailable"
          ? "暂时无法使用，请稍后重试。"
          : "此功能暂未开放。"}
      </p>
    </Feedback>
  );
}
