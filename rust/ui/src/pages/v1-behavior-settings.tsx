import type { ReactNode } from "react";
import { toast } from "sonner";
import { useSaveSettings, useSettings } from "../settings";
import { Failure, Field, Loading, Locked, SettingsPanel, SettingsSection, Toggle } from "../ui";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

/** Shared plumbing for the v1-fork behavior panels: settings query, save, env-lock lookup. */
function usePanel() {
  const settings = useSettings();
  const save = useSaveSettings();
  const view = settings.data;
  // `locked` paths come from INFINIDYSK__ env vars; match casing-insensitively (snake vs camel).
  const norm = (p: string) => p.replace(/_/g, "").toLowerCase();
  const locked = (path: string) =>
    (view?.locked ?? []).some((l) => {
      const n = norm(l);
      const p = norm(path);
      return n === p || p.startsWith(`${n}.`) || n.startsWith(`${p}.`);
    });
  return { settings, save, view, locked };
}

function PanelFrame({
  title,
  description,
  settings,
  children,
}: {
  title: string;
  description: string;
  settings: ReturnType<typeof useSettings>;
  children: ReactNode;
}) {
  if (settings.isPending) return <SettingsSection title={title} description={description}><Loading /></SettingsSection>;
  if (settings.isError) return <SettingsSection title={title} description={description}><Failure error={settings.error as Error} /></SettingsSection>;
  return (
    <SettingsSection title={title} description={description}>
      <SettingsPanel>{children}</SettingsPanel>
    </SettingsSection>
  );
}

type SectionProps = { title: string; description: string };

/** Repair settings (`repair.*`): read-ahead repair window for active streams. */
export function RepairSettings({ title, description }: SectionProps) {
  const { settings, save, view, locked } = usePanel();
  if (!view) return <PanelFrame title={title} description={description} settings={settings}><Loading /></PanelFrame>;
  const r = view.settings.repair;
  const patch = (next: Partial<typeof r>) =>
    save.mutate({ ...view.settings, repair: { ...r, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  return (
    <PanelFrame title={title} description={description} settings={settings}>
      <p className="text-muted-foreground text-sm">
        PAR2 repair itself is an experiment (Settings → Experiments) and has no other settings. These options control the
        fork's read-ahead repair: while a stream is active, its prefetch window widens so missing or corrupt articles are
        found and repaired before playback reaches them. The player never waits for a repair.
      </p>
      <Toggle
        id="repair-readahead"
        label={<>Read-ahead PAR2 repair <Locked locked={locked("repair.readahead_enabled")} /></>}
        hint="Requires PAR2 gap repair (experiment). Applies to new streams after saving."
        checked={r.readahead_enabled}
        disabled={locked("repair.readahead_enabled")}
        onChange={(on) => patch({ readahead_enabled: on })}
      />
      <Field
        id="repair-readahead-window"
        label={<>Window (seconds) <Locked locked={locked("repair.readahead_window_seconds")} /></>}
        hint="How far ahead of the read head the window widens: 5–600, default 45."
      >
        <Input
          id="repair-readahead-window"
          type="number"
          min={5}
          max={600}
          className="font-mono"
          disabled={locked("repair.readahead_window_seconds")}
          value={r.readahead_window_seconds}
          onChange={(e) => patch({ readahead_window_seconds: Number(e.target.value) })}
        />
      </Field>
    </PanelFrame>
  );
}

/** Search settings (`search.*`): exclude patterns and user agents. */
export function SearchSettings({ title, description }: SectionProps) {
  const { settings, save, view, locked } = usePanel();
  if (!view) return <PanelFrame title={title} description={description} settings={settings}><Loading /></PanelFrame>;
  const s = view.settings.search;
  const patch = (next: Partial<typeof s>) =>
    save.mutate({ ...view.settings, search: { ...s, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  return (
    <PanelFrame title={title} description={description} settings={settings}>
      <Field
        id="search-exclude"
        label={<>Exclude patterns <Locked locked={locked("search.exclude_patterns")} /></>}
        hint="One regex per line (bare body or /body/flags). Matching titles drop out of results. play.excludePatterns falls back to this list."
      >
        <Textarea
          id="search-exclude"
          className="font-mono min-h-24"
          placeholder={"/\\b(cam|web)-?rip\\b/i"}
          disabled={locked("search.exclude_patterns")}
          value={s.exclude_patterns}
          onChange={(e) => patch({ exclude_patterns: e.target.value })}
        />
      </Field>
      <Field
        id="search-user-agent"
        label={<>NZB retrieval User-Agent <Locked locked={locked("search.user_agent")} /></>}
        hint="Sent when downloading NZBs; default SABnzbd/5.1.0."
      >
        <Input
          id="search-user-agent"
          className="font-mono"
          disabled={locked("search.user_agent")}
          value={s.user_agent}
          onChange={(e) => patch({ user_agent: e.target.value })}
        />
      </Field>
      <Field
        id="search-search-user-agent"
        label={<>Indexer search User-Agent <Locked locked={locked("search.search_user_agent")} /></>}
        hint="Sent on indexer search requests."
      >
        <Input
          id="search-search-user-agent"
          className="font-mono"
          disabled={locked("search.search_user_agent")}
          value={s.search_user_agent}
          onChange={(e) => patch({ search_user_agent: e.target.value })}
        />
      </Field>
      <Toggle
        id="search-resolvers"
        label={<>External-ID resolvers <Locked locked={locked("search.resolvers_enabled")} /></>}
        hint="TVmaze/Wikidata/Kitsu/anime-lists lookups for search and profiles."
        checked={s.resolvers_enabled}
        disabled={locked("search.resolvers_enabled")}
        onChange={(on) => patch({ resolvers_enabled: on })}
      />
    </PanelFrame>
  );
}

/** Preflight settings (`preflight.*`): background verification of candidates. */
export function PreflightSettings({ title, description }: SectionProps) {
  const { settings, save, view, locked } = usePanel();
  if (!view) return <PanelFrame title={title} description={description} settings={settings}><Loading /></PanelFrame>;
  const p = view.settings.preflight;
  const patch = (next: Partial<typeof p>) =>
    save.mutate({ ...view.settings, preflight: { ...p, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  const num =
    (key: "maxAttempts" | "ttlSeconds" | "indexerMaxWaitSeconds", min: number, max: number) =>
    (e: React.ChangeEvent<HTMLInputElement>) =>
      patch({ [key]: Math.min(max, Math.max(min, Number(e.target.value) || min)) } as Partial<typeof p>);
  return (
    <PanelFrame title={title} description={description} settings={settings}>
      <Field
        id="preflight-mode"
        label={<>Mode <Locked locked={locked("preflight.mode")} /></>}
        hint="off = no preflight. The harder the mode, the more is verified before playback: light, standard, full."
      >
        <Select value={p.mode} onValueChange={(v) => patch({ mode: v })} disabled={locked("preflight.mode")}>
          <SelectTrigger id="preflight-mode"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="off">Off</SelectItem>
            <SelectItem value="light">Light</SelectItem>
            <SelectItem value="standard">Standard</SelectItem>
            <SelectItem value="full">Full</SelectItem>
          </SelectContent>
        </Select>
      </Field>
      <Field id="preflight-max-attempts" label={<>Max candidates to try <Locked locked={locked("preflight.maxAttempts")} /></>} hint="1–50, default 20.">
        <Input id="preflight-max-attempts" type="number" min={1} max={50} className="font-mono" disabled={locked("preflight.maxAttempts")} value={p.maxAttempts} onChange={num("maxAttempts", 1, 50)} />
      </Field>
      <Field id="preflight-ttl" label={<>Result TTL (seconds) <Locked locked={locked("preflight.ttlSeconds")} /></>} hint="How long a preflight result is reused: 10–1800, default 120.">
        <Input id="preflight-ttl" type="number" min={10} max={1800} className="font-mono" disabled={locked("preflight.ttlSeconds")} value={p.ttlSeconds} onChange={num("ttlSeconds", 10, 1800)} />
      </Field>
      <Field id="preflight-indexer-wait" label={<>Indexer max wait (seconds) <Locked locked={locked("preflight.indexerMaxWaitSeconds")} /></>} hint="How long to wait for indexer responses during preflight: 0–120, default 5.">
        <Input id="preflight-indexer-wait" type="number" min={0} max={120} className="font-mono" disabled={locked("preflight.indexerMaxWaitSeconds")} value={p.indexerMaxWaitSeconds} onChange={num("indexerMaxWaitSeconds", 0, 120)} />
      </Field>
    </PanelFrame>
  );
}

/** Playback settings (`play.*`): candidate selection, verification and failover. */
export function PlaySettings({ title, description }: SectionProps) {
  const { settings, save, view, locked } = usePanel();
  if (!view) return <PanelFrame title={title} description={description} settings={settings}><Loading /></PanelFrame>;
  const p = view.settings.play;
  const patch = (next: Partial<typeof p>) =>
    save.mutate({ ...view.settings, play: { ...p, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  const num =
    (key: "verifySampleCount" | "totalBudgetSeconds" | "hedgeDelaySeconds" | "maxCandidates" | "maxAttempts" | "stallNegativeMarkingMinutes" | "candidateNegativeCacheMinutes" | "resolutionCacheTtlHours", min: number, max: number) =>
    (e: React.ChangeEvent<HTMLInputElement>) =>
      patch({ [key]: Math.min(max, Math.max(min, Number(e.target.value) || min)) } as Partial<typeof p>);
  const numField = (id: string, path: string, label: string, hint: string, key: Parameters<typeof num>[0], min: number, max: number, value: number) => (
    <Field id={id} label={<>{label} <Locked locked={locked(path)} /></>} hint={hint}>
      <Input id={id} type="number" min={min} max={max} className="font-mono" disabled={locked(path)} value={value} onChange={num(key, min, max)} />
    </Field>
  );
  return (
    <PanelFrame title={title} description={description} settings={settings}>
      <Field
        id="play-verify-mode"
        label={<>Verification mode <Locked locked={locked("play.verifyMode")} /></>}
        hint="How candidates are checked before playback: none, stat (NNTP STAT only), body (STAT plus article body spot checks)."
      >
        <Select value={p.verifyMode} onValueChange={(v) => patch({ verifyMode: v })} disabled={locked("play.verifyMode")}>
          <SelectTrigger id="play-verify-mode"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="none">None</SelectItem>
            <SelectItem value="stat">STAT</SelectItem>
            <SelectItem value="body">Body spot checks</SelectItem>
          </SelectContent>
        </Select>
      </Field>
      {numField("play-verify-samples", "play.verifySampleCount", "Body samples per candidate", "1–10, default 3.", "verifySampleCount", 1, 10, p.verifySampleCount)}
      {numField("play-budget", "play.totalBudgetSeconds", "Total time budget (seconds)", "3–180, default 30.", "totalBudgetSeconds", 3, 180, p.totalBudgetSeconds)}
      {numField("play-hedge", "play.hedgeDelaySeconds", "Hedge delay (seconds)", "Start a second candidate after this delay: 1–30, default 3.", "hedgeDelaySeconds", 1, 30, p.hedgeDelaySeconds)}
      {numField("play-max-candidates", "play.maxCandidates", "Max candidates", "1–200, default 10.", "maxCandidates", 1, 200, p.maxCandidates)}
      {numField("play-max-attempts", "play.maxAttempts", "Max attempts", "1–10, default 3.", "maxAttempts", 1, 10, p.maxAttempts)}
      <Toggle id="play-prefer-subtitles" label={<>Prefer subtitles <Locked locked={locked("play.preferSubtitles")} /></>} hint="Prefer releases with subtitles when ranking candidates." checked={p.preferSubtitles} disabled={locked("play.preferSubtitles")} onChange={(on) => patch({ preferSubtitles: on })} />
      <Toggle id="play-watchdog" label={<>Watchdog <Locked locked={locked("play.watchdogEnabled")} /></>} hint="Watchdog verifies and warms likely candidates before playback." checked={p.watchdogEnabled} disabled={locked("play.watchdogEnabled")} onChange={(on) => patch({ watchdogEnabled: on })} />
      <Toggle
        id="play-stall-marking"
        label={<>Stall negative marking <Locked locked={locked("play.stallNegativeMarkingEnabled")} /></>}
        hint="Candidates that abort on a serving stall are remembered, so a warm player retry skips them instead of re-stalling on the same winner. Advisory only; fails open when every candidate is marked."
        checked={p.stallNegativeMarkingEnabled}
        disabled={locked("play.stallNegativeMarkingEnabled")}
        onChange={(on) => patch({ stallNegativeMarkingEnabled: on })}
      />
      {numField("play-stall-minutes", "play.stallNegativeMarkingMinutes", "Stall mark lifetime (minutes)", "1–1440, default 15.", "stallNegativeMarkingMinutes", 1, 1440, p.stallNegativeMarkingMinutes)}
      {numField("play-neg-cache", "play.candidateNegativeCacheMinutes", "Candidate negative cache (minutes)", "1–1440, default 5.", "candidateNegativeCacheMinutes", 1, 1440, p.candidateNegativeCacheMinutes)}
      {numField("play-res-cache", "play.resolutionCacheTtlHours", "Resolution cache TTL (hours)", "1–720, default 168.", "resolutionCacheTtlHours", 1, 720, p.resolutionCacheTtlHours)}
      <Field
        id="play-exclude"
        label={<>Playback exclude patterns <Locked locked={locked("play.excludePatterns")} /></>}
        hint="One regex per line. Matching candidates are skipped during playback; empty falls back to search.exclude_patterns."
      >
        <Textarea
          id="play-exclude"
          className="font-mono min-h-24"
          placeholder={"/\\b(cam|web)-?rip\\b/i"}
          disabled={locked("play.excludePatterns")}
          value={p.excludePatterns}
          onChange={(e) => patch({ excludePatterns: e.target.value })}
        />
      </Field>
    </PanelFrame>
  );
}

/** Stall failover settings (`grab.*`): what happens when a chosen candidate stalls during playback. */
export function GrabSettings({ title, description }: SectionProps) {
  const { settings, save, view, locked } = usePanel();
  if (!view) return <PanelFrame title={title} description={description} settings={settings}><Loading /></PanelFrame>;
  const g = view.settings.grab;
  const patch = (next: Partial<typeof g>) =>
    save.mutate({ ...view.settings, grab: { ...g, ...next } }, { onError: (e) => toast.error((e as Error).message) });
  const num = (key: "stallFailoverWindowSeconds" | "stallFailoverCeilingSeconds", min: number, max: number) => (e: React.ChangeEvent<HTMLInputElement>) =>
    patch({ [key]: Math.min(max, Math.max(min, Number(e.target.value) || min)) } as Partial<typeof g>);
  return (
    <PanelFrame title={title} description={description} settings={settings}>
      <Toggle
        id="grab-stall-failover"
        label={<>Stall failover <Locked locked={locked("grab.stallFailoverEnabled")} /></>}
        hint="When the chosen candidate stalls, fail over to the next candidate instead of waiting it out."
        checked={g.stallFailoverEnabled}
        disabled={locked("grab.stallFailoverEnabled")}
        onChange={(on) => patch({ stallFailoverEnabled: on })}
      />
      <Field id="grab-window" label={<>Stall window (seconds) <Locked locked={locked("grab.stallFailoverWindowSeconds")} /></>} hint="How long without progress counts as a stall: 2–60, default 2.">
        <Input id="grab-window" type="number" min={2} max={60} className="font-mono" disabled={locked("grab.stallFailoverWindowSeconds")} value={g.stallFailoverWindowSeconds} onChange={num("stallFailoverWindowSeconds", 2, 60)} />
      </Field>
      <Field id="grab-ceiling" label={<>Failover ceiling (seconds) <Locked locked={locked("grab.stallFailoverCeilingSeconds")} /></>} hint="Latest point in the file to still switch candidates: 5–120, default 5.">
        <Input id="grab-ceiling" type="number" min={5} max={120} className="font-mono" disabled={locked("grab.stallFailoverCeilingSeconds")} value={g.stallFailoverCeilingSeconds} onChange={num("stallFailoverCeilingSeconds", 5, 120)} />
      </Field>
    </PanelFrame>
  );
}
