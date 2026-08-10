// The primary view (SPEC 14.4): the instance table with the quota bar
// above it, sorted by name, with every operation reachable per row.
import { useState } from "react";
import { api, type Instance } from "../api";
import { relativeTime } from "../format";
import { useAsync, usePoll } from "../hooks";
import {
  DeleteDialog,
  NewInstanceDialog,
  PortDialog,
  RenameDialog,
  ResizeDialog,
  SharesDialog,
  VisibilityDialog,
} from "../components/instance-dialogs";
import { QuotaBar } from "../components/QuotaBar";
import { Button } from "../components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu";
import { Empty, ErrorState, Loading, StateBadge } from "../components/ui/states";

type DialogKind = "rename" | "resize" | "port" | "visibility" | "shares" | "delete";

export function InstancesView() {
  const list = useAsync(api.listInstances);
  usePoll(() => void list.reload(true), 10_000);

  const [creating, setCreating] = useState(false);
  const [dialog, setDialog] = useState<{ kind: DialogKind; instance: Instance } | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const run = async (fn: () => Promise<unknown>) => {
    setActionError(null);
    try {
      await fn();
      await list.reload(true);
    } catch (e) {
      setActionError(e instanceof Error ? e.message : String(e));
    }
  };

  if (list.loading && !list.data) return <Loading what="instances" />;
  if (list.error && !list.data)
    return <ErrorState message={list.error} onRetry={() => void list.reload()} />;
  if (!list.data) return null;

  const { instances, quota, usage } = list.data;
  const refresh = () => void list.reload(true);

  return (
    <div className="space-y-4">
      <QuotaBar quota={quota} usage={usage} />

      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">Instances</h2>
        <Button onClick={() => setCreating(true)}>New instance</Button>
      </div>

      {list.error && <ErrorState message={`Refresh failed: ${list.error}`} onRetry={refresh} />}
      {actionError && <ErrorState message={actionError} />}

      {instances.length === 0 ? (
        <Empty>
          <p>No instances yet.</p>
          <p className="mt-1">
            Create one here, or run{" "}
            <span className="font-mono">ssh bento.foid.space new &lt;name&gt;</span>.
          </p>
          <Button className="mt-4" onClick={() => setCreating(true)}>
            New instance
          </Button>
        </Empty>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-surface1">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-surface1 bg-mantle text-left text-xs text-subtext0">
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 font-medium">State</th>
                <th className="px-3 py-2 font-medium">Address</th>
                <th className="px-3 py-2 font-medium">Image</th>
                <th className="px-3 py-2 font-medium">Visibility</th>
                <th className="px-3 py-2 font-medium">Last use</th>
                <th className="px-3 py-2">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody className="divide-y divide-surface0">
              {instances.map((inst) => (
                <InstanceRow
                  key={inst.uuid}
                  instance={inst}
                  onAction={run}
                  onDialog={(kind) => setDialog({ kind, instance: inst })}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      <NewInstanceDialog open={creating} onClose={() => setCreating(false)} onChanged={refresh} />
      {dialog?.kind === "rename" && (
        <RenameDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
      {dialog?.kind === "resize" && (
        <ResizeDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
      {dialog?.kind === "port" && (
        <PortDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
      {dialog?.kind === "visibility" && (
        <VisibilityDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
      {dialog?.kind === "shares" && (
        <SharesDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
      {dialog?.kind === "delete" && (
        <DeleteDialog instance={dialog.instance} onClose={() => setDialog(null)} onChanged={refresh} />
      )}
    </div>
  );
}

function InstanceRow({
  instance: inst,
  onAction,
  onDialog,
}: {
  instance: Instance;
  onAction: (fn: () => Promise<unknown>) => Promise<void>;
  onDialog: (kind: DialogKind) => void;
}) {
  const mine = !inst.shared_with_me;
  return (
    <tr className="hover:bg-mantle">
      <td className="px-3 py-2 font-mono">
        {inst.name}
        {inst.shared_with_me && (
          <span className="ml-2 rounded bg-surface0 px-1.5 py-0.5 text-xs text-subtext0">
            shared by {inst.owner}
          </span>
        )}
      </td>
      <td className="px-3 py-2">
        <StateBadge state={inst.state} />
      </td>
      <td className="px-3 py-2 font-mono">{inst.address || "—"}</td>
      <td className="px-3 py-2 font-mono">{inst.image}</td>
      <td className="px-3 py-2">{inst.visibility}</td>
      <td className="px-3 py-2 text-subtext0">{relativeTime(inst.last_seen_at)}</td>
      <td className="px-3 py-2 text-right">
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="sm" aria-label={`Actions for ${inst.name}`}>
              ⋯
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            <DropdownMenuItem
              disabled={!mine || inst.state === "running"}
              onSelect={() => void onAction(() => api.start(inst.uuid))}
            >
              Start
            </DropdownMenuItem>
            <DropdownMenuItem
              disabled={!mine || inst.state === "stopped"}
              onSelect={() => void onAction(() => api.stop(inst.uuid))}
            >
              Stop
            </DropdownMenuItem>
            <DropdownMenuItem
              disabled={!mine || inst.state === "stopped"}
              onSelect={() => void onAction(() => api.restart(inst.uuid))}
            >
              Restart
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem disabled={!mine} onSelect={() => onDialog("rename")}>
              Rename…
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!mine} onSelect={() => onDialog("resize")}>
              Resize…
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!mine} onSelect={() => onDialog("port")}>
              HTTP port…
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!mine} onSelect={() => onDialog("visibility")}>
              Visibility…
            </DropdownMenuItem>
            <DropdownMenuItem disabled={!mine} onSelect={() => onDialog("shares")}>
              Sharing…
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem destructive disabled={!mine} onSelect={() => onDialog("delete")}>
              Delete…
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </td>
    </tr>
  );
}
