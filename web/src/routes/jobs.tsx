import { createFileRoute } from "@tanstack/react-router";
import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Trash2, Plus, Pencil } from "lucide-react";
import {
  Table,
  TableHeader,
  TableBody,
  TableRow,
  TableHead,
  TableCell,
} from "@/components/ui/table";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Spinner } from "@/components/ui/spinner";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import {
  Dialog,
  DialogPopup,
  DialogHeader,
  DialogFooter,
  DialogPanel,
  DialogTitle,
  DialogDescription,
  DialogClose,
  DialogTrigger,
} from "@/components/ui/dialog";
import { Card, CardPanel } from "@/components/ui/card";
import type { JobRecord } from "@/types/bindings";

function JobsPage() {
  const queryClient = useQueryClient();
  const { data: jobs, isLoading } = useQuery<JobRecord[]>({
    queryKey: ["jobs"],
    queryFn: () => fetch("/api/jobs").then((r) => r.json()),
    refetchInterval: 5000,
  });

  const deleteMutation = useMutation({
    mutationFn: (id: string) =>
      fetch(`/api/jobs/${id}`, { method: "DELETE" }).then((r) => {
        if (!r.ok) throw new Error("Delete failed");
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["jobs"] }),
  });

  if (isLoading)
    return (
      <div className="flex justify-center p-8">
        <Spinner />
      </div>
    );

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Scheduled Jobs</h2>
        <CreateJobDialog />
      </div>

      {!jobs || jobs.length === 0 ? (
        <p className="text-muted-foreground text-sm">No jobs configured.</p>
      ) : (
        <Card>
          <CardPanel className="p-0">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Schedule</TableHead>
                  <TableHead>Action</TableHead>
                  <TableHead className="w-16" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {jobs.map((job) => (
                  <TableRow key={job.stable_id}>
                    <TableCell className="font-medium">
                      {job.name}
                      {job.one_shot && (
                        <Badge variant="secondary" size="sm" className="ml-2">
                          one-shot
                        </Badge>
                      )}
                    </TableCell>
                    <TableCell>
                      <code className="text-xs">{job.cron_expression}</code>
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline" size="sm">
                        {job.action.type.replace("_", " ")}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <EditJobDialog job={job} />
                        <Button
                          variant="ghost"
                          size="icon-xs"
                          onClick={() => deleteMutation.mutate(job.stable_id)}
                          disabled={deleteMutation.isPending}
                        >
                          <Trash2 />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </CardPanel>
        </Card>
      )}
    </div>
  );
}

type Frequency = "minutes" | "hours" | "daily" | "weekly";
const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"] as const;

function buildCron(
  freq: Frequency,
  every: number,
  hour: number,
  minute: number,
  days: Set<string>,
): string {
  switch (freq) {
    case "minutes":
      return `0 */${every} * * * *`;
    case "hours":
      return `0 ${minute} */${every} * * *`;
    case "daily":
      return every === 1
        ? `0 ${minute} ${hour} * * *`
        : `0 ${minute} ${hour} */${every} * *`;
    case "weekly": {
      const sel = DAYS.filter((d) => days.has(d));
      return `0 ${minute} ${hour} * * ${sel.length > 0 ? sel.join(",") : "*"}`;
    }
  }
}

function describeCron(
  freq: Frequency,
  every: number,
  hour: number,
  minute: number,
  days: Set<string>,
): string {
  const time = `${hour.toString().padStart(2, "0")}:${minute.toString().padStart(2, "0")}`;
  switch (freq) {
    case "minutes":
      return `Every ${every} minute${every > 1 ? "s" : ""}`;
    case "hours":
      return `Every ${every} hour${every > 1 ? "s" : ""} at :${minute.toString().padStart(2, "0")}`;
    case "daily":
      return every === 1
        ? `Daily at ${time}`
        : `Every ${every} days at ${time}`;
    case "weekly": {
      const sel = DAYS.filter((d) => days.has(d));
      return sel.length === 0
        ? `Weekly at ${time} (no days)`
        : `${sel.join(", ")} at ${time}`;
    }
  }
}

const selectCls = "rounded-lg border bg-background px-2 py-1.5 text-sm";
const numCls =
  "w-16 rounded-lg border bg-background px-2 py-1.5 text-sm tabular-nums";

const everyRange: Record<Frequency, { min: number; max: number }> = {
  minutes: { min: 1, max: 59 },
  hours: { min: 1, max: 23 },
  daily: { min: 1, max: 30 },
  weekly: { min: 1, max: 1 },
};

function clampEvery(freq: Frequency, val: number): number {
  const { min, max } = everyRange[freq];
  return Math.max(min, Math.min(max, Math.round(val) || min));
}

function ScheduleBuilder({ onChange }: { onChange: (cron: string) => void }) {
  const [freq, setFreq] = useState<Frequency>("hours");
  const [every, setEvery] = useState(6);
  const [hour, setHour] = useState(9);
  const [minute, setMinute] = useState(0);
  const [days, setDays] = useState<Set<string>>(() => new Set(["Mon"]));

  const emit = (
    f: Frequency,
    e: number,
    h: number,
    m: number,
    d: Set<string>,
  ) => {
    onChange(buildCron(f, e, h, m, d));
  };

  useState(() => {
    emit(freq, every, hour, minute, days);
  });

  const changeFreq = (f: Frequency) => {
    setFreq(f);
    const e = f === "minutes" ? 15 : f === "hours" ? 6 : 1;
    setEvery(e);
    emit(f, e, hour, minute, days);
  };

  const changeEvery = (raw: string) => {
    const parsed = parseInt(raw, 10);
    if (isNaN(parsed)) return;
    const v = clampEvery(freq, parsed);
    setEvery(v);
    emit(freq, v, hour, minute, days);
  };

  const toggleDay = (day: string) => {
    const next = new Set(days);
    if (next.has(day)) next.delete(day);
    else next.add(day);
    setDays(next);
    emit(freq, every, hour, minute, next);
  };

  return (
    <div className="space-y-2.5 rounded-lg border bg-muted/40 p-3">
      {/* Frequency + interval */}
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="text-muted-foreground">Every</span>
        {freq !== "weekly" && (
          <input
            type="number"
            className={numCls}
            value={every}
            min={everyRange[freq].min}
            max={everyRange[freq].max}
            onChange={(e) => changeEvery(e.target.value)}
            onBlur={() => setEvery(clampEvery(freq, every))}
          />
        )}
        <select
          className={selectCls}
          value={freq}
          onChange={(e) => changeFreq(e.target.value as Frequency)}
        >
          <option value="minutes">minutes</option>
          <option value="hours">hours</option>
          <option value="daily">days</option>
          <option value="weekly">week</option>
        </select>
      </div>

      {/* Time picker (hours/daily/weekly) */}
      {freq !== "minutes" && (
        <div className="flex items-center gap-2 text-sm">
          <span className="text-muted-foreground">
            {freq === "hours" ? "At minute" : "At"}
          </span>
          {freq !== "hours" && (
            <>
              <input
                type="number"
                className={numCls}
                value={hour}
                min={0}
                max={23}
                onChange={(e) => {
                  const v = Math.max(
                    0,
                    Math.min(23, Number(e.target.value) || 0),
                  );
                  setHour(v);
                  emit(freq, every, v, minute, days);
                }}
              />
              <span className="text-muted-foreground">:</span>
            </>
          )}
          <input
            type="number"
            className={numCls}
            value={minute}
            min={0}
            max={59}
            onChange={(e) => {
              const v = Math.max(0, Math.min(59, Number(e.target.value) || 0));
              setMinute(v);
              emit(freq, every, hour, v, days);
            }}
          />
        </div>
      )}

      {/* Day-of-week (weekly) */}
      {freq === "weekly" && (
        <div className="flex items-center gap-1.5">
          <span className="text-sm text-muted-foreground mr-0.5">On</span>
          {DAYS.map((day) => (
            <button
              key={day}
              type="button"
              onClick={() => toggleDay(day)}
              className={`rounded-md px-2 py-1 text-xs font-medium transition-colors ${
                days.has(day)
                  ? "bg-primary text-primary-foreground"
                  : "bg-background text-muted-foreground hover:bg-accent"
              }`}
            >
              {day.slice(0, 2)}
            </button>
          ))}
        </div>
      )}

      {/* Summary */}
      <p className="text-xs text-muted-foreground pt-0.5">
        {describeCron(freq, every, hour, minute, days)}
      </p>
    </div>
  );
}

type ActionType = "claude_prompt" | "telegram_message";

function ActionFields({
  actionType,
  onActionTypeChange,
  body,
  onBodyChange,
}: {
  actionType: ActionType;
  onActionTypeChange: (t: ActionType) => void;
  body: string;
  onBodyChange: (b: string) => void;
}) {
  return (
    <>
      <select
        className="w-full rounded-lg border bg-background px-3 py-2 text-sm"
        value={actionType}
        onChange={(e) => onActionTypeChange(e.target.value as ActionType)}
      >
        <option value="claude_prompt">Claude Prompt</option>
        <option value="telegram_message">Send Message</option>
      </select>
      <Textarea
        placeholder={
          actionType === "claude_prompt"
            ? "What should Claude do?"
            : "Message text"
        }
        value={body}
        onChange={(e: React.ChangeEvent<HTMLTextAreaElement>) =>
          onBodyChange(e.target.value)
        }
        rows={3}
      />
    </>
  );
}

function buildPayload(actionType: ActionType, body: string, cron: string, name?: string) {
  const payload: Record<string, unknown> = {
    cron_expression: cron,
    type: actionType,
  };
  if (name !== undefined) payload.name = name;
  if (actionType === "claude_prompt") payload.prompt = body;
  else payload.text = body;
  return payload;
}

function EditJobDialog({ job }: { job: JobRecord }) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const [useBuilder, setUseBuilder] = useState(false);
  const [cron, setCron] = useState(job.cron_expression);
  const [actionType, setActionType] = useState<ActionType>(
    job.action.type as ActionType,
  );
  const [body, setBody] = useState(
    "prompt" in job.action ? job.action.prompt : "text" in job.action ? job.action.text : "",
  );

  const mutation = useMutation({
    mutationFn: async () => {
      const res = await fetch(`/api/jobs/${job.stable_id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(buildPayload(actionType, body, cron)),
      });
      if (!res.ok) throw new Error(await res.text());
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      setOpen(false);
    },
  });

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger
        render={
          <Button variant="ghost" size="icon-xs">
            <Pencil />
          </Button>
        }
      />
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>Edit: {job.name}</DialogTitle>
          <DialogDescription>Update the schedule or action.</DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <div className="space-y-4">
            {useBuilder ? (
              <ScheduleBuilder onChange={setCron} />
            ) : (
              <div>
                <label className="text-sm font-medium mb-1.5 block">Schedule</label>
                <Input
                  value={cron}
                  onChange={(e: React.ChangeEvent<HTMLInputElement>) =>
                    setCron(e.target.value)
                  }
                  className="font-mono text-xs"
                />
              </div>
            )}
            <button
              type="button"
              className="text-xs text-muted-foreground hover:text-foreground transition-colors"
              onClick={() => setUseBuilder(!useBuilder)}
            >
              {useBuilder ? "Edit cron directly" : "Use schedule builder"}
            </button>
            <ActionFields
              actionType={actionType}
              onActionTypeChange={setActionType}
              body={body}
              onBodyChange={setBody}
            />
            {mutation.error && (
              <p className="text-destructive-foreground text-sm">
                {(mutation.error as Error).message}
              </p>
            )}
          </div>
        </DialogPanel>
        <DialogFooter variant="bare">
          <DialogClose render={<Button variant="outline">Cancel</Button>} />
          <Button
            onClick={() => mutation.mutate()}
            disabled={mutation.isPending || !body}
          >
            {mutation.isPending ? "Saving..." : "Save"}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  );
}

function CreateJobDialog() {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const [name, setName] = useState("");
  const [cron, setCron] = useState("");
  const [actionType, setActionType] = useState<ActionType>("claude_prompt");
  const [body, setBody] = useState("");

  const mutation = useMutation({
    mutationFn: async () => {
      const res = await fetch("/api/jobs", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(buildPayload(actionType, body, cron, name)),
      });
      if (!res.ok) throw new Error(await res.text());
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      setOpen(false);
      setName("");
      setBody("");
    },
  });

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger
        render={
          <Button size="sm">
            <Plus /> Add Job
          </Button>
        }
      />
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>Create Job</DialogTitle>
          <DialogDescription>Schedule a recurring task.</DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <div className="space-y-4">
            <Input
              placeholder="Name"
              value={name}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) =>
                setName(e.target.value)
              }
            />
            <ScheduleBuilder onChange={setCron} />
            <ActionFields
              actionType={actionType}
              onActionTypeChange={setActionType}
              body={body}
              onBodyChange={setBody}
            />
            {mutation.error && (
              <p className="text-destructive-foreground text-sm">
                {(mutation.error as Error).message}
              </p>
            )}
          </div>
        </DialogPanel>
        <DialogFooter variant="bare">
          <DialogClose render={<Button variant="outline">Cancel</Button>} />
          <Button
            onClick={() => mutation.mutate()}
            disabled={mutation.isPending || !name || !body}
          >
            {mutation.isPending ? "Creating..." : "Create"}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  );
}

export const Route = createFileRoute("/jobs")({ component: JobsPage });
