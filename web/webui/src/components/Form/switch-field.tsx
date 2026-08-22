import { FC } from "react";
import { cn } from "@/lib/utils";
import { useFieldContext } from "@/contexts/Form/form-context";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";

type SwitchFieldProps = {
  className?: string;
  label: string;
};

const SwitchField: FC<SwitchFieldProps> = ({ className, label }) => {
  const field = useFieldContext();
  return (
    <div
      className={cn("form-field__switch flex items-center gap-3", className)}
    >
      <Label
        htmlFor={field.name}
        className="text-foreground/90 text-sm font-medium"
      >
        {label}
      </Label>
      <Switch
        id={field.name}
        checked={field.state.value === true}
        onCheckedChange={field.handleChange}
      />
    </div>
  );
};

export default SwitchField;
