import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

type Tone = "default" | "primary" | "success" | "warning" | "destructive";

interface StatCardProps {
  title: string;
  value: string | number;
  description?: string;
  className?: string;
  icon?: ReactNode;
  tone?: Tone;
  trend?: {
    value: number;
    label: string;
  };
}

const toneStyles: Record<Tone, { tile: string; rule: string }> = {
  default: {
    tile: "bg-muted text-muted-foreground",
    rule: "from-border to-transparent",
  },
  primary: {
    tile: "bg-primary/10 text-primary",
    rule: "from-primary/60 to-transparent",
  },
  success: {
    tile: "bg-success/10 text-success",
    rule: "from-success/60 to-transparent",
  },
  warning: {
    tile: "bg-warning/15 text-warning",
    rule: "from-warning/60 to-transparent",
  },
  destructive: {
    tile: "bg-destructive/10 text-destructive",
    rule: "from-destructive/60 to-transparent",
  },
};

export function StatCard({
  title,
  value,
  description,
  className,
  icon,
  tone = "default",
  trend,
}: StatCardProps) {
  const styles = toneStyles[tone];

  return (
    <div
      className={cn(
        "bg-card text-card-foreground ring-border/70 group relative overflow-hidden rounded-2xl p-5 shadow-sm ring-1 transition-shadow hover:shadow-md",
        className,
      )}
    >
      {/* A hairline of the tone colour along the top edge, in place of a border. */}
      <span
        aria-hidden
        className={cn(
          "absolute inset-x-0 top-0 h-px bg-gradient-to-r",
          styles.rule,
        )}
      />

      <div className="flex items-start justify-between gap-3">
        <h3 className="eyebrow leading-4">{title}</h3>
        {icon && (
          <span
            aria-hidden
            className={cn(
              "flex size-8 shrink-0 items-center justify-center rounded-xl [&>svg]:size-4",
              styles.tile,
            )}
          >
            {icon}
          </span>
        )}
      </div>

      <div className="mt-3 flex items-baseline gap-2">
        <span className="numeric text-3xl leading-none font-semibold tracking-tight">
          {value}
        </span>
        {trend && (
          <span className="text-muted-foreground text-xs">
            <span
              className={cn(
                "font-semibold",
                trend.value > 0 ? "text-success" : "text-destructive",
              )}
            >
              {trend.value > 0 ? "+" : ""}
              {trend.value}
            </span>{" "}
            {trend.label}
          </span>
        )}
      </div>

      {description && (
        <p className="text-muted-foreground mt-2 text-xs leading-relaxed">
          {description}
        </p>
      )}
    </div>
  );
}
