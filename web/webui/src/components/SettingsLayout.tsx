import ImportConfigButton from "@/features/ImportConfig/import-config-button";

interface SettingsLayoutProps {
  children: React.ReactNode;
}

export function SettingsLayout({ children }: SettingsLayoutProps) {
  return (
    <div className="pb-6">
      <div className="border-border/70 mb-6 flex flex-wrap items-center justify-between gap-3 border-b pb-4">
        <div>
          <h2 className="text-xl font-semibold tracking-tight">Edit config</h2>
          <p className="text-muted-foreground mt-0.5 text-sm">
            Changes are saved to the database and take effect immediately.
          </p>
        </div>
        <ImportConfigButton />
      </div>
      {children}
    </div>
  );
}
