import { cn } from "@/lib/utils";
import { FC } from "react";

type TIndicatorProps = {
  status: "ok" | "error" | "warning" | "unknown";
};

const dotStyles: Record<TIndicatorProps["status"], string> = {
  ok: "bg-success ring-success/25",
  error: "bg-destructive ring-destructive/25",
  warning: "bg-warning ring-warning/30",
  unknown: "bg-muted-foreground/40 ring-transparent",
};

export const StatusIndicator: FC<TIndicatorProps> = ({ status }) => (
  <span className="relative flex size-3 items-center justify-center">
    {status !== "unknown" && (
      <span
        aria-hidden
        className={cn(
          "absolute inline-flex size-full animate-ping rounded-full opacity-60",
          dotStyles[status].split(" ")[0],
        )}
      />
    )}
    <span
      className={cn("relative size-3 rounded-full ring-4", dotStyles[status])}
    >
      <span className="sr-only">Status is {status}</span>
    </span>
  </span>
);
