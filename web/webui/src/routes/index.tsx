import { createFileRoute } from "@tanstack/react-router";
import { useMemo } from "react";
import { useTRPC } from "@/lib/trpc";
import { useSuspenseQuery } from "@tanstack/react-query";
import {
  Activity,
  CircleCheck,
  Crosshair,
  Database,
  Download,
  Gauge,
  Percent,
  Radar,
  Search,
  TriangleAlert,
} from "lucide-react";
import { StatCard } from "@/components/ui/stat-card";
import { Page } from "@/components/Page";

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-3">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <h2 className="eyebrow">{title}</h2>
        <span className="text-muted-foreground/80 text-xs">{hint}</span>
      </div>
      {children}
    </section>
  );
}

function Home() {
  const trpc = useTRPC();
  const { data: statsData } = useSuspenseQuery(
    trpc.stats.getOverview.queryOptions(),
  );
  const showDistinctQueryCount = useMemo(
    () => statsData.queryCount !== statsData.totalSearchees,
    [statsData.queryCount, statsData.totalSearchees],
  );
  const showMatchesPerQuery = showDistinctQueryCount;
  const topGridCols = showDistinctQueryCount
    ? "lg:grid-cols-5"
    : "lg:grid-cols-4";
  const conversionGridCols = showMatchesPerQuery
    ? "lg:grid-cols-4"
    : "lg:grid-cols-3";
  const indexerHealthTitle = statsData.allIndexersHealthy
    ? "Indexer Health"
    : "Unhealthy Indexers";
  const indexerHealthValue = statsData.allIndexersHealthy
    ? `All ${statsData.totalIndexers.toLocaleString()}`
    : statsData.unhealthyIndexers.toLocaleString();
  const indexerHealthDescription = statsData.allIndexersHealthy
    ? "Indexers are all reporting healthy"
    : `${statsData.unhealthyIndexers.toLocaleString()} of ${statsData.totalIndexers.toLocaleString()} indexers need attention`;

  return (
    <Page breadcrumbs={["Dashboard"]}>
      <Section
        title="Coverage"
        hint="What crust-seed is watching and asking for"
      >
        <div className={`grid gap-4 sm:grid-cols-2 ${topGridCols}`}>
          <StatCard
            title="Total Searchees"
            value={statsData.totalSearchees.toLocaleString()}
            description="Torrents being monitored"
            icon={<Database />}
            tone="primary"
          />
          {showDistinctQueryCount && (
            <StatCard
              title="Total Search Queries"
              value={statsData.queryCount.toLocaleString()}
              description="Distinct estimated searches"
              icon={<Search />}
            />
          )}
          <StatCard
            title="Total Query-Indexer Pairs"
            value={statsData.queryIndexerCount.toLocaleString()}
            description="Unique indexer searches"
            icon={<Radar />}
          />
          <StatCard
            title="Total Snatches"
            value={statsData.snatchCount.toLocaleString()}
            description="Unique infohash attempts"
            icon={<Download />}
          />
          <StatCard
            title="Total Matches"
            value={statsData.totalMatches.toLocaleString()}
            description="Unique cross-seeds found"
            icon={<CircleCheck />}
            tone="success"
          />
        </div>
      </Section>

      <Section
        title="Conversion"
        hint="How much of that effort turns into matches"
      >
        <div className={`grid gap-4 sm:grid-cols-2 ${conversionGridCols}`}>
          <StatCard
            title="Matches per Searchee"
            value={statsData.matchRate.toFixed(2)}
            description="Average matches per monitored torrent"
            icon={<Gauge />}
          />
          {showMatchesPerQuery && (
            <StatCard
              title="Matches per Query"
              value={statsData.matchesPerQuery.toFixed(2)}
              description="Matches per search estimate"
              icon={<Crosshair />}
            />
          )}
          <StatCard
            title="Match Rate"
            value={`${(statsData.matchesPerQueryIndexer * 100).toFixed(1)}%`}
            description="of indexer searches find a match"
            icon={<Percent />}
          />
          <StatCard
            title="Wasted Snatches"
            value={`${(statsData.wastedSnatchRate * 100).toFixed(1)}%`}
            description={`${statsData.wastedSnatchCount.toLocaleString()} snatched but mismatched`}
            icon={<TriangleAlert />}
            tone={statsData.wastedSnatchRate > 0 ? "warning" : "default"}
          />
        </div>
      </Section>

      <Section title="Status" hint="Right now">
        <div className="grid gap-4 sm:grid-cols-2">
          <StatCard
            title={indexerHealthTitle}
            value={indexerHealthValue}
            description={indexerHealthDescription}
            icon={
              statsData.allIndexersHealthy ? <CircleCheck /> : <TriangleAlert />
            }
            tone={statsData.allIndexersHealthy ? "success" : "warning"}
          />
          <StatCard
            title="Recent Activity"
            value={statsData.recentMatches.toLocaleString()}
            description="Matches in last 24h"
            icon={<Activity />}
            tone="primary"
          />
        </div>
      </Section>
    </Page>
  );
}

export const Route = createFileRoute("/")({
  component: Home,
});
