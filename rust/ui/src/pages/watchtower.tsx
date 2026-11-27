import { keepPreviousData, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, ListPlus, Plus, RefreshCw, Search, Telescope, Trash2 } from "lucide-react";
import { useState } from "react";
import { useSearchParams } from "react-router";
import { toast } from "sonner";
import {
  discoverCatalogs,
  getWatchtower,
  watchtowerMutate,
  type DiscoveredCatalog,
  type WatchtowerData,
  type WtItem,
  type WtSource,
} from "../api/fork";
import { ago, bytes } from "../format";
import { Failure, Loading, PAGE, PageHeader, Pager, Panel, Status, type Tone } from "../ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";

const POLL_MS = 5000;

const SCOPE_OPTIONS = [
  { value: "", label: "Default scope" },
  { value: "latest-season", label: "Latest season" },
  { value: "first-season", label: "First season" },
  { value: "all-aired", label: "All aired seasons" },
  { value: "recent", label: "Recent episodes" },
  { value: "off", label: "Don't expand" },
] as const;

const STATE_FILTERS = [
  { value: "", label: "All states" },
  { value: "ready", label: "Ready" },
  { value: "scouting", label: "Scouting" },
  { value: "unavailable", label: "Unavailable" },
  { value: "parked", label: "Parked" },
  { value: "expander", label: "Shows" },
] as const;

const SORTS = [
  { value: "", label: "Last updated" },
  { value: "title", label: "Title" },
  { value: "recheck", label: "Next recheck" },
  { value: "status", label: "Status" },
] as const;

const stateTone: Record<string, Tone> = {
  ready: "success",
  scouting: "primary",
  unavailable: "destructive",
  parked: "warning",
  expander: "muted",
};

function useParams() {
  const [sp, setSp] = useSearchParams();
  const set = (patch: Record<string, string | undefined>) => {
    const next = new URLSearchParams(sp);
    for (const [k, v] of Object.entries(patch)) {
      if (v === undefined || v === "") next.delete(k);
      else next.set(k, v);
    }
    setSp(next, { replace: false });
  };
  return {
    state: sp.get("state") ?? "",
    q: sp.get("q") ?? "",
    sort: sp.get("sort") ?? "",
    expander: sp.get("expander") ?? "",
    offset: Number(sp.get("offset")) || 0,
    set,
  };
}

export function Watchtower() {
  const p = useParams();
  const client = useQueryClient();
  const query = useQuery({
    queryKey: ["watchtower", p.state, p.q, p.sort, p.expander, p.offset],
    queryFn: () =>
      getWatchtower({
        state: p.state || undefined,
        q: p.q || undefined,
        sort: p.sort || undefined,
        expander: p.expander || undefined,
        offset: p.offset,
        limit: PAGE,
      }),
    placeholderData: keepPreviousData,
    refetchInterval: POLL_MS,
  });

  const mutate = useMutation({
    mutationFn: (fields: Record<string, string>) => watchtowerMutate(fields),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: ["watchtower"] });
    },
    onError: (e) => toast.error((e as Error).message),
  });

  if (query.isPending) return <Loading />;
  if (query.isError) return <Failure error={query.error as Error} />;
  const data = query.data;

  return (
    <div className="flex flex-col gap-6">
      <PageHeader
        title="Watchtower"
        description="List sources feed wanted items; the background loop resolves and keeps them fresh"
        tab={["Watchtower"]}
      />
      <StatsRow stats={data.stats} enabled={data.enabled} total={data.total} />
      <SourcesPanel
        sources={data.sources}
        disabled={mutate.isPending}
        onMutate={(f, ok) => {
          mutate.mutate(f, { onSuccess: () => ok && toast.success("Saved") });
        }}
      />
      <ItemsPanel
        data={data}
        params={p}
        disabled={mutate.isPending}
        onMutate={(f, ok) => {
          mutate.mutate(f, { onSuccess: () => ok && toast.success("Saved") });
        }}
      />
    </div>
  );
}

function StatsRow({ stats, enabled, total }: { stats: WatchtowerData["stats"]; enabled: boolean; total: number }) {
  const cards: [string, number, Tone][] = [
    ["Ready", stats.ready, "success"],
    ["Scouting", stats.scouting, "primary"],
    ["Unavailable", stats.unavailable, "destructive"],
    ["Parked", stats.parked, "warning"],
    ["Expanders", stats.expanders, "muted"],
    ["Total", stats.total || total, "muted"],
  ];
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-6">
      {cards.map(([label, n, tone]) => (
        <Panel key={label} className="p-3">
          <Status tone={tone} compact>
            <span className="text-muted-foreground">{label}</span>
          </Status>
          <div className="mt-1 text-xl font-semibold tabular-nums">{n.toLocaleString()}</div>
        </Panel>
      ))}
      {!enabled && (
        <Panel className="col-span-2 p-3 sm:col-span-3 lg:col-span-6">
          <span className="text-warning-text text-sm">
            Watchtower is disabled in Settings → Watchtower; sources are not synced until it is enabled.
          </span>
        </Panel>
      )}
    </div>
  );
}

function SourcesPanel({
  sources,
  disabled,
  onMutate,
}: {
  sources: WtSource[];
  disabled: boolean;
  onMutate: (fields: Record<string, string>, ok: boolean) => void;
}) {
  const [discoverOpen, setDiscoverOpen] = useState(false);
  const [kind, setKind] = useState("stremio-catalog");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [cap, setCap] = useState("0");
  const [scope, setScope] = useState("");

  const add = () => {
    onMutate({ action: "add-source", kind, name, url, cap, seriesScope: scope }, true);
    setName("");
    setUrl("");
    setCap("0");
  };

  return (
    <section className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <h2 className="text-lg font-semibold">Sources</h2>
        <span className="text-muted-foreground text-sm">{sources.length}</span>
        <div className="ml-auto flex flex-wrap gap-2">
          <Button variant="outline" size="sm" onClick={() => setDiscoverOpen(true)}>
            <Search /> Discover catalogs
          </Button>
        </div>
      </div>
      <Panel>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-10">On</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead>Name</TableHead>
              <TableHead className="hidden md:table-cell">URL</TableHead>
              <TableHead className="hidden sm:table-cell">Cap</TableHead>
              <TableHead className="hidden lg:table-cell">Scope</TableHead>
              <TableHead className="hidden lg:table-cell">Last sync</TableHead>
              <TableHead className="w-24"></TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {sources.map((s) => (
              <SourceRow key={s.id} source={s} disabled={disabled} onMutate={onMutate} />
            ))}
            {sources.length === 0 && (
              <TableRow>
                <TableCell colSpan={8} className="text-muted-foreground py-6 text-center">
                  No list sources yet — add one below or discover Stremio catalogs.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Panel>
      <form
        className="flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          add();
        }}
      >
        <div className="flex flex-col gap-1">
          <Label htmlFor="src-kind">Kind</Label>
          <Select value={kind} onValueChange={setKind}>
            <SelectTrigger id="src-kind" className="w-40">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="stremio-catalog">Stremio catalog</SelectItem>
              <SelectItem value="url-list">URL list</SelectItem>
              <SelectItem value="manual">Manual</SelectItem>
            </SelectContent>
          </Select>
        </div>
        <div className="flex min-w-32 flex-1 flex-col gap-1">
          <Label htmlFor="src-name">Name</Label>
          <Input id="src-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="Optional" />
        </div>
        <div className="flex min-w-40 flex-1 flex-col gap-1">
          <Label htmlFor="src-url">URL</Label>
          <Input
            id="src-url"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder={kind === "manual" ? "Not needed" : "https://…"}
            disabled={kind === "manual"}
          />
        </div>
        <div className="flex flex-col gap-1">
          <Label htmlFor="src-cap">Cap</Label>
          <Input id="src-cap" className="w-20" value={cap} onChange={(e) => setCap(e.target.value)} inputMode="numeric" />
        </div>
        <div className="flex flex-col gap-1">
          <Label htmlFor="src-scope">Scope</Label>
          <Select value={scope} onValueChange={setScope}>
            <SelectTrigger id="src-scope" className="w-44">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {SCOPE_OPTIONS.map((o) => (
                <SelectItem key={o.value} value={o.value}>
                  {o.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <Button type="submit" disabled={disabled || (kind !== "manual" && !url.trim())}>
          <Plus /> Add source
        </Button>
      </form>
      <DiscoverDialog open={discoverOpen} onOpenChange={setDiscoverOpen} disabled={disabled} onMutate={onMutate} />
    </section>
  );
}

function SourceRow({
  source,
  disabled,
  onMutate,
}: {
  source: WtSource;
  disabled: boolean;
  onMutate: (fields: Record<string, string>, ok: boolean) => void;
}) {
  return (
    <TableRow>
      <TableCell>
        <Switch
          checked={source.enabled}
          disabled={disabled}
          aria-label={`Enable ${source.name}`}
          onCheckedChange={(on) => onMutate({ action: "toggle-source", id: source.id, enabled: String(on) }, false)}
        />
      </TableCell>
      <TableCell>
        <Badge variant="outline">{source.kind}</Badge>
      </TableCell>
      <TableCell className="max-w-48 truncate font-medium" title={source.name}>
        {source.name}
      </TableCell>
      <TableCell className="hidden max-w-64 truncate md:table-cell" title={source.url ?? ""}>
        {source.url ?? "—"}
      </TableCell>
      <TableCell className="hidden tabular-nums sm:table-cell">{source.cap > 0 ? source.cap.toLocaleString() : "—"}</TableCell>
      <TableCell className="hidden lg:table-cell">
        <Select
          value={source.seriesScope ?? ""}
          onValueChange={(v) => onMutate({ action: "set-source-scope", id: source.id, seriesScope: v }, false)}
        >
          <SelectTrigger className="h-8 w-44" aria-label={`Scope of ${source.name}`} disabled={disabled}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {SCOPE_OPTIONS.map((o) => (
              <SelectItem key={o.value} value={o.value}>
                {o.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </TableCell>
      <TableCell className="hidden text-xs whitespace-nowrap lg:table-cell" title={source.lastSyncError ?? undefined}>
        {source.lastSyncedAtUnix ? ago(source.lastSyncedAtUnix * 1000) : "never"}
        {source.lastSyncError && <span className="text-destructive block max-w-48 truncate">{source.lastSyncError}</span>}
      </TableCell>
      <TableCell>
        <div className="flex gap-1">
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Sync ${source.name} now`}
            title={`Sync ${source.name} now`}
            disabled={disabled}
            onClick={() => onMutate({ action: "sync-source", id: source.id }, false)}
          >
            <RefreshCw />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Remove ${source.name}`}
            title={`Remove ${source.name}`}
            disabled={disabled}
            onClick={() => onMutate({ action: "remove-source", id: source.id }, false)}
          >
            <Trash2 />
          </Button>
        </div>
      </TableCell>
    </TableRow>
  );
}

function DiscoverDialog({
  open,
  onOpenChange,
  disabled,
  onMutate,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  disabled: boolean;
  onMutate: (fields: Record<string, string>, ok: boolean) => void;
}) {
  const [url, setUrl] = useState("");
  const [bulkScope, setBulkScope] = useState("");
  const [result, setResult] = useState<{ addonName: string | null; catalogs: DiscoveredCatalog[] } | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [caps, setCaps] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const discover = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await discoverCatalogs(url.trim());
      setResult({ addonName: r.addonName, catalogs: r.catalogs });
      setPicked(new Set(r.catalogs.map((c) => c.url)));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const add = () => {
    if (!result) return;
    const sources = result.catalogs
      .filter((c) => picked.has(c.url))
      .map((c) => ({ url: c.url, name: c.name, cap: Number(caps[c.url] || 0) || 0 }));
    if (sources.length === 0) return;
    onMutate({ action: "add-sources", sources: JSON.stringify(sources), seriesScope: bulkScope }, true);
    onOpenChange(false);
    setResult(null);
    setUrl("");
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[85dvh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Discover Stremio catalogs</DialogTitle>
          <DialogDescription>Paste an addon manifest URL; its catalogs can be added as sources.</DialogDescription>
        </DialogHeader>
        <div className="flex gap-2">
          <Input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://addon.example.com or stremio://…"
            aria-label="Manifest URL"
            className="min-w-0 flex-1"
          />
          <Button onClick={() => void discover()} disabled={busy || !url.trim()}>
            {busy ? "…" : "Discover"}
          </Button>
        </div>
        {error && <p className="text-destructive text-sm">{error}</p>}
        {result && (
          <>
            <p className="text-muted-foreground text-sm">
              {result.addonName ? `${result.addonName}: ` : ""}
              {result.catalogs.length} catalogs
            </p>
            <div className="flex max-h-64 flex-col gap-1 overflow-y-auto rounded-md border p-2">
              {result.catalogs.map((c) => (
                <label key={c.url} className="flex items-center gap-2 rounded px-1 py-1 text-sm hover:bg-accent/50">
                  <Checkbox
                    checked={picked.has(c.url)}
                    onCheckedChange={(on) =>
                      setPicked((s) => {
                        const n = new Set(s);
                        if (on) n.add(c.url);
                        else n.delete(c.url);
                        return n;
                      })
                    }
                  />
                  <span className="min-w-0 flex-1 truncate" title={c.url}>
                    {c.name} <span className="text-muted-foreground">({c.type}/{c.id})</span>
                  </span>
                  {c.extraRequired && <span className="text-warning-text text-xs">{c.extraRequired} required</span>}
                  <Input
                    className="h-7 w-20"
                    inputMode="numeric"
                    aria-label={`Cap for ${c.name}`}
                    value={caps[c.url] ?? ""}
                    onChange={(e) => setCaps((m) => ({ ...m, [c.url]: e.target.value }))}
                    placeholder="cap"
                  />
                </label>
              ))}
            </div>
            <div className="flex items-center gap-2">
              <Label htmlFor="bulk-scope">Scope for all</Label>
              <Select value={bulkScope} onValueChange={setBulkScope}>
                <SelectTrigger id="bulk-scope" className="w-44">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {SCOPE_OPTIONS.map((o) => (
                    <SelectItem key={o.value} value={o.value}>
                      {o.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Close
          </Button>
          <Button onClick={add} disabled={disabled || !result || picked.size === 0}>
            Add {picked.size} selected
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function ItemsPanel({
  data,
  params,
  disabled,
  onMutate,
}: {
  data: WatchtowerData;
  params: ReturnType<typeof useParams>;
  disabled: boolean;
  onMutate: (fields: Record<string, string>, ok: boolean) => void;
}) {
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [qInput, setQInput] = useState(params.q);
  const [addType, setAddType] = useState("series");
  const [addId, setAddId] = useState("");
  const [addTitle, setAddTitle] = useState("");

  const filtering = params.state !== "" || params.q !== "";
  const browsingShows = !params.expander && !filtering;

  const toggleAll = (on: boolean) =>
    setSelected(on ? new Set(data.items.map((i) => i.key)) : new Set());

  const bulk = (action: "recheck-items" | "remove-items") => {
    onMutate({ action, keys: [...selected].join("\n") }, true);
    setSelected(new Set());
  };

  return (
    <section className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        {params.expander ? (
          <>
            <Button variant="outline" size="sm" onClick={() => params.set({ expander: undefined })}>
              <ArrowLeft /> Back to {params.state === "expander" || browsingShows ? "shows" : "items"}
            </Button>
            <h2 className="max-w-64 truncate text-lg font-semibold" title={params.expander}>
              {params.expander}
            </h2>
          </>
        ) : (
          <h2 className="text-lg font-semibold">{browsingShows ? "Shows" : "Items"}</h2>
        )}
        <span className="text-muted-foreground text-sm tabular-nums">{data.total.toLocaleString()} total</span>
        {selected.size > 0 && !params.expander && (
          <div className="ml-auto flex gap-2">
            <Button size="sm" variant="outline" disabled={disabled} onClick={() => bulk("recheck-items")}>
              <RefreshCw /> Recheck {selected.size}
            </Button>
            <Button size="sm" variant="destructive" disabled={disabled} onClick={() => bulk("remove-items")}>
              <Trash2 /> Remove {selected.size}
            </Button>
          </div>
        )}
      </div>

      {!params.expander && (
        <div className="flex flex-wrap items-end gap-2">
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-state">State</Label>
            <Select value={params.state} onValueChange={(v) => params.set({ state: v, offset: undefined })}>
              <SelectTrigger id="wt-state" className="w-36">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {STATE_FILTERS.map((o) => (
                  <SelectItem key={o.value} value={o.value}>
                    {o.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-sort">Sort</Label>
            <Select value={params.sort} onValueChange={(v) => params.set({ sort: v })}>
              <SelectTrigger id="wt-sort" className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {SORTS.map((o) => (
                  <SelectItem key={o.value} value={o.value}>
                    {o.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <form
            className="flex min-w-40 flex-1 items-end gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              params.set({ q: qInput.trim() || undefined, offset: undefined });
            }}
          >
            <div className="flex min-w-0 flex-1 flex-col gap-1">
              <Label htmlFor="wt-q">Search</Label>
              <Input
                id="wt-q"
                value={qInput}
                onChange={(e) => setQInput(e.target.value)}
                placeholder="Title or id…"
                className="min-w-0"
              />
            </div>
            <Button type="submit" variant="outline" size="sm">
              <Search /> Search
            </Button>
          </form>
        </div>
      )}

      {browsingShows && data.shows.length > 0 && <ShowsList shows={data.shows} onOpen={(key) => params.set({ expander: key })} />}

      <Panel>
        <Table>
          <TableHeader>
            <TableRow>
              {!params.expander && (
                <TableHead className="w-10">
                  <Checkbox
                    aria-label="Select all"
                    checked={data.items.length > 0 && selected.size === data.items.length}
                    onCheckedChange={(on) => toggleAll(!!on)}
                  />
                </TableHead>
              )}
              <TableHead>Title</TableHead>
              <TableHead className="hidden sm:table-cell">Type</TableHead>
              <TableHead>State</TableHead>
              <TableHead className="hidden md:table-cell">Shortlist</TableHead>
              <TableHead className="hidden lg:table-cell">Next check</TableHead>
              <TableHead className="w-24"></TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {data.items.map((item) => (
              <ItemRow
                key={item.key}
                item={item}
                selectable={!params.expander}
                selected={selected.has(item.key)}
                onSelect={(on) =>
                  setSelected((s) => {
                    const n = new Set(s);
                    if (on) n.add(item.key);
                    else n.delete(item.key);
                    return n;
                  })
                }
                disabled={disabled}
                onMutate={onMutate}
              />
            ))}
            {data.items.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="text-muted-foreground py-6 text-center">
                  {params.expander ? "No episodes for this show yet." : "No items match."}
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Panel>
      <Pager offset={params.offset} more={data.hasMore} onChange={(o) => params.set({ offset: o ? String(o) : undefined })} />

      {!params.expander && (
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (!addId.trim()) return;
            onMutate({ action: "add-item", type: addType, id: addId.trim(), title: addTitle.trim() }, true);
            setAddId("");
            setAddTitle("");
          }}
        >
          <div className="flex flex-col gap-1">
            <Label htmlFor="add-type">Type</Label>
            <Select value={addType} onValueChange={setAddType}>
              <SelectTrigger id="add-type" className="w-28">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="series">Series</SelectItem>
                <SelectItem value="movie">Movie</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="flex min-w-40 flex-1 flex-col gap-1">
            <Label htmlFor="add-id">Content id</Label>
            <Input id="add-id" value={addId} onChange={(e) => setAddId(e.target.value)} placeholder="tt1234567" className="min-w-0" />
          </div>
          <div className="flex min-w-32 flex-1 flex-col gap-1">
            <Label htmlFor="add-title">Title</Label>
            <Input id="add-title" value={addTitle} onChange={(e) => setAddTitle(e.target.value)} placeholder="Optional" className="min-w-0" />
          </div>
          <Button type="submit" disabled={disabled || !addId.trim()}>
            <ListPlus /> Add item
          </Button>
        </form>
      )}
    </section>
  );
}

function ShowsList({ shows, onOpen }: { shows: WtItem[]; onOpen: (key: string) => void }) {
  return (
    <Panel>
      <Table>
        <TableHeader>
          <TableRow>
            <TableHead>Show</TableHead>
            <TableHead className="hidden sm:table-cell">Type</TableHead>
            <TableHead className="tabular-nums">Children</TableHead>
            <TableHead className="hidden md:table-cell tabular-nums">Ready</TableHead>
            <TableHead className="hidden md:table-cell tabular-nums">Unavailable</TableHead>
            <TableHead className="w-16"></TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {shows.map((s) => (
            <TableRow key={s.key}>
              <TableCell className="max-w-64 truncate font-medium" title={s.title}>
                {s.title}
              </TableCell>
              <TableCell className="hidden sm:table-cell">{s.type}</TableCell>
              <TableCell className="tabular-nums">{(s.childTotal ?? 0).toLocaleString()}</TableCell>
              <TableCell className="hidden md:table-cell tabular-nums">{(s.childReady ?? 0).toLocaleString()}</TableCell>
              <TableCell className="hidden md:table-cell tabular-nums">{(s.childUnavailable ?? 0).toLocaleString()}</TableCell>
              <TableCell>
                <Button size="sm" variant="outline" onClick={() => onOpen(s.key)}>
                  Open
                </Button>
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </Panel>
  );
}

function ItemRow({
  item,
  selectable,
  selected,
  onSelect,
  disabled,
  onMutate,
}: {
  item: WtItem;
  selectable: boolean;
  selected: boolean;
  onSelect: (on: boolean) => void;
  disabled: boolean;
  onMutate: (fields: Record<string, string>, ok: boolean) => void;
}) {
  return (
    <TableRow>
      {selectable && (
        <TableCell>
          <Checkbox aria-label={`Select ${item.title}`} checked={selected} onCheckedChange={(on) => onSelect(!!on)} />
        </TableCell>
      )}
      <TableCell className="max-w-64">
        <div className="truncate font-medium" title={item.title}>
          {item.title}
        </div>
        <div className="text-muted-foreground truncate text-xs" title={item.contentId}>
          {item.contentId}
        </div>
        {item.failReason && <div className="text-destructive truncate text-xs">{item.failReason}</div>}
      </TableCell>
      <TableCell className="hidden sm:table-cell">{item.type}</TableCell>
      <TableCell>
        <Status tone={stateTone[item.state] ?? "muted"} compact>
          {item.state}
        </Status>
      </TableCell>
      <TableCell className="hidden md:table-cell">
        {item.shortlistCount > 0 ? (
          <span title={item.winnerTitle ?? undefined}>
            {item.shortlistCount} · {bytes(item.winnerSize)}
          </span>
        ) : (
          "—"
        )}
      </TableCell>
      <TableCell className="hidden text-xs whitespace-nowrap lg:table-cell">
        {item.nextCheckAtUnix ? ago(item.nextCheckAtUnix * 1000) : "—"}
      </TableCell>
      <TableCell>
        <div className="flex gap-1">
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Recheck ${item.title}`}
            title={`Recheck ${item.title}`}
            disabled={disabled}
            onClick={() => onMutate({ action: "recheck-item", key: item.key }, false)}
          >
            <RefreshCw />
          </Button>
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={`Remove ${item.title}`}
            title={`Remove ${item.title}`}
            disabled={disabled}
            onClick={() => onMutate({ action: "remove-item", key: item.key }, false)}
          >
            <Trash2 />
          </Button>
        </div>
      </TableCell>
    </TableRow>
  );
}

export { Telescope as WatchtowerIcon };
