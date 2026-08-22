import { useFormContext } from "@/contexts/Form/form-context";
import { Button } from "@/components/ui/button";
import { Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";

interface SubmitButtonProps {
  label?: string;
  actionLabel?: string;
  size?: "sm" | "md" | "lg";
}

function SubmitButton({ label, actionLabel, size = "md" }: SubmitButtonProps) {
  const form = useFormContext();

  return (
    <>
      <form.Subscribe
        selector={(state) => [state.canSubmit, state.isSubmitting]}
      >
        {([canSubmit, isSubmitting]) => (
          <div className="form__submit sticky right-0 bottom-0 left-0 z-10 w-full pt-6 pb-1">
            <Button
              type="submit"
              className={cn("w-full", {
                "opacity-70": isSubmitting,
                "h-12 text-base": size === "lg",
                "h-10": size === "md",
                "h-8 text-sm": size === "sm",
              })}
              disabled={!canSubmit || isSubmitting}
            >
              {isSubmitting ? (
                <>
                  <Loader2 className="size-4 animate-spin" />{" "}
                  {actionLabel ?? "Saving..."}
                </>
              ) : (
                <>{label ?? "Save"}</>
              )}
            </Button>
          </div>
        )}
      </form.Subscribe>
    </>
  );
}

export default SubmitButton;
