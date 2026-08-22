import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Sheet,
  SheetClose,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetFooter,
  SheetTitle,
} from "@/components/ui/sheet";
import { Pencil } from "lucide-react";
import { TDownloadClient } from "@/types/download-clients";

interface ClientViewSheetProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  client: TDownloadClient | null;
  onEdit?: (client: TDownloadClient) => void;
}

export default function ClientViewSheet({
  open,
  onOpenChange,
  client,
  onEdit,
}: ClientViewSheetProps) {
  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="sm:max-w-md">
        <SheetHeader>
          <SheetTitle>View Torrent Client</SheetTitle>
          <SheetDescription>View torrent client details.</SheetDescription>
        </SheetHeader>

        <div className="grid flex-1 auto-rows-min gap-6 px-4">
          <div className="grid gap-3">
            <Label>URL</Label>
            <div className="bg-muted/60 border-border/70 rounded-lg border px-3 py-2 font-mono text-sm break-all">
              {client?.url || "N/A"}
            </div>
          </div>

          {client?.useApiKey ? (
            <div className="grid gap-3">
              <Label>API Key</Label>
              <div className="bg-muted/60 border-border/70 rounded-lg border px-3 py-2 font-mono text-sm break-all">
                {client?.apiKey ? "********" : "N/A"}
              </div>
            </div>
          ) : (
            <>
              <div className="grid gap-3">
                <Label>Username</Label>
                <div className="bg-muted/60 border-border/70 rounded-lg border px-3 py-2 font-mono text-sm break-all">
                  {client?.user || "N/A"}
                </div>
              </div>

              <div className="grid gap-3">
                <Label>Password</Label>
                <div className="bg-muted/60 border-border/70 rounded-lg border px-3 py-2 font-mono text-sm break-all">
                  {client?.password ? "********" : "N/A"}
                </div>
              </div>
            </>
          )}

          <div className="grid gap-3">
            <Label>Read only</Label>
            <div className="bg-muted/60 border-border/70 rounded-lg border px-3 py-2 font-mono text-sm break-all">
              {client?.readOnly ? "Yes" : "No"}
            </div>
          </div>
        </div>

        <SheetFooter>
          {onEdit && client && (
            <Button
              type="button"
              onClick={() => onEdit(client)}
              className="flex-1"
            >
              <Pencil className="mr-2 h-4 w-4" />
              Edit
            </Button>
          )}
          <SheetClose asChild className="flex-1">
            <Button type="button" variant="outline" className="flex-1">
              Close
            </Button>
          </SheetClose>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
