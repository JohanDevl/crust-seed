interface SettingsLayoutProps {
  children: React.ReactNode;
}

export function SettingsLayout({ children }: SettingsLayoutProps) {
  return (
    <div className="pb-6">
      <div className="border-border/70 mb-6 border-b pb-4">
        <h2 className="text-xl font-semibold tracking-tight">Edit config</h2>
      </div>
      {children}
    </div>
  );
}
