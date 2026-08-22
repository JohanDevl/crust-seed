import { Badge } from "@/components/ui/badge";
import { Label } from "@/components/ui/label.tsx";
import { Separator } from "@/components/ui/separator.tsx";
import { Switch } from "@/components/ui/switch.tsx";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { formatRelativeTime } from "@/lib/time";
import { useTRPC } from "@/lib/trpc";
import { useSubscription } from "@trpc/tanstack-react-query";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";

interface LogEntry {
  timestamp: string;
  level: string;
  label?: string;
  message: string;
}

/**
 * Log levels get their own badge tone. Kept as a table so the filter chips and
 * the per-row badges cannot drift apart, and so every colour comes from a
 * theme token rather than a fixed palette class.
 */
const LEVEL_VARIANT: Record<
  string,
  "destructive" | "warning" | "info" | "success" | "muted"
> = {
  error: "destructive",
  warn: "warning",
  info: "info",
  verbose: "success",
  debug: "muted",
};

export function Logs() {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [isAtBottom, setIsAtBottom] = useState(true);
  const [levelFilters, setLevelFilters] = useState<Set<string>>(
    new Set(["error", "warn", "info", "verbose", "debug"]),
  );
  const [isReversed, setIsReversed] = useState(() => {
    if (typeof window === "undefined") {
      return false;
    }
    const storedValue = window.localStorage.getItem("logs:newest-first");
    return storedValue === "true";
  });
  const [labelFilters, setLabelFilters] = useState<Set<string>>(new Set());
  const trpc = useTRPC();
  const tableRef = useRef<HTMLTableElement>(null);
  useSubscription(
    trpc.logs.subscribe.subscriptionOptions(
      { limit: 100 },
      {
        enabled: true,
        onData: (newLog) => {
          setLogs((prev = []) => {
            const updated = [...prev, newLog];
            // Keep only last 500 logs to prevent memory issues
            return updated.slice(-500);
          });
        },
        onError: (err) => {
          console.error("Log subscription error:", err);
        },
      },
    ),
  );

  // Detect if user is at bottom of page
  useEffect(() => {
    const handleScroll = () => {
      const position = window.innerHeight + window.scrollY;
      const height = document.documentElement.scrollHeight;
      const atBottom = position >= height && window.scrollY > 0;
      setIsAtBottom(atBottom);
    };

    window.addEventListener("scroll", handleScroll);
    handleScroll(); // Check initial state

    return () => window.removeEventListener("scroll", handleScroll);
  }, []);

  // Auto-scroll to bottom when new logs arrive and user is at bottom
  useLayoutEffect(() => {
    if (isAtBottom && logs.length > 0) {
      window.scrollTo({
        top: document.documentElement.scrollHeight,
        behavior: "instant",
      });
    }
  }, [logs, isAtBottom]);

  useEffect(() => {
    window.localStorage.setItem("logs:newest-first", String(isReversed));
  }, [isReversed]);

  // Get unique labels from logs
  const uniqueLabels = useMemo(() => {
    const labels = new Set<string>();
    logs.forEach((log) => {
      if (log.label) {
        labels.add(log.label);
      }
    });
    return Array.from(labels).sort();
  }, [logs]);

  // Initialize label filters when new labels appear
  useEffect(() => {
    setLabelFilters((prev) => {
      const newFilters = new Set(prev);
      uniqueLabels.forEach((label) => newFilters.add(label));
      return newFilters;
    });
  }, [uniqueLabels]);

  // Filter logs based on level and label filters
  const filteredLogs = useMemo(() => {
    const ret = logs.filter((log) => {
      const levelMatch = levelFilters.has(log.level);
      const labelMatch = !log.label || labelFilters.has(log.label);
      return levelMatch && labelMatch;
    });
    if (isReversed) {
      ret.reverse();
    }
    return ret;
  }, [logs, isReversed, levelFilters, labelFilters]);

  return (
    <div className="w-full space-y-6">
      <div className="flex flex-wrap items-stretch justify-between gap-4">
        <div className="flex flex-wrap items-center gap-2">
          {(["error", "warn", "info", "verbose", "debug"] as const).map(
            (level) => (
              <Badge
                key={level}
                variant={
                  levelFilters.has(level) ? LEVEL_VARIANT[level] : "outline"
                }
                className={cn(
                  "cursor-pointer px-3 py-1 select-none",
                  levelFilters.has(level)
                    ? "hover:brightness-95"
                    : "text-muted-foreground/60 border-border/60 border-dashed line-through hover:no-underline",
                )}
                onClick={() => {
                  setLevelFilters((prev) => {
                    const newFilters = new Set(prev);
                    if (newFilters.has(level)) {
                      newFilters.delete(level);
                    } else {
                      newFilters.add(level);
                    }
                    return newFilters;
                  });
                }}
              >
                {level}
              </Badge>
            ),
          )}
        </div>
        <div>
          <Separator orientation="vertical" className="h-6" />
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {uniqueLabels.map((label) => (
            <Badge
              key={label}
              variant={labelFilters.has(label) ? "default" : "outline"}
              className="hover:bg-muted cursor-pointer font-mono text-xs select-none"
              onClick={() => {
                setLabelFilters((prev) => {
                  const newFilters = new Set(prev);
                  if (newFilters.has(label)) {
                    newFilters.delete(label);
                  } else {
                    newFilters.add(label);
                  }
                  return newFilters;
                });
              }}
            >
              {label}
            </Badge>
          ))}
        </div>
        <Label className="ml-auto">
          <span className="text-sm font-medium">Show newest first</span>
          <Switch checked={isReversed} onCheckedChange={setIsReversed} />
        </Label>
      </div>

      {filteredLogs.length > 0 ? (
        <div className="bg-card ring-border/70 overflow-hidden rounded-2xl shadow-sm ring-1">
          <Table ref={tableRef}>
            <TableHeader className="bg-muted/95 sticky top-0 z-10 backdrop-blur">
              <TableRow>
                <TableHead className="w-32">Time</TableHead>
                <TableHead className="w-24">Level</TableHead>
                <TableHead className="w-32">Label</TableHead>
                <TableHead>Message</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filteredLogs.map((log, index) => (
                <TableRow
                  key={`${log.timestamp}-${index}`}
                  className="hover:bg-accent/40 h-8"
                >
                  <TableCell
                    className="font-mono text-xs"
                    title={new Date(log.timestamp).toLocaleString()}
                  >
                    {formatRelativeTime(log.timestamp)}
                  </TableCell>
                  <TableCell className="py-1">
                    <Badge
                      variant={LEVEL_VARIANT[log.level] ?? "muted"}
                      className="w-16"
                    >
                      {log.level}
                    </Badge>
                  </TableCell>
                  <TableCell className="py-1 font-medium">
                    {log.label && (
                      <Badge
                        variant="outline"
                        className="py-1 font-mono text-xs"
                      >
                        {log.label}
                      </Badge>
                    )}
                  </TableCell>
                  <TableCell className="py-1 font-mono text-xs">
                    <div className="whitespace-pre-wrap" title={log.message}>
                      {log.message}
                    </div>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      ) : (
        <div className="text-muted-foreground items-center justify-center py-16">
          <div className="mb-2 flex items-center gap-2">
            <div className="bg-success size-2 animate-pulse rounded-full"></div>
            <span className="text-sm font-medium">
              {logs.length === 0
                ? "Waiting for logs..."
                : "No logs match current filters"}
            </span>
          </div>
          {logs.length === 0 && (
            <p className="text-xs">Real-time streaming from server logs</p>
          )}
        </div>
      )}
    </div>
  );
}
