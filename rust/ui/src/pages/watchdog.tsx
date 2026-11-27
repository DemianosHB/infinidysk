import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ShieldAlert, Trash2 } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { clearWatchdogEntries, getWatchdogEntries, type WatchdogEntry } from "../api/fork";
import { ago, bytes } from "../format";
import { ConfirmButton, Failure, Loading, Panel, Status, type Tone } from "../ui";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";

const POLL_MS = 5000;

const outcomeTone: Record<string, Tone> = {
  PreVerifyAvailable: "success",
  QueueCompleted: "success",
  PreVerifyDead: "destructive",
  QueueFailed: "destructive",
  EnqueueFailed: "destructive",
  PreVerifyTimeout: "warning",
  BudgetTimeout: "warning",
  Cancelled: "muted",
  ExcludedByPattern: "muted",
};

export function Watchdog() {
  const client = useQueryClient();
  const [limit, setLimit] = useState("200");
  const query = useQuery({
    queryKey: ["watchdog", limit],
    queryFn: () => getWatchdogEntries(Number(limit)),
    placeholderData: keepPreviousData,
    refetchInterval: POLL_MS,
  });
  const clear = useMutation({
    mutationFn: () => clearWatchdogEntries(),
    onSuccess: (r) => {
      toast.success(`Cleared ${r.deleted} entries`);
      void client.invalidateQueries({ queryKey: ["watchdog"] });
    },
    onError: (e) => toast.error((e as Error).message),
  });

  if (query.isPending) return <Loading />;
  if (query.isError) return <Failure error={query.error as Error} />;
  const entries = query.data.entries;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <h1 className="text-xl font-semibold">Watchdog</h1>
        <span className="text-muted-foreground text-sm tabular-nums">{entries.length.toLocaleString()} shown</span>
        <div className="ml-auto flex items-center gap-2">
          <Label htmlFor="wd-limit" className="text-muted-foreground text-sm">
            Limit
          </Label>
          <Select value={limit} onValueChange={setLimit}>
            <SelectTrigger id="wd-limit" className="w-24">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {["50", "200", "500"].map((n) => (
                <SelectItem key={n} value={n}>
                  {n}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <ConfirmButton
            label="Clear all"
            icon={<Trash2 />}
            title="Clear watchdog entries"
            message={`Delete all ${entries.length.toLocaleString()} shown entries? They are also purged automatically over time.`}
            confirm="Clear"
            variant="outline"
            disabled={clear.isPending || entries.length === 0}
            onConfirm={() => clear.mutate()}
          />
        </div>
      </div>
      <Panel>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-36">When</TableHead>
              <TableHead>Outcome</TableHead>
              <TableHead>Requested</TableHead>
              <TableHead className="hidden md:table-cell">Candidate</TableHead>
              <TableHead className="hidden sm:table-cell">Indexer</TableHead>
              <TableHead className="hidden lg:table-cell tabular-nums">Size</TableHead>
              <TableHead className="hidden lg:table-cell tabular-nums">Took</TableHead>
              <TableHead className="hidden xl:table-cell">Provider</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {entries.map((e) => (
              <EntryRow key={`${e.clickId}-${e.candidateTitle}-${e.attemptedAtUnix}`} entry={e} />
            ))}
            {entries.length === 0 && (
              <TableRow>
                <TableCell colSpan={8} className="text-muted-foreground py-6 text-center">
                  No watchdog entries yet — they appear when a play verifies or queues candidates.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Panel>
      <p className="text-muted-foreground text-xs">
        Entries record every play attempt (pre-verify, queue, failover). Providers show their configured nickname.
      </p>
    </div>
  );
}

function EntryRow({ entry: e }: { entry: WatchdogEntry }) {
  return (
    <TableRow>
      <TableCell className="text-xs whitespace-nowrap" title={new Date(e.attemptedAtUnix * 1000).toLocaleString()}>
        {ago(e.attemptedAtUnix * 1000)}
      </TableCell>
      <TableCell>
        <Status tone={outcomeTone[e.outcome] ?? "muted"} compact>
          {e.outcome.replace(/([a-z])([A-Z])/g, "$1 $2")}
        </Status>
        {e.failReason && (
          <div className="text-destructive mt-0.5 max-w-56 truncate text-xs" title={e.failReason}>
            {e.failReason}
          </div>
        )}
      </TableCell>
      <TableCell className="max-w-56 truncate" title={e.requestedTitle}>
        {e.requestedTitle}
        {e.isWinner && (
          <span className="text-success ml-1.5 text-xs font-medium">winner</span>
        )}
      </TableCell>
      <TableCell className="hidden max-w-56 truncate md:table-cell" title={e.candidateTitle}>
        {e.candidateTitle}
      </TableCell>
      <TableCell className="hidden sm:table-cell">{e.indexerName}</TableCell>
      <TableCell className="hidden lg:table-cell tabular-nums">{bytes(e.size)}</TableCell>
      <TableCell className="hidden lg:table-cell tabular-nums">{e.durationMs} ms</TableCell>
      <TableCell className="hidden max-w-40 truncate xl:table-cell" title={e.providerHost ?? undefined}>
        {e.providerNickname ?? e.providerHost ?? "—"}
      </TableCell>
    </TableRow>
  );
}

export { ShieldAlert as WatchdogIcon, Button as _WatchdogButton };
