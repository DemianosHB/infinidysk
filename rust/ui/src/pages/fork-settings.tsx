successfully downloaded text file (SHA: 5a8730a960feead5ee730979ca97e412d2a3ed0f)import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, Plus, RefreshCw, Trash2 } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { getProwlarrSync, syncProwlarrNow, type ForkDoc, type ForkIndexer, type ForkSettings } from "../api/fork";
import { ago } from "../format";
import { copyText, useSaveSettings, useSettings } from "../settings";
import { Failure, Loading, SettingsSection } from "../ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";

type Wt = ForkSettings["watchtower"];

/** Watchtower behaviour settings (`watchtower.*`), saved with the settings document. */
export function WatchtowerSettings({ title, description }: { title: string; description: string }) {
  const settings = useSettings();
  const save = useSaveSettings();
  if (settings.isPending) return <SettingsSection title={title} description={description}><Loading /></SettingsSection>;
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  const view = settings.data;
  const w = view.settings.watchtower;
  const patch = (next: Partial<Wt>) =>
    save.mutate({ ...view.settings, watchtower: { ...w, ...next } }, { onError: (e) => toast.error((e as Error).message) });

  const num = (key: keyof Wt) => (e: React.ChangeEvent<HTMLInputElement>) =>
    patch({ [key]: Number(e.target.value) } as Partial<Wt>);

  return (
    <SettingsSection title={title} description={description}>
      <div className="flex flex-col gap-4">
        <label className="flex items-center gap-2 text-sm font-medium">
          <Switch checked={w.enabled} onCheckedChange={(on) => patch({ enabled: on })} />
          Watchtower enabled
        </label>
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-profile">Search profile token</Label>
            <Input id="wt-profile" value={w.profileToken} onChange={(e) => patch({ profileToken: e.target.value })} placeholder="empty = first profile" />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-ranking">Ranking</Label>
            <Select value={w.ranking} onValueChange={(v) => patch({ ranking: v })}>
              <SelectTrigger id="wt-ranking"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="watchdog">Watchdog (health first)</SelectItem>
                <SelectItem value="largest">Largest first</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-scope">Series scope</Label>
            <Select value={w.seriesScope} onValueChange={(v) => patch({ seriesScope: v })}>
              <SelectTrigger id="wt-scope"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="latest-season">Latest season</SelectItem>
                <SelectItem value="first-season">First season</SelectItem>
                <SelectItem value="all-aired">All aired</SelectItem>
                <SelectItem value="recent">Recent episodes</SelectItem>
                <SelectItem value="off">Don't expand</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <Num id="wt-floor" label="Size floor (bytes)" value={w.sizeFloorBytes} onChange={num("sizeFloorBytes")} />
          <Num id="wt-ceiling" label="Size ceiling (0 = none)" value={w.sizeCeilingBytes} onChange={num("sizeCeilingBytes")} />
          <Num id="wt-grabs" label="Min grabs" value={w.minGrabs} onChange={num("minGrabs")} />
          <Num id="wt-depth" label="Shortlist depth" value={w.shortlistDepth} onChange={num("shortlistDepth")} />
          <Num id="wt-grabcap" label="Grabs per resolve" value={w.grabCapPerResolve} onChange={num("grabCapPerResolve")} />
          <Num id="wt-sample" label="Verify sample count" value={w.verifySampleCount} onChange={num("verifySampleCount")} />
          <Num id="wt-vtimeout" label="Verify timeout (s)" value={w.verifyTimeoutSeconds} onChange={num("verifyTimeoutSeconds")} />
          <Num id="wt-budget" label="Daily resolve budget" value={w.dailyResolveBudget} onChange={num("dailyResolveBudget")} />
          <Num id="wt-sync" label="Source sync interval (s)" value={w.syncIntervalSeconds} onChange={num("syncIntervalSeconds")} />
          <Num id="wt-keep-base" label="Keep-fresh base (s)" value={w.keepfreshBaseSeconds} onChange={num("keepfreshBaseSeconds")} />
          <Num id="wt-keep-max" label="Keep-fresh max (s)" value={w.keepfreshMaxSeconds} onChange={num("keepfreshMaxSeconds")} />
          <Num id="wt-recent" label="Recent episodes per series" value={w.seriesRecentCount} onChange={num("seriesRecentCount")} />
          <Num id="wt-maxep" label="Max episodes per series (0 = all)" value={w.seriesMaxEpisodes} onChange={num("seriesMaxEpisodes")} />
          <Num id="wt-retry" label="Unavailable retry (s)" value={w.unavailableRetrySeconds} onChange={num("unavailableRetrySeconds")} />
          <div className="flex flex-col gap-1">
            <Label htmlFor="wt-capkeep">When capped, keep</Label>
            <Select value={w.seriesCapKeep} onValueChange={(v) => patch({ seriesCapKeep: v })}>
              <SelectTrigger id="wt-capkeep"><SelectValue /></SelectTrigger>
              <SelectContent>
                <SelectItem value="newest">Newest</SelectItem>
                <SelectItem value="oldest">Oldest</SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
        <div className="flex flex-wrap gap-4">
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={w.seasonBundles} onCheckedChange={(on) => patch({ seasonBundles: on })} />
            Season bundles for complete seasons
          </label>
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={w.seasonBundleFallback} onCheckedChange={(on) => patch({ seasonBundleFallback: on })} />
            Episode fallback for parked season bundles
          </label>
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={w.autoThroughput} onCheckedChange={(on) => patch({ autoThroughput: on })} />
            Auto throughput staging
          </label>
          <label className="flex items-center gap-2 text-sm">
            <Switch checked={w.verboseLogging} onCheckedChange={(on) => patch({ verboseLogging: on })} />
            Verbose logging
          </label>
        </div>
      </div>
    </SettingsSection>
  );
}

function Num({ id, label, value, onChange }: { id: string; label: string; value: number; onChange: (e: React.ChangeEvent<HTMLInputElement>) => void }) {
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={id}>{label}</Label>
      <Input id={id} className="w-full" inputMode="numeric" value={String(value)} onChange={onChange} />
    </div>
  );
}

/** Indexer list (`indexers.*`), edited through the settings document. */
export function IndexersPanel({ title, description }: { title: string; description: string }) {
  const settings = useSettings();
  const save = useSaveSettings();
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [apiKey, setApiKey] = useState("");

  if (settings.isPending) return <SettingsSection title={title} description={description}><Loading /></SettingsSection>;
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  const view = settings.data;
  const cfg = view.settings.indexers;
  const indexers = cfg.indexers;

  const patchList = (next: ForkIndexer[]) =>
    save.mutate({ ...view.settings, indexers: { ...cfg, indexers: next } }, { onError: (e) => toast.error((e as Error).message) });

  const add = () => {
    if (!name.trim() || !url.trim()) return;
    patchList([...indexers, { name: name.trim(), url: url.trim(), apiKey, enabled: true } as ForkIndexer]);
    setName("");
    setUrl("");
    setApiKey("");
  };

  return (
    <SettingsSection title={title} description={description}>
      <div className="flex flex-col gap-3">
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-10">On</TableHead>
                <TableHead>Name</TableHead>
                <TableHead className="hidden md:table-cell">URL</TableHead>
                <TableHead className="hidden sm:table-cell">API key</TableHead>
                <TableHead className="w-12"></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {indexers.map((ix, i) => (
                <TableRow key={`${ix.name}-${i}`}>
                  <TableCell>
                    <Switch
                      checked={ix.enabled}
                      aria-label={`Enable ${ix.name}`}
                      onCheckedChange={(on) => patchList(indexers.map((x, j) => (j === i ? { ...x, enabled: on } : x)))}
                    />
                  </TableCell>
                  <TableCell className="max-w-40 truncate font-medium" title={ix.name}>
                    {ix.name}
                    {ix.prowlarrIndexerId != null && <Badge variant="outline" className="ml-1.5">prowlarr</Badge>}
                  </TableCell>
                  <TableCell className="hidden max-w-64 truncate md:table-cell" title={ix.url}>{ix.url}</TableCell>
                  <TableCell className="hidden max-w-32 truncate sm:table-cell">{ix.apiKey ? "••••" : "—"}</TableCell>
                  <TableCell>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={`Remove ${ix.name}`}
                      disabled={ix.prowlarrIndexerId != null}
                      title={ix.prowlarrIndexerId != null ? "Managed by Prowlarr sync" : `Remove ${ix.name}`}
                      onClick={() => patchList(indexers.filter((_, j) => j !== i))}
                    >
                      <Trash2 />
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
              {indexers.length === 0 && (
                <TableRow>
                  <TableCell colSpan={5} className="text-muted-foreground py-6 text-center">
                    No indexers — add one below or sync from Prowlarr.
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            add();
          }}
        >
          <div className="flex min-w-28 flex-col gap-1">
            <Label htmlFor="ix-name">Name</Label>
            <Input id="ix-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="My indexer" />
          </div>
          <div className="flex min-w-40 flex-1 flex-col gap-1">
            <Label htmlFor="ix-url">URL</Label>
            <Input id="ix-url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://indexer/api" className="min-w-0" />
          </div>
          <div className="flex min-w-32 flex-1 flex-col gap-1">
            <Label htmlFor="ix-key">API key</Label>
            <Input id="ix-key" value={apiKey} onChange={(e) => setApiKey(e.target.value)} placeholder="optional" className="min-w-0" />
          </div>
          <Button type="submit" disabled={!name.trim() || !url.trim()}>
            <Plus /> Add indexer
          </Button>
        </form>
      </div>
    </SettingsSection>
  );
}

type Profile = ForkDoc["profiles"]["profiles"][number];

/** Search profiles (`profiles.*`) with their addon URLs. */
export function ProfilesPanel({ title, description }: { title: string; description: string }) {
  const settings = useSettings();
  const save = useSaveSettings();
  const [name, setName] = useState("");

  if (settings.isPending) return <SettingsSection title={title} description={description}><Loading /></SettingsSection>;
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  const view = settings.data;
  const cfg = view.settings.profiles;
  const profiles = cfg.profiles;

  const patchList = (next: Profile[]) =>
    save.mutate({ ...view.settings, profiles: { profiles: next } }, { onError: (e) => toast.error((e as Error).message) });

  const add = () => {
    if (!name.trim()) return;
    // `randomUUID` needs a secure context; LAN IPs are not one, so build the token by hand.
    const token = Array.from(crypto.getRandomValues(new Uint8Array(8)), (b) => b.toString(16).padStart(2, "0")).join("");
    patchList([
      ...profiles,
      {
        token,
        name: name.trim(),
        indexerNames: [],
        enabledAdapters: null,
        qualitySort: "Off",
        preferredAudioLanguages: null,
        movieFallback: "Off",
        tvFallback: "Off",
        movieFallbackMinResults: 3,
        tvFallbackMinResults: 3,
      },
    ]);
    setName("");
  };

  return (
    <SettingsSection title={title} description={description}>
      <div className="flex flex-col gap-3">
        <div className="rounded-lg border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead className="hidden sm:table-cell">Token</TableHead>
                <TableHead className="hidden lg:table-cell">Indexers</TableHead>
                <TableHead className="hidden md:table-cell">Addon URL</TableHead>
                <TableHead className="w-12"></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {profiles.map((p, i) => (
                <TableRow key={p.token || i}>
                  <TableCell className="max-w-40 truncate font-medium" title={p.name}>{p.name}</TableCell>
                  <TableCell className="hidden sm:table-cell">
                    <code className="text-xs">{p.token}</code>
                  </TableCell>
                  <TableCell className="hidden max-w-40 truncate lg:table-cell" title={p.indexerNames.join(", ")}>
                    {p.indexerNames.length ? p.indexerNames.join(", ") : "all"}
                  </TableCell>
                  <TableCell className="hidden md:table-cell">
                    <Button
                      variant="ghost"
                      size="sm"
                      aria-label={`Copy addon URL of ${p.name}`}
                      onClick={() => void copyText(`${location.origin}/adapters/addon/${p.token}/manifest.json`)}
                    >
                      <Copy /> manifest
                    </Button>
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="ghost"
                      size="icon-sm"
                      aria-label={`Remove ${p.name}`}
                      onClick={() => patchList(profiles.filter((_, j) => j !== i))}
                    >
                      <Trash2 />
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
              {profiles.length === 0 && (
                <TableRow>
                  <TableCell colSpan={5} className="text-muted-foreground py-6 text-center">
                    No profiles — add one to expose the Stremio/Newznab adapters.
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
        <form
          className="flex flex-wrap items-end gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            add();
          }}
        >
          <div className="flex min-w-40 flex-1 flex-col gap-1">
            <Label htmlFor="prof-name">Profile name</Label>
            <Input id="prof-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. My Stremio profile" className="min-w-0" />
          </div>
          <Button type="submit" disabled={!name.trim()}>
            <Plus /> Add profile
          </Button>
        </form>
      </div>
    </SettingsSection>
  );
}

/** Prowlarr pull-sync status + connection settings (`prowlarr.*`). */
export function ProwlarrPanel({ title, description }: { title: string; description: string }) {
  const settings = useSettings();
  const save = useSaveSettings();
  const client = useQueryClient();
  const status = useQuery({ queryKey: ["prowlarr-sync"], queryFn: () => getProwlarrSync(), refetchInterval: 30_000 });
  const sync = useMutation({
    mutationFn: () => syncProwlarrNow(),
    onSuccess: (r) => {
      toast.success(r.error ? `Sync failed: ${r.error}` : `Synced: +${r.added} ~${r.updated} -${r.removed}`);
      void client.invalidateQueries({ queryKey: ["prowlarr-sync"] });
      void client.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (e) => toast.error((e as Error).message),
  });

  if (settings.isPending) return <SettingsSection title={title} description={description}><Loading /></SettingsSection>;
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  const view = settings.data;
  const p = view.settings.prowlarr;
  const patch = (next: Partial<typeof p>) =>
    save.mutate({ ...view.settings, prowlarr: { ...p, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  const s = status.data;

  return (
    <SettingsSection title={title} description={description}>
      <div className="flex flex-col gap-4">
        {s && (
          <div className="flex flex-wrap items-center gap-2 text-sm">
            <Badge variant={s.configured ? "default" : "secondary"}>{s.configured ? "configured" : "not configured"}</Badge>
            <Badge variant={s.error ? "destructive" : "default"}>{s.error ? "error" : "ok"}</Badge>
            <span className="text-muted-foreground">
              last attempt {s.lastAttemptAt ? ago(s.lastAttemptAt * 1000) : "never"}
              {s.lastSuccessAt ? ` · last success ${ago(s.lastSuccessAt * 1000)}` : ""}
              {s.remoteIndexerCount > 0 ? ` · ${s.remoteIndexerCount} remote indexers (+${s.added} ~${s.updated} -${s.removed})` : ""}
            </span>
            <Button size="sm" variant="outline" className="ml-auto" disabled={sync.isPending || !s.configured} onClick={() => sync.mutate()}>
              <RefreshCw /> Sync now
            </Button>
          </div>
        )}
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
          <div className="flex flex-col gap-1">
            <Label htmlFor="prowlarr-url">Prowlarr URL</Label>
            <Input id="prowlarr-url" value={p.url} onChange={(e) => patch({ url: e.target.value })} placeholder="http://prowlarr:9696" />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="prowlarr-key">API key</Label>
            <Input id="prowlarr-key" value={p.api_key} onChange={(e) => patch({ api_key: e.target.value })} placeholder="Prowlarr API key" autoComplete="off" />
          </div>
          <div className="flex flex-col gap-1">
            <Label htmlFor="prowlarr-interval">Sync interval (min)</Label>
            <Input id="prowlarr-interval" inputMode="numeric" value={String(p.sync_interval_minutes)} onChange={(e) => patch({ sync_interval_minutes: Number(e.target.value) || 60 })} />
          </div>
          <label className="flex items-center gap-2 self-end pb-2 text-sm">
            <Switch checked={p.sync_enabled} onCheckedChange={(on) => patch({ sync_enabled: on })} />
            Automatic pull-sync
          </label>
        </div>
      </div>
    </SettingsSection>
  );
}
