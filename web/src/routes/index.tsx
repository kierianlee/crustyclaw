import { createFileRoute } from "@tanstack/react-router";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { createAtom } from "@tanstack/store";
import { useStore } from "@tanstack/react-store";
import { Area, AreaChart, Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts";
import { Card, CardPanel } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Separator } from "@/components/ui/separator";
import { Spinner } from "@/components/ui/spinner";
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart";
import type { RuntimeStatus } from "@/types/bindings";

// ---------------------------------------------------------------------------
// History store — accumulates polling snapshots client-side, persists across
// component unmount/remount on page navigation.
// ---------------------------------------------------------------------------

const MAX_POINTS = 60;

interface Snapshot {
  time: string;
  invocations: number;
  queue: number;
  rate: number;
}

const historyAtom = createAtom<Snapshot[]>([]);
let _prevInv: number | null = null;

function useStatusHistory(status: RuntimeStatus | undefined) {
  const history = useStore(historyAtom, (s: Snapshot[]) => s);
  const prevStatus = useRef(status);

  useEffect(() => {
    if (!status || status === prevStatus.current) return;
    prevStatus.current = status;

    const now = new Date();
    const time = now.toLocaleTimeString([], { minute: "2-digit", second: "2-digit" });
    const rate =
      _prevInv !== null
        ? Math.max(0, status.total_invocations - _prevInv)
        : 0;
    _prevInv = status.total_invocations;

    historyAtom.set((prev: Snapshot[]) => [
      ...prev.slice(-(MAX_POINTS - 1)),
      { time, invocations: status.total_invocations, queue: status.queue_depth, rate },
    ]);
  }, [status]);

  return history;
}

// ---------------------------------------------------------------------------
// Chart configs
// ---------------------------------------------------------------------------

const rateConfig = {
  rate: { label: "Invocations", color: "var(--chart-1)" },
} satisfies ChartConfig;

const queueConfig = {
  queue: { label: "Queue depth", color: "var(--chart-2)" },
} satisfies ChartConfig;

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

function StatusPage() {
  const {
    data: status,
    isLoading,
    error,
  } = useQuery<RuntimeStatus>({
    queryKey: ["status"],
    queryFn: () => fetch("/api/status").then((r) => r.json()),
    refetchInterval: 3000,
  });

  const history = useStatusHistory(status);

  if (isLoading)
    return (
      <div className="flex justify-center p-8">
        <Spinner />
      </div>
    );
  if (error) return <p className="text-destructive">Failed to load status</p>;
  if (!status) return null;

  const uptime = formatUptime(status.daemon_started_at);
  const isHealthy = !status.heartbeat_last_alert;
  const startedAt = new Date(status.daemon_started_at).toLocaleString();
  const recentRates = history.slice(-10).map((s) => s.rate);
  const avgRate =
    recentRates.length > 0
      ? recentRates.reduce((a, b) => a + b, 0) / recentRates.length
      : 0;
  const ratePerMin = (avgRate * (60 / 3)).toFixed(1);

  return (
    <div className="max-w-3xl space-y-3">
      {/* Hero banner */}
      <Card>
        <CardPanel className="p-0">
          <div className="flex items-center gap-4 px-5 py-4">
            <div
              className={`size-3 rounded-full shrink-0 ${isHealthy ? "bg-success shadow-[0_0_8px_var(--color-success)]" : "bg-warning shadow-[0_0_8px_var(--color-warning)]"}`}
            />
            <div className="flex-1 min-w-0">
              <div className="flex items-center gap-2">
                <span className="font-semibold text-sm">
                  {isHealthy ? "Healthy" : "Alert"}
                </span>
                <span className="text-muted-foreground text-xs">
                  up {uptime}
                </span>
              </div>
              <p className="text-muted-foreground text-xs truncate">
                Started {startedAt}
                {status.session_id_short && (
                  <>
                    {" "}
                    &middot; Session{" "}
                    <code className="text-[.625rem]">
                      {status.session_id_short}
                    </code>
                  </>
                )}
              </p>
            </div>
          </div>

          <Separator />

          {/* Stat counters */}
          <div className="grid grid-cols-4 divide-x divide-border">
            <Stat label="Invocations" value={status.total_invocations} />
            <Stat label="Queue" value={status.queue_depth} />
            <Stat label="Jobs" value={status.scheduler_job_count} />
            <Stat
              label="Rejections"
              value={status.queue_full_rejections ?? 0}
              muted={!status.queue_full_rejections}
            />
          </div>
        </CardPanel>
      </Card>

      {/* Charts */}
      <div className="grid grid-cols-2 gap-3">
        {/* Invocation rate */}
        <Card>
          <CardPanel className="p-0">
            <div className="flex items-baseline justify-between px-4 pt-3 pb-0">
              <div>
                <p className="text-xs text-muted-foreground">
                  Invocation rate
                </p>
                <p className="text-lg font-semibold tabular-nums">
                  {ratePerMin}
                  <span className="text-xs text-muted-foreground font-normal ml-0.5">
                    /min
                  </span>
                </p>
              </div>
              <Badge variant="outline" size="sm">
                live
              </Badge>
            </div>
            <ChartContainer config={rateConfig} className="h-30 w-full">
              <BarChart
                data={history}
                margin={{ top: 8, right: 8, left: 0, bottom: 0 }}
              >
                <CartesianGrid vertical={false} strokeDasharray="3 3" />
                <XAxis
                  dataKey="time"
                  tickLine={false}
                  axisLine={false}
                  tick={{ fontSize: 10 }}
                  interval="preserveStartEnd"
                  minTickGap={40}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <Bar
                  dataKey="rate"
                  fill="var(--color-rate)"
                  radius={[3, 3, 0, 0]}
                />
              </BarChart>
            </ChartContainer>
          </CardPanel>
        </Card>

        {/* Queue depth */}
        <Card>
          <CardPanel className="p-0">
            <div className="flex items-baseline justify-between px-4 pt-3 pb-0">
              <div>
                <p className="text-xs text-muted-foreground">Queue depth</p>
                <p className="text-lg font-semibold tabular-nums">
                  {status.queue_depth}
                </p>
              </div>
              {(status.queue_full_rejections ?? 0) > 0 && (
                <Badge variant="warning" size="sm">
                  {status.queue_full_rejections} rejected
                </Badge>
              )}
            </div>
            <ChartContainer config={queueConfig} className="h-30 w-full">
              <AreaChart
                data={history}
                margin={{ top: 8, right: 8, left: 0, bottom: 0 }}
              >
                <CartesianGrid vertical={false} strokeDasharray="3 3" />
                <XAxis
                  dataKey="time"
                  tickLine={false}
                  axisLine={false}
                  tick={{ fontSize: 10 }}
                  interval="preserveStartEnd"
                  minTickGap={40}
                />
                <YAxis
                  tickLine={false}
                  axisLine={false}
                  tick={{ fontSize: 10 }}
                  width={24}
                  allowDecimals={false}
                />
                <ChartTooltip content={<ChartTooltipContent />} />
                <defs>
                  <linearGradient id="queueGrad" x1="0" y1="0" x2="0" y2="1">
                    <stop
                      offset="5%"
                      stopColor="var(--color-queue)"
                      stopOpacity={0.3}
                    />
                    <stop
                      offset="95%"
                      stopColor="var(--color-queue)"
                      stopOpacity={0.05}
                    />
                  </linearGradient>
                </defs>
                <Area
                  type="monotone"
                  dataKey="queue"
                  stroke="var(--color-queue)"
                  fill="url(#queueGrad)"
                  strokeWidth={2}
                />
              </AreaChart>
            </ChartContainer>
          </CardPanel>
        </Card>
      </div>

      {/* Heartbeat + Jobs row */}
      <div className="grid grid-cols-2 gap-3">
        <Card>
          <CardPanel className="px-4 py-3">
            <div className="flex items-center justify-between">
              <div>
                <p className="text-sm font-semibold">Heartbeat</p>
                <p className="text-muted-foreground text-xs mt-0.5">
                  {status.heartbeat_enabled
                    ? status.heartbeat_last_ok
                      ? `Last OK ${new Date(status.heartbeat_last_ok).toLocaleTimeString()}`
                      : "Waiting for first check"
                    : "Disabled"}
                </p>
              </div>
              <Badge
                variant={
                  !status.heartbeat_enabled
                    ? "secondary"
                    : status.heartbeat_last_alert
                      ? "warning"
                      : "success"
                }
              >
                {!status.heartbeat_enabled
                  ? "Off"
                  : status.heartbeat_last_alert
                    ? "Alert"
                    : "OK"}
              </Badge>
            </div>
          </CardPanel>
        </Card>

        <Card>
          <CardPanel className="px-4 py-3">
            <div className="flex items-center justify-between">
              <div>
                <p className="text-sm font-semibold">Scheduler</p>
                <p className="text-muted-foreground text-xs mt-0.5">
                  {status.scheduler_job_count === 0
                    ? "No active jobs"
                    : `${status.scheduler_job_count} job${status.scheduler_job_count !== 1 ? "s" : ""} running`}
                </p>
              </div>
              <span className="text-2xl font-semibold tabular-nums">
                {status.scheduler_job_count}
              </span>
            </div>
          </CardPanel>
        </Card>
      </div>

      {/* Alerts */}
      {(status.last_error || status.heartbeat_last_alert) && (
        <Card>
          <CardPanel className="p-0">
            {status.last_error && (
              <div className="px-5 py-3 bg-destructive/4">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0 flex-1">
                    <p className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-1">
                      Last Error
                    </p>
                    <p className="text-destructive-foreground text-sm wrap-break-word">
                      {status.last_error.message}
                    </p>
                  </div>
                  <span className="text-muted-foreground text-xs shrink-0 pt-4">
                    {timeAgo(status.last_error.at)}
                  </span>
                </div>
              </div>
            )}
            {status.last_error && status.heartbeat_last_alert && <Separator />}
            {status.heartbeat_last_alert && (
              <div className="px-5 py-3 bg-warning/4">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0 flex-1">
                    <p className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-1">
                      Heartbeat Alert
                    </p>
                    <p className="text-warning-foreground text-sm wrap-break-word">
                      {status.heartbeat_last_alert.message}
                    </p>
                  </div>
                  <span className="text-muted-foreground text-xs shrink-0 pt-4">
                    {timeAgo(status.heartbeat_last_alert.at)}
                  </span>
                </div>
              </div>
            )}
          </CardPanel>
        </Card>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sub-components
// ---------------------------------------------------------------------------

function Stat({
  label,
  value,
  muted,
}: {
  label: string;
  value: number;
  muted?: boolean;
}) {
  return (
    <div className="px-4 py-3 text-center">
      <p
        className={`text-lg font-semibold tabular-nums ${muted ? "text-muted-foreground" : ""}`}
      >
        {value.toLocaleString()}
      </p>
      <p className="text-muted-foreground text-xs">{label}</p>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function timeAgo(dateStr: string): string {
  const secs = Math.floor((Date.now() - new Date(dateStr).getTime()) / 1000);
  if (secs < 60) return `${secs}s ago`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  return `${Math.floor(hrs / 24)}d ago`;
}

function formatUptime(startedAt: string): string {
  const elapsed = Date.now() - new Date(startedAt).getTime();
  const totalSecs = Math.floor(elapsed / 1000);
  const hours = Math.floor(totalSecs / 3600);
  const minutes = Math.floor((totalSecs % 3600) / 60);
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

export const Route = createFileRoute("/")({ component: StatusPage });
