import { cn } from "@/lib/utils";
import { LogoMark } from "@/components/brand/LogoMark";

interface LoadingSpinnerProps {
  className?: string;
  size?: "sm" | "md" | "lg";
  showText?: boolean;
}

const markSize = {
  sm: "size-10",
  md: "size-16",
  lg: "size-24",
} as const;

const textSize = {
  sm: "text-xs",
  md: "text-sm",
  lg: "text-base",
} as const;

export function LoadingSpinner({
  className,
  size = "md",
  showText = true,
}: LoadingSpinnerProps) {
  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-4">
      <span className="relative flex items-center justify-center">
        {/* Ring that sweeps around the mark, so the logo itself stays upright
            and legible instead of tumbling. */}
        <span
          aria-hidden
          className={cn(
            "border-primary/25 border-t-primary absolute animate-spin rounded-full border-2 [animation-duration:1.1s]",
            size === "sm" ? "size-16" : size === "md" ? "size-24" : "size-36",
          )}
        />
        <LogoMark
          className={cn(markSize[size], "animate-pulse", className)}
          title="Loading"
        />
      </span>
      {showText && (
        <span
          className={cn(
            "text-muted-foreground animate-pulse tracking-wide",
            textSize[size],
          )}
        >
          Loading…
        </span>
      )}
    </div>
  );
}
