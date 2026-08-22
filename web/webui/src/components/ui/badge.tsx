import * as React from "react";
import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

/*
 * Tonal badges: a 12%-alpha wash of the status colour with the solid colour as
 * text. Reads at a glance without the solid blocks the old palette classes
 * produced, and each variant is one token so light and dark stay in step.
 */
const badgeVariants = cva(
  "focus-visible:border-ring focus-visible:ring-ring/40 aria-invalid:border-destructive inline-flex w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-full border px-2.5 py-0.5 text-[0.6875rem] font-semibold tracking-wide whitespace-nowrap transition-[color,box-shadow] focus-visible:ring-[3px] [&>svg]:pointer-events-none [&>svg]:size-3",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground border-transparent",
        secondary: "bg-secondary text-secondary-foreground border-border/70",
        destructive: "bg-destructive/10 text-destructive border-destructive/25",
        success: "bg-success/10 text-success border-success/25",
        warning: "bg-warning/15 text-warning border-warning/30",
        info: "bg-info/10 text-info border-info/25",
        muted: "bg-muted text-muted-foreground border-border/60",
        outline:
          "text-foreground border-border [a&]:hover:bg-accent [a&]:hover:text-accent-foreground",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

function Badge({
  className,
  variant,
  asChild = false,
  ...props
}: React.ComponentProps<"span"> &
  VariantProps<typeof badgeVariants> & { asChild?: boolean }) {
  const Comp = asChild ? Slot : "span";

  return (
    <Comp
      data-slot="badge"
      className={cn(badgeVariants({ variant }), className)}
      {...props}
    />
  );
}

export { Badge, badgeVariants };
