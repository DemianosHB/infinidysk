// Typed helpers for the v1-fork API routes (watchtower, warden, watchdog, search,
// prowlarr). These routes are API-key/session gated like v1's `BaseApiController` and
// are not part of the generated OpenAPI document, so they are called with plain fetch.

export class ForkError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ForkError";
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    credentials: "same-origin",
    ...init,
    headers: { ...(init?.body instanceof FormData ? {} : { "content-type": "application/json" }), ...init?.headers },
  });
  const text = await response.text();
  let json: unknown = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  const fail = json as { status?: boolean; error?: string } | null;
  if (!response.ok || fail?.status === false) {
    throw new ForkError(response.status, fail?.error ?? response.statusText);
  }
  return json as T;
}

function form(fields: Record<string, string>): RequestInit {
  const body = new URLSearchParams();
  for (const [k, v] of Object.entries(fields)) body.set(k, v);
  return { method: "POST", body };
}

// --- Watchtower (`/api/get-watchtower`, `/api/watchtower-mutate`, `/api/watchtower-discover-catalogs`) ---

export type WtSource = {
  id: string;
  kind: string;
  name: string;
  url: string | null;
  enabled: boolean;
  cap: number;
  seriesScope: string | null;
  lastSyncedAtUnix: number | null;
  lastSyncError: string | null;
};

export type WtItem = {
  key: string;
  type: string;
  contentId: string;
  title: string;
  state: string;
  provenanceCount: number;
  expanderKey: string | null;
  childTotal?: number | null;
  childReady?: number | null;
  childUnavailable?: number | null;
  shortlistCount: number;
  winnerTitle: string | null;
  winnerSize: number;
  lastVerifiedAtUnix: number | null;
  nextCheckAtUnix: number | null;
  failReason: string | null;
};

export type WtStats = { total: number; ready: number; scouting: number; unavailable: number; parked: number; expanders: number };

export type WatchtowerData = {
  status: boolean;
  enabled: boolean;
  sources: WtSource[];
  items: WtItem[];
  shows: WtItem[];
  total: number;
  hasMore: boolean;
  stats: WtStats;
};

export function getWatchtower(params: {
  statsOnly?: boolean;
  offset?: number;
  limit?: number;
  state?: string;
  q?: string;
  sort?: string;
  expander?: string;
}): Promise<WatchtowerData> {
  const sp = new URLSearchParams();
  if (params.statsOnly) sp.set("statsOnly", "1");
  if (params.offset) sp.set("offset", String(params.offset));
  if (params.limit) sp.set("limit", String(params.limit));
  if (params.state) sp.set("state", params.state);
  if (params.q) sp.set("q", params.q);
  if (params.sort) sp.set("sort", params.sort);
  if (params.expander) sp.set("expander", params.expander);
  const qs = sp.toString();
  return call<WatchtowerData>(`/api/get-watchtower${qs ? `?${qs}` : ""}`);
}

export function watchtowerMutate(fields: Record<string, string>): Promise<{ status: boolean; error?: string }> {
  return call("/api/watchtower-mutate", form(fields));
}

export type DiscoveredCatalog = { type: string; id: string; name: string; url: string; extraRequired: string | null };

export function discoverCatalogs(url: string): Promise<{ status: boolean; addonName: string | null; catalogs: DiscoveredCatalog[]; error?: string }> {
  return call("/api/watchtower-discover-catalogs", form({ url }));
}

// --- Warden (`/api/get-warden`, `/api/warden-*`) ---

export type WardenSource = {
  id: string;
  kind: string;
  name: string;
  url: string | null;
  enabled: boolean;
  trust: string;
  refreshHours: number;
  lastChecked: number;
  lastUpdated: number;
  status: string | null;
  count: number;
};

export type WardenData = {
  status: boolean;
  count: number;
  localCount: number;
  effectiveCount: number;
  hideDead: boolean;
  quorum: number;
  sources: WardenSource[];
};

export function getWarden(): Promise<WardenData> {
  return call("/api/get-warden");
}

export function wardenSourceAdd(body: { kind?: string; name?: string; url?: string; trust?: string; refreshHours?: number }): Promise<{ status: boolean; id?: string; error?: string }> {
  return call("/api/warden-source-add", { method: "POST", body: JSON.stringify(body) });
}

export function wardenSourceUpdate(body: { id: string; name?: string; url?: string; enabled?: boolean; trust?: string; refreshHours?: number }): Promise<{ status: boolean; error?: string }> {
  return call("/api/warden-source-update", { method: "POST", body: JSON.stringify(body) });
}

export function wardenSourceRemove(id: string): Promise<{ status: boolean; error?: string }> {
  return call("/api/warden-source-remove", { method: "POST", body: JSON.stringify({ id }) });
}

export function wardenExport(): Promise<unknown> {
  return call("/api/warden-export");
}

export function wardenImport(doc: unknown): Promise<{ status: boolean; error?: string }> {
  return call("/api/warden-import", { method: "POST", body: JSON.stringify(doc) });
}

export function wardenSourcesExport(): Promise<unknown> {
  return call("/api/warden-sources-export");
}

export function wardenSourcesImport(doc: unknown): Promise<{ status: boolean; error?: string }> {
  return call("/api/warden-sources-import", { method: "POST", body: JSON.stringify(doc) });
}

// --- Watchdog (`/api/get-watchdog-entries`, `/api/clear-watchdog-entries`) ---

export type WatchdogEntry = {
  clickId: string;
  attemptedAtUnix: number;
  contentType: string;
  requestedTitle: string;
  candidateTitle: string;
  indexerName: string;
  size: number;
  rankIndex: number;
  outcome: string;
  failReason: string | null;
  durationMs: number;
  isWinner: boolean;
  providerHost: string | null;
  providerNickname: string | null;
};

export function getWatchdogEntries(limit = 200): Promise<{ status: boolean; entries: WatchdogEntry[] }> {
  return call(`/api/get-watchdog-entries?limit=${limit}`);
}

export function clearWatchdogEntries(): Promise<{ status: boolean; deleted: number }> {
  return call("/api/clear-watchdog-entries", { method: "POST", body: new URLSearchParams() });
}

// --- Search (`/api/search-indexers`, `/api/v1/queue/from-url`) ---

export type SearchResult = {
  indexer: string;
  title: string;
  nzbUrl: string;
  size: number;
  posted: string | null;
  sourceIndexer: string | null;
  language: string | null;
  subs: string | null;
  infoHash: string | null;
};

export type SearchIndexersData = {
  status: boolean;
  results: SearchResult[];
  indexers: { name: string; ok: boolean; resultCount: number; error: string | null; elapsedMs: number }[];
};

export function searchIndexers(q: string, limit = 100): Promise<SearchIndexersData> {
  return call(`/api/search-indexers?q=${encodeURIComponent(q)}&limit=${limit}`);
}

export function enqueueFromUrl(url: string, name: string): Promise<{ id: string }> {
  return call("/api/v1/queue/from-url", form({ url, nzbname: name }));
}

// --- Prowlarr (`/api/prowlarr-sync`) ---

export type ProwlarrSyncStatus = {
  status: boolean;
  error: string | null;
  configured: boolean;
  syncEnabled: boolean;
  indexersEnvironmentManaged: boolean;
  profilesEnvironmentManaged: boolean;
  lastAttemptAt: number | null;
  lastSuccessAt: number | null;
  remoteIndexerCount: number;
  added: number;
  updated: number;
  removed: number;
  skipped: number;
  indexerConfigJson?: string | null;
  profileConfigJson?: string | null;
};

export function getProwlarrSync(): Promise<ProwlarrSyncStatus> {
  return call("/api/prowlarr-sync");
}

export function syncProwlarrNow(): Promise<ProwlarrSyncStatus> {
  return call("/api/prowlarr-sync", { method: "POST", body: new URLSearchParams() });
}

// --- Fork settings sections (part of `/api/v1/settings`, not in the generated schema yet) ---

export type ForkSettings = {
  watchtower: {
    enabled: boolean;
    profileToken: string;
    ranking: string;
    sizeFloorBytes: number;
    sizeCeilingBytes: number;
    minGrabs: number;
    shortlistDepth: number;
    grabCapPerResolve: number;
    verifySampleCount: number;
    verifyTimeoutSeconds: number;
    activeSetCap: number;
    resolveConcurrency: number;
    dailyResolveBudget: number;
    syncIntervalSeconds: number;
    listSourceMaxResponseBytes: number;
    keepfreshBaseSeconds: number;
    keepfreshMaxSeconds: number;
    seriesRecentCount: number;
    seriesMaxEpisodes: number;
    seriesCapKeep: string;
    seasonBundles: boolean;
    seasonBundleFallback: boolean;
    seasonBundleFallbackScope: string;
    seasonBundleFallbackRecentCount: number;
    seasonBundleFallbackMaxEpisodes: number;
    autoThroughput: boolean;
    verboseLogging: boolean;
    seriesScope: string;
    unavailableRetrySeconds: number;
  };
  warden: { quorum: number; hideDead: boolean; backboneScope: boolean; maxSourceEntries: number };
};

/** The fork sections of the settings document, typed locally until the schema is regenerated. */
export function forkSettings(settings: unknown): ForkSettings {
  return (settings as { watchtower: ForkSettings["watchtower"]; warden: ForkSettings["warden"] });
}

// --- Full fork sections of the settings document (`/api/v1/settings`) ---
// Casing mirrors each Rust struct's serde attribute: `watchtower`/`warden`/`indexers`/
// `profiles` are camelCase, `prowlarr` is snake_case.

export type ForkIndexer = {
  name: string;
  url: string;
  apiKey: string;
  enabled: boolean;
  userAgent?: string | null;
  searchUserAgent?: string | null;
  skipTlsVerification?: boolean;
  maxRequestsPerMinute?: number;
  timeoutSeconds?: number | null;
  searchResultLimit?: number | null;
  maxResponseBytes?: number | null;
  hitLimit?: number | null;
  hitLimitResetTime?: number | null;
  proxyUrl?: string | null;
  extraMovieCategories?: string | null;
  extraTvCategories?: string | null;
  prowlarrIndexerId?: number | null;
};

export type ForkDoc = {
  watchtower: ForkSettings["watchtower"];
  warden: ForkSettings["warden"];
  indexers: {
    proxyUrl: string | null;
    timeoutSeconds: number | null;
    searchResultLimit: number | null;
    maxResponseBytes: number | null;
    indexers: ForkIndexer[];
  };
  profiles: {
    profiles: {
      token: string;
      name: string;
      indexerNames: string[];
      enabledAdapters: string[] | null;
      qualitySort: string;
      preferredAudioLanguages: string[] | null;
      movieFallback: string;
      tvFallback: string;
      movieFallbackMinResults: number;
      tvFallbackMinResults: number;
    }[];
  };
  prowlarr: {
    url: string;
    api_key: string;
    sync_enabled: boolean;
    sync_interval_minutes: number;
  };
};
