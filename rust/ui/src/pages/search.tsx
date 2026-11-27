import { useMutation, useQuery } from "@tanstack/react-query";
import { Download, Search as SearchIcon } from "lucide-react";
import { useState } from "react";
import { useSearchParams } from "react-router";
import { toast } from "sonner";
import { enqueueFromUrl, searchIndexers, type SearchResult } from "../api/fork";
import { bytes } from "../format";
import { Empty, Failure, Loading, Panel, PageHeader } from "../ui";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

export function Search() {
  const [sp, setSp] = useSearchParams();
  const q = sp.get("q")?.trim() ?? "";
  const [input, setInput] = useState(q);

  const query = useQuery({
    queryKey: ["search-indexers", q],
    queryFn: () => searchIndexers(q, 100),
    enabled: q !== "",
  });

  const queue = useMutation({
    mutationFn: (r: SearchResult) => enqueueFromUrl(r.nzbUrl, r.title),
    onSuccess: () => toast.success("Queued for import"),
    onError: (e) => toast.error((e as Error).message),
  });

  return (
    <div className="flex flex-col gap-6">
      <PageHeader title="Search" description="Query your configured indexers and queue an NZB" tab={["Search"]} />
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const next = new URLSearchParams(sp);
          const v = input.trim();
          if (v) next.set("q", v);
          else next.delete("q");
          setSp(next);
        }}
      >
        <Input
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Search your indexers…"
          aria-label="Search your indexers"
          className="min-w-0 flex-1"
          autoFocus
        />
        <Button type="submit" disabled={query.isFetching}>
          {query.isFetching ? "Searching…" : <><SearchIcon /> Search</>}
        </Button>
      </form>

      {q === "" && (
        <Empty title="Nothing searched yet">
          Type a query above to search your configured Newznab indexers. Configure them under Settings → Indexers.
        </Empty>
      )}
      {query.isPending && q !== "" && <Loading />}
      {query.isError && <Failure error={query.error as Error} />}
      {query.data && (
        <>
          <div className="flex flex-wrap gap-2">
            {query.data.indexers.map((i) => (
              <Badge key={i.name} variant={i.ok ? "default" : "destructive"}>
                {i.name}: {i.ok ? `${i.resultCount} results` : "failed"} ({i.elapsedMs} ms)
                {i.error ? ` — ${i.error}` : ""}
              </Badge>
            ))}
          </div>
          {query.data.results.length === 0 ? (
            <Empty title={`No results for “${q}”`} />
          ) : (
            <Panel>
              <ul className="divide-y">
                {query.data.results.map((r) => (
                  <li key={r.nzbUrl} className="flex items-center gap-3 px-3 py-2.5">
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm font-medium" title={r.title}>
                        {r.title}
                      </div>
                      <div className="text-muted-foreground text-xs">
                        {r.indexer} · {bytes(r.size)}
                        {r.posted && ` · ${new Date(r.posted).toLocaleDateString()}`}
                      </div>
                    </div>
                    <Button
                      size="sm"
                      variant="outline"
                      className="shrink-0"
                      disabled={queue.isPending}
                      onClick={() => queue.mutate(r)}
                    >
                      <Download /> Queue
                    </Button>
                  </li>
                ))}
              </ul>
            </Panel>
          )}
        </>
      )}
    </div>
  );
}
