import { useMemo, Fragment, type ReactNode } from "react";
import { useLocation } from "@tanstack/react-router";
import { ModeToggle } from "@/components/ModeToggle/ModeToggle";
import { SidebarTrigger } from "@/components/ui/sidebar";
import {
  Breadcrumb,
  BreadcrumbList,
  BreadcrumbItem,
  BreadcrumbSeparator,
} from "@/components/ui/breadcrumb";

interface PageProps {
  breadcrumbs?: string[];
  actions?: ReactNode;
  children: ReactNode;
}

export function Page({ breadcrumbs, actions, children }: PageProps) {
  const location = useLocation();

  // Auto-generate breadcrumbs from URL if not provided
  const autoBreadcrumbs = useMemo(() => {
    if (breadcrumbs) return breadcrumbs;

    const segments = location.pathname.split("/").filter(Boolean);
    return segments.map(
      (segment) => segment.charAt(0).toUpperCase() + segment.slice(1),
    );
  }, [location.pathname, breadcrumbs]);

  // The last crumb becomes the page title; anything above it stays a trail.
  const title = autoBreadcrumbs[autoBreadcrumbs.length - 1] ?? "Dashboard";
  const trail = autoBreadcrumbs.slice(0, -1);

  return (
    <>
      <header className="bg-background/85 supports-[backdrop-filter]:bg-background/65 border-border/70 sticky top-0 z-20 flex shrink-0 items-center gap-3 border-b px-4 py-3 backdrop-blur-md sm:px-6">
        <SidebarTrigger className="text-muted-foreground hover:text-foreground -ml-1 size-8 rounded-lg" />

        <div className="min-w-0 flex-1">
          {trail.length > 0 && (
            <Breadcrumb>
              <BreadcrumbList className="text-muted-foreground gap-1 text-[0.7rem] sm:gap-1.5">
                {trail.map((crumb, i) => (
                  <Fragment key={i}>
                    <BreadcrumbItem>{crumb}</BreadcrumbItem>
                    {i < trail.length - 1 && <BreadcrumbSeparator />}
                  </Fragment>
                ))}
              </BreadcrumbList>
            </Breadcrumb>
          )}
          <h1 className="truncate text-lg leading-tight font-semibold">
            {title}
          </h1>
        </div>

        <div className="flex shrink-0 items-center gap-2">
          {actions}
          <ModeToggle className="text-muted-foreground hover:text-foreground size-8 rounded-lg" />
        </div>
      </header>

      <div className="flex flex-1 flex-col">
        <div className="@container/main mx-auto w-full max-w-[1400px] flex-1">
          <div className="flex flex-col gap-6 px-4 py-6 sm:px-6">
            {children}
          </div>
        </div>
      </div>
    </>
  );
}
