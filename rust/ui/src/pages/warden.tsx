import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Download, Plus, RefreshCw, Trash2, Upload } from "lucide-react";
import { useRef, useState } from "react";
import { toast } from "sonner";
import {
  getWarden,
  wardenExport,
  wardenImport,
  wardenSourceAdd,
  wardenSourceRemove,
  wardenSourceUpdate,
  wardenSourcesExport,
  wardenSourcesImport,
  type WardenData,
  type WardenSource,
} from "../api/fork";
import { ago } from "../format";
import { useSaveSettings, useSettings } from "../settings";
import { Failure, Loading, SettingsSection } from "../ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";

const POLL_MS = 10_000;

export function WardenPanel({ title, description }: { title: string; description: string }) {
  const settings = useSettings();
  const save = useSaveSettings();
  const client = useQueryClient();
  const warden = useQuery({
    queryKey: ["warden"],
    queryFn: () => getWarden(),
    refetchInterval: POLL_MS,
  });
  const [adding, setAdding] = useState(false);
  const importRef = useRef<HTMLInputElement>(null);
  const sourcesImportRef = useRef<HTMLInputElement>(null);

  const mutateSource = useMutation({
    mutationFn: (fn: () => Promise<unknown>) => fn(),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ["warden"] });
      toast.success("Saved");
    },
    onError: (e) => toast.error((e as Error).message),
  });

  if (settings.isPending || warden.isPending) {
    return (
      <SettingsSection title={title} description={description}>
        <Loading />
      </SettingsSection>
    );
  }
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  if (warden.isError) return <SettingsSection title={title} description={description}><Failure error={warden.error as Error} /></SettingsSection>;

  const view = settings.data;
  const w = view.settings.warden;
  const data: WardenData = warden.data;
  const patchWarden = (next: Partial<typeof w>) => {
    save.mutate({ ...view.settings, warden: { ...w, ...next } }, {
      onError: (e) => toast.error((e as Error).message),
    });
  };

  const download = async (what: () => Promise<unknown>, file: string) => {
    try {
      const doc = await what();
      const blob = new Blob([JSON.stringify(doc, null, 2)], { type: "application/json" });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(blob);
      a.download = file;
      a.click();
      URL.revokeObjectURL(a.href);
    } catch (e) {
      toast.error((e as Error).message);
    }
  };

  const importFile = async (ref: React.RefObject<HTMLInputElement | null>, apply: (doc: unknown) => Promise<unknown>) => {
    const file = ref.current?.files?.[0];
    if (!file) return;
    try {
      apply(JSON.parse(await file.text()));
      toast.success("Imported");
      void client.invalidateQueries({ queryKey: ["warden"] });
    } catch (e) {
      toast.error(`Import failed: ${(e as Error).message}`);
    } finally {
      if (ref.current) ref.current.value = "";
    }
  };

  return (
    <SettingsSection title={title} description={description}>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        {[
          ["Dead entries", data.count],
          ["Local list", data.localCount],
          ["Effective (quorum)", data.effectiveCount],
          ["Sources", data.sources.length],
        ].map(([label, n]) => (
          <div key={label} className="bg-card rounded-lg border p-3">
            <div className="text-muted-foreground text-xs">{label}</div>
            <div className="mt-0.5 text-xl font-semibold tabular-nums">{Number(n).toLocaleString()}</div>
          </div>
        ))}
      </div>

      <div className="flex flex-col gap-3">
        <h3 className="font-medium">Behaviour</h3>
        <label className="flex items-center gap-2 text-sm">
          <Switch checked={w.hideDead} onCheckedChange={(on) => patchWarden({ hideDead: on })} />
          Hide dead releases from search results
        </label>
        <label className="flex items-center gap-2 text-sm">
          <Switch checked={w.backboneScope} onCheckedChange={(on) => patchWarden({ backboneScope: on })} />
          Scope corroboration to the local providers' backbones
        </label>
        <div className="flex flex-wrap items-end gap-3">
          <div className="flex flex-col gap-1">
            <Label htmlFor="warden-quorum">Quorum (corroborating sources)</Label>
            <Input
              id="warden-quorum"
              className="w-24"
              inputMode="numeric"
              value={String(w.quorum)}
              onChange={(e) => patchWarden({ quorum: Number(e.target.value) || 1 })}
            />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="warden-max-entries">Max entries per source</Label>
            <Input
              id="warden-max-entries"
              className="w-28"
              inputMode="numeric"
              value={String(w.maxSourceEntries)}
              onChange={(e) => patchWarden({ maxSourceEntries: Number(e.target.value) || 1 })}
            />
          </div>
        </div>
      </div>

      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="font-medium">Sources</h3>
          <div className="ml-auto flex flex-wrap gap-2">
            <Button variant="outline" size="sm" onClick={() => setAdding(true)}>
              <Plus /> Add source
            </Button>
            <Button variant="outline" size="sm" onClick={() => void download(wardenSourcesExport, "warden-sources.json")}>
              <Download /> Export sources
            </Button>
            <Button variant="outline" size="sm" onClick={() => sourcesImportRef.current?.click()}>
              <Upload /> Import sources
            </Button>
            <input ref={sourcesImportRef} type="file" accept="application/json" className="hidden" onChange={() => void importFile(sourcesImportRef, (d) => wardenSourcesImport(d) as Promise<unknown>)} />
            <Button variant="outline" size="sm" onClick={() => void download(wardenExport, "warden-backup.json")}>
              <Download /> Export all
            </Button>
            <Button variant="outline" size="sm" onClick={() => importRef.current?.click()}>
              <Upload /> Import all
            </Button>
            <input ref={importRef} type="file" accept="application/json" className="hidden" onChange={() => void importFile(importRef, (d) => wardenImport(d) as Promise<unknown>)} />
          </div>
        </div>
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-10">On</TableHead>
                <TableHead>Name</TableHead>
                <TableHead className="hidden md:table-cell">URL</TableHead>
                <TableHead>Trust</TableHead>
                <TableHead className="hidden sm:table-cell tabular-nums">Entries</TableHead>
                <TableHead className="hidden lg:table-cell">Checked</TableHead>
                <TableHead className="w-16"></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {data.sources.map((s) => (
                <SourceRow key={s.id} source={s} disabled={mutateSource.isPending} onMutate={mutateSource} />
              ))}
              {data.sources.length === 0 && (
                <TableRow>
                  <TableCell colSpan={7} className="text-muted-foreground py-6 text-center">
                    Only the local list — add a remote source to corroborate.
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      </div>

      <AddSourceDialog
        open={adding}
        onOpenChange={setAdding}
        disabled={mutateSource.isPending}
        onAdd={(body) => mutateSource.mutate(() => wardenSourceAdd(body))}
      />
    </SettingsSection>
  );
}

function SourceRow({
  source: s,
  disabled,
  onMutate,
}: {
  source: WardenSource;
  disabled: boolean;
  onMutate: ReturnType<typeof useMutation<unknown, Error, () => Promise<unknown>>>;
}) {
  const isLocal = s.id === "local";
  return (
    <TableRow>
      <TableCell>
        <Switch
          checked={s.enabled}
          disabled={disabled || isLocal}
          aria-label={`Enable ${s.name}`}
          onCheckedChange={(on) => onMutate.mutate(() => wardenSourceUpdate({ id: s.id, enabled: on }))}
        />
      </TableCell>
      <TableCell className="max-w-48 truncate font-medium" title={s.name}>
        {s.name}
        {isLocal && <Badge variant="outline" className="ml-1.5">local</Badge>}
      </TableCell>
      <TableCell className="hidden max-w-64 truncate md:table-cell" title={s.url ?? ""}>
        {s.url ?? "—"}
      </TableCell>
      <TableCell>
        <Select
          value={s.trust}
          disabled={disabled || isLocal}
          onValueChange={(v) => onMutate.mutate(() => wardenSourceUpdate({ id: s.id, trust: v }))}
        >
          <SelectTrigger className="h-8 w-32" aria-label={`Trust of ${s.name}`}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="full">Full trust</SelectItem>
            <SelectItem value="corroborate">Corroborate</SelectItem>
            <SelectItem value="observe">Observe</SelectItem>
          </SelectContent>
        </Select>
      </TableCell>
      <TableCell className="hidden tabular-nums sm:table-cell">{s.count.toLocaleString()}</TableCell>
      <TableCell className="hidden text-xs whitespace-nowrap lg:table-cell" title={s.status ?? undefined}>
        {s.lastChecked ? ago(s.lastChecked * 1000) : "never"}
        {s.status && <span className="text-muted-foreground block max-w-40 truncate">{s.status}</span>}
      </TableCell>
      <TableCell>
        {!isLocal && (
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Remove ${s.name}`}
            title={`Remove ${s.name}`}
            disabled={disabled}
            onClick={() => onMutate.mutate(() => wardenSourceRemove(s.id))}
          >
            <Trash2 />
          </Button>
        )}
      </TableCell>
    </TableRow>
  );
}

function AddSourceDialog({
  open,
  onOpenChange,
  disabled,
  onAdd,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  disabled: boolean;
  onAdd: (body: { kind?: string; name?: string; url?: string; trust?: string; refreshHours?: number }) => void;
}) {
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [trust, setTrust] = useState("full");
  const [refreshHours, setRefreshHours] = useState("24");
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add warden source</DialogTitle>
          <DialogDescription>A remote dead-release list, fetched on its refresh interval.</DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-3">
          <div className="flex flex-col gap-1">
            <Label htmlFor="w-name">Name</Label>
            <Input id="w-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. nzbindex dead list" />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="w-url">URL</Label>
            <Input id="w-url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://…" />
          </div>
          <div className="flex gap-3">
            <div className="flex flex-col gap-1">
              <Label htmlFor="w-trust">Trust</Label>
              <Select value={trust} onValueChange={setTrust}>
                <SelectTrigger id="w-trust" className="w-36">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="full">Full trust</SelectItem>
                  <SelectItem value="corroborate">Corroborate</SelectItem>
                  <SelectItem value="observe">Observe</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex flex-col gap-1">
              <Label htmlFor="w-refresh">Refresh (h)</Label>
              <Input id="w-refresh" className="w-20" inputMode="numeric" value={refreshHours} onChange={(e) => setRefreshHours(e.target.value)} />
            </div>
          </div>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            disabled={disabled || !url.trim()}
            onClick={() => {
              onAdd({ kind: "remote", name: name.trim() || undefined, url: url.trim(), trust, refreshHours: Number(refreshHours) || 24 });
              onOpenChange(false);
              setName("");
              setUrl("");
            }}
          >
            <Plus /> Add
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

export { RefreshCw as _WardenRefreshIcon };
