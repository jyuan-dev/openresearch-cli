import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { ArrowUp, Folder, GitBranch, House } from "lucide-react";
import { m } from "../paraglide/messages.js";
import { ltr } from "../i18n";
import { useWorkspace } from "../workspace";
import { Badge, Button, LoadingRow } from "./ui";

/**
 * Browses folders on the machine the server runs on, so an existing project
 * folder can be chosen from any device. The native picker only opens on that
 * machine's own screen, and not at all from a background service.
 */
export function FolderBrowser({ start, onChoose, onCancel }: {
  start?: string;
  onChoose: (path: string) => void;
  onCancel: () => void;
}) {
  const { getProjectFolderListingQuery } = useWorkspace().queries;
  const [dir, setDir] = useState(start ?? "");
  const [hidden, setHidden] = useState(false);
  const listing = useQuery(getProjectFolderListingQuery(dir, hidden));
  const here = listing.data;
  return (
    <section className="flex flex-col gap-2 rounded-md border border-border bg-background p-2" aria-label={m.folder_browser_label()}>
      <div className="flex items-center gap-1">
        <Button type="button" size="small" variant="ghost" disabled={!here?.parent} onClick={() => here?.parent && setDir(here.parent)}>
          <ArrowUp size={14} /> {m.folder_browser_up()}
        </Button>
        <Button type="button" size="small" variant="ghost" disabled={!here?.home || here.home === here.path} onClick={() => here?.home && setDir(here.home)}>
          <House size={14} /> {m.folder_browser_home()}
        </Button>
        <code className="min-w-0 flex-1 truncate text-xs text-subtext" dir="ltr" title={here?.path}>{here ? ltr(here.path) : dir}</code>
      </div>
      <div className="flex max-h-64 flex-col overflow-y-auto rounded-sm border border-border-variant" role="list">
        {listing.error ? (
          <div className="flex items-center gap-2 p-2 text-sm text-accent-red" role="alert">
            <span className="flex-1">{listing.error.message}</span>
            {dir && <Button type="button" size="small" onClick={() => setDir("")}>{m.folder_browser_home()}</Button>}
          </div>
        ) : !here ? (
          <LoadingRow />
        ) : here.folders.length === 0 ? (
          <p className="m-0 p-2 text-sm text-subtext">{m.folder_browser_empty()}</p>
        ) : (
          here.folders.map((folder) => (
            <button
              key={folder.path}
              type="button"
              role="listitem"
              className="flex items-center gap-2 border-0 border-b border-b-border-variant bg-transparent px-2 py-1.5 text-start text-sm text-text last:border-b-0 hover:bg-surface"
              aria-label={m.folder_browser_open({ name: folder.name })}
              onClick={() => setDir(folder.path)}
            >
              <Folder size={14} className="flex-none text-subtext" />
              <span className="min-w-0 flex-1 truncate" dir="auto">{folder.name}</span>
              {folder.git && <Badge size="small"><GitBranch size={12} /> {m.folder_browser_git()}</Badge>}
            </button>
          ))
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <label className="!flex-row items-center !gap-1.5 !font-normal text-xs text-subtext">
          <input type="checkbox" checked={hidden} onChange={(event) => setHidden(event.target.checked)} />
          {m.folder_browser_hidden()}
        </label>
        <div className="ms-auto flex gap-2">
          <Button type="button" size="small" variant="ghost" onClick={onCancel}>{m.common_cancel()}</Button>
          <Button type="button" size="small" variant="primary" disabled={!here} onClick={() => here && onChoose(here.path)}>
            {m.folder_browser_choose()}
          </Button>
        </div>
      </div>
    </section>
  );
}
