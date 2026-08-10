// Account view: whoami and quota (SPEC 15), plus the operator's
// download-database control and documented database path (SPEC 12.1).
import { api, dbDownloadPath } from "../api";
import { formatMiB } from "../format";
import { useAsync } from "../hooks";
import { ErrorState, Loading } from "../components/ui/states";

export function AccountView() {
  const who = useAsync(api.whoami);

  if (who.loading) return <Loading what="account" />;
  if (who.error) return <ErrorState message={who.error} onRetry={() => void who.reload()} />;
  if (!who.data) return null;
  const { user, quota, usage, operator, db_path } = who.data;

  const quotaRows = [
    { label: "Instances", used: String(usage.instances), max: quota ? String(quota.max_instances) : null },
    { label: "vCPU", used: String(usage.vcpu), max: quota ? String(quota.max_vcpu) : null },
    { label: "Memory", used: formatMiB(usage.memory_mib), max: quota ? formatMiB(quota.max_memory_mib) : null },
    { label: "Disk", used: `${usage.disk_gib} GiB`, max: quota ? `${quota.max_disk_gib} GiB` : null },
  ];

  return (
    <div className="max-w-2xl space-y-6">
      <section className="space-y-3">
        <h2 className="text-sm font-semibold">Account</h2>
        <dl className="rounded-lg border border-surface1 bg-mantle p-4 text-sm">
          <div className="flex justify-between py-1">
            <dt className="text-subtext0">User</dt>
            <dd className="font-mono">{user.name}</dd>
          </div>
          <div className="flex justify-between py-1">
            <dt className="text-subtext0">Email</dt>
            <dd className="font-mono">{user.email}</dd>
          </div>
          <div className="flex justify-between py-1">
            <dt className="text-subtext0">CLI</dt>
            <dd className="font-mono">ssh bento.foid.space ls</dd>
          </div>
        </dl>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-semibold">Quota</h2>
        <div className="overflow-x-auto rounded-lg border border-surface1">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-surface1 bg-mantle text-left text-xs text-subtext0">
                <th className="px-3 py-2 font-medium">Limit</th>
                <th className="px-3 py-2 font-medium">Used</th>
                <th className="px-3 py-2 font-medium">Maximum</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-surface0">
              {quotaRows.map((row) => (
                <tr key={row.label}>
                  <td className="px-3 py-2">{row.label}</td>
                  <td className="px-3 py-2 font-mono">{row.used}</td>
                  <td className="px-3 py-2 font-mono">{row.max ?? "no limit"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      {operator && (
        <section className="space-y-3">
          <h2 className="text-sm font-semibold">Operator</h2>
          <div className="space-y-3 rounded-lg border border-surface1 bg-mantle p-4 text-sm">
            <p>
              <span className="text-subtext0">Database path: </span>
              <span className="font-mono">{db_path}</span>
            </p>
            <p className="text-subtext0">
              The download below is a consistent snapshot written with the SQLite backup API —
              never copy the database file directly while bentod runs. Instance disks live in the
              storage directory and are backed up separately, together with the image directory.
            </p>
            <a
              href={dbDownloadPath}
              download
              className="inline-flex h-9 items-center justify-center rounded-md bg-accent px-4 text-sm font-medium text-accent-fg hover:opacity-90 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
            >
              Download database
            </a>
          </div>
        </section>
      )}
    </div>
  );
}
