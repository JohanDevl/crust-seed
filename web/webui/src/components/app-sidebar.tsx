import * as React from "react";
import { Link } from "@tanstack/react-router";
import {
  useMutation,
  useQuery,
  useSuspenseQuery,
  useQueryClient,
} from "@tanstack/react-query";
import {
  LogOut,
  Home,
  Settings,
  Search,
  FileText,
  Clock,
  Download,
  Folders,
  Webhook,
  Popcorn,
  Library,
  AlertTriangle,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { LogoMark } from "@/components/brand/LogoMark";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
} from "@/components/ui/sidebar";
import { useTRPC } from "@/lib/trpc";
import { cn } from "@/lib/utils";

const navItems = [
  {
    title: "Overview",
    items: [
      {
        title: "Dashboard",
        icon: <Home className="size-4" />,
        url: "/",
      },
      {
        title: "Library",
        icon: <Library className="size-4" />,
        url: "/library",
      },
      {
        title: "Jobs",
        icon: <Clock className="size-4" />,
        url: "/jobs",
      },
    ],
  },
  {
    title: "Settings",
    items: [
      {
        title: "General",
        icon: <Settings className="size-4" />,
        url: "/settings/general",
      },
      {
        title: "Trackers",
        icon: <Popcorn className="size-4" />,
        url: "/settings/trackers",
      },
      {
        title: "Torrent Clients",
        icon: <Download className="size-4" />,
        url: "/settings/clients",
      },
      {
        title: "Search & RSS",
        icon: <Search className="size-4" />,
        url: "/settings/search",
      },
      {
        title: "Connect",
        icon: <Webhook className="size-4" />,
        url: "/settings/connect",
      },
      {
        title: "Directories",
        icon: <Folders className="size-4" />,
        url: "/settings/directories",
      },
    ],
  },
  {
    title: "Diagnostics",
    items: [
      {
        title: "Health",
        icon: <AlertTriangle className="size-4" />,
        url: "/settings/health",
      },
      {
        title: "Logs",
        icon: <FileText className="size-4" />,
        url: "/logs",
      },
    ],
  },
];

export function AppSidebar({ ...props }: React.ComponentProps<typeof Sidebar>) {
  const queryClient = useQueryClient();
  const trpc = useTRPC();
  const { data: authStatus } = useSuspenseQuery(
    trpc.auth.authStatus.queryOptions(),
  );
  const { data: buildInfoResponse } = useSuspenseQuery(
    trpc.meta.getBuildInfo.queryOptions(),
  );
  const { data: healthData } = useQuery({
    ...trpc.health.get.queryOptions(),
    refetchInterval: 60_000,
  });
  const healthStatus = healthData?.problems.some(
    (problem) => problem.severity === "error",
  )
    ? "error"
    : healthData?.problems.some((problem) => problem.severity === "warning")
      ? "warning"
      : undefined;
  const buildInfo = buildInfoResponse?.build;
  const buildVersion = buildInfoResponse?.version;
  const buildTag = buildInfo?.tag ?? buildVersion;
  const buildBranch = buildInfo?.branch ?? undefined;
  const shortSha = buildInfo?.commitSha?.slice(0, 7);
  const buildLine = [buildTag, shortSha, buildBranch]
    .filter(Boolean)
    .join(" · ");
  const commitMessage = buildInfo?.message?.split("\n")[0]?.trim();
  const buildDate = (() => {
    if (!buildInfo?.date) return undefined;
    const parsed = new Date(buildInfo.date);
    if (Number.isNaN(parsed.getTime())) return buildInfo.date;
    return parsed.toLocaleDateString(undefined, {
      year: "numeric",
      month: "short",
      day: "2-digit",
    });
  })();
  const normalizedTag = buildInfo?.tag?.replace(/^v/i, "") ?? "";
  const isVersionTrackingTag =
    normalizedTag === "latest" ||
    /^\d+(\.\d+){0,2}([-.][0-9A-Za-z.]+)?$/.test(normalizedTag);
  const formatVersion = (version?: string) => {
    if (!version) return undefined;
    return version.startsWith("v") ? version : `v${version}`;
  };
  const preferCommitInfo =
    authStatus?.isDocker && buildInfo?.tag && !isVersionTrackingTag;
  const commitLine = [shortSha, buildBranch, buildDate]
    .filter(Boolean)
    .join(" · ");
  const hasCommitInfo = Boolean(commitLine || commitMessage);
  const isSourceBuild =
    !authStatus?.isDocker && !buildInfo?.tag && hasCommitInfo;
  const isPublishedNpm =
    !authStatus?.isDocker && !buildInfo?.tag && !hasCommitInfo;
  const versionLabel = formatVersion(buildVersion);
  const primaryLine = preferCommitInfo
    ? commitLine || buildLine || versionLabel || ""
    : isSourceBuild
      ? commitLine || versionLabel || ""
      : isPublishedNpm && versionLabel
        ? `${versionLabel} (npm)`
        : buildLine || versionLabel || "";
  const secondaryLine = preferCommitInfo
    ? (versionLabel ?? "")
    : isSourceBuild
      ? (versionLabel ?? commitMessage ?? "")
      : (commitMessage ?? "");
  const hasBuildInfo = Boolean(primaryLine || secondaryLine);

  const { mutate: logout } = useMutation(
    trpc.auth.logOut.mutationOptions({
      onSuccess: async () => {
        await queryClient.invalidateQueries({
          queryKey: trpc.auth.authStatus.queryKey(),
        });
      },
    }),
  );

  return (
    <Sidebar variant="inset" {...props}>
      <SidebarHeader className="px-3 pt-3 pb-1">
        <div className="flex items-center gap-2.5">
          <span className="bg-card ring-sidebar-border/80 flex size-9 items-center justify-center rounded-xl shadow-xs ring-1">
            <LogoMark className="size-5" />
          </span>
          <span className="flex min-w-0 flex-col leading-none">
            <span className="text-[0.95rem] font-semibold tracking-tight">
              crust<span className="text-primary">-seed</span>
            </span>
            {hasBuildInfo && primaryLine && (
              <span
                className="text-muted-foreground mt-1 truncate text-[0.68rem]"
                title={
                  preferCommitInfo || isSourceBuild ? commitMessage : undefined
                }
              >
                {primaryLine}
              </span>
            )}
          </span>
        </div>
        {hasBuildInfo && secondaryLine && (
          <div
            className={cn(
              "text-muted-foreground/80 mt-1.5 truncate pl-0.5 text-[0.68rem]",
              preferCommitInfo && "opacity-70",
            )}
            title={!preferCommitInfo ? commitMessage : undefined}
          >
            {secondaryLine}
          </div>
        )}
      </SidebarHeader>

      <SidebarContent className="px-1.5">
        {navItems.map((section) => (
          <SidebarGroup key={section.title} className="py-1">
            <SidebarGroupLabel className="eyebrow h-6 px-2">
              {section.title}
            </SidebarGroupLabel>
            <SidebarGroupContent>
              <SidebarMenu className="gap-0.5">
                {section.items.map((item) => (
                  <SidebarMenuItem key={item.title}>
                    <SidebarMenuButton
                      asChild
                      tooltip={item.title}
                      className={cn(
                        "h-9 rounded-lg px-2.5 font-medium",
                        "[&>svg]:text-muted-foreground [&>svg]:transition-colors",
                        "hover:[&>svg]:text-foreground",
                        "data-[active=true]:[&>svg]:text-primary data-[active=true]:shadow-xs",
                      )}
                    >
                      <Link
                        to={item.url}
                        activeProps={{
                          "data-active": true,
                        }}
                        activeOptions={{ exact: true }}
                      >
                        {item.icon}
                        <span>{item.title}</span>
                        {item.title === "Health" && healthStatus && (
                          <span
                            className={cn(
                              "ml-auto size-2 rounded-full ring-2",
                              healthStatus === "error"
                                ? "bg-destructive ring-destructive/25"
                                : "bg-warning ring-warning/25",
                            )}
                            aria-label={`Health has ${healthStatus}s`}
                          />
                        )}
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ))}
      </SidebarContent>

      <SidebarFooter className="p-2">
        {authStatus?.isLoggedIn && (
          <div className="bg-sidebar-accent/40 ring-sidebar-border/70 flex items-center gap-2.5 rounded-xl p-2 ring-1">
            <span className="bg-primary text-primary-foreground flex size-7 shrink-0 items-center justify-center rounded-full text-xs font-semibold uppercase">
              {authStatus.user?.username?.charAt(0) ?? "?"}
            </span>
            <div className="min-w-0 flex-1 truncate text-sm font-medium">
              {authStatus.user?.username}
            </div>
            <Button
              variant="ghost"
              size="icon"
              className="text-muted-foreground hover:text-foreground size-7 rounded-lg"
              onClick={() => logout()}
              title="Logout"
            >
              <LogOut className="size-4" />
            </Button>
          </div>
        )}
      </SidebarFooter>

      <SidebarRail />
    </Sidebar>
  );
}
