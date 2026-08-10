// SSH key management (SPEC 15 `ssh-key`): add, list, remove.
import { type FormEvent, useState } from "react";
import { api, type SSHKey } from "../api";
import { relativeTime } from "../format";
import { useAsync } from "../hooks";
import { Button } from "../components/ui/button";
import { Dialog, DialogClose, DialogContent, DialogFooter } from "../components/ui/dialog";
import { Field, Input } from "../components/ui/input";
import { Empty, ErrorState, Loading } from "../components/ui/states";

export function KeysView() {
  const keys = useAsync(api.listSSHKeys);
  const [publicKey, setPublicKey] = useState("");
  const [comment, setComment] = useState("");
  const [addError, setAddError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState<SSHKey | null>(null);

  const add = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setAddError(null);
    try {
      await api.addSSHKey(publicKey.trim(), comment.trim());
      setPublicKey("");
      setComment("");
      await keys.reload(true);
    } catch (err) {
      setAddError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="space-y-4">
        <h2 className="text-sm font-semibold">SSH keys</h2>
        {keys.loading ? (
          <Loading what="SSH keys" />
        ) : keys.error ? (
          <ErrorState message={keys.error} onRetry={() => void keys.reload()} />
        ) : (keys.data ?? []).length === 0 ? (
          <Empty>
            <p>No SSH keys.</p>
            <p className="mt-1">Add your public key below to reach instances over SSH.</p>
          </Empty>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-surface1">
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-surface1 bg-mantle text-left text-xs text-subtext0">
                  <th className="px-3 py-2 font-medium">Fingerprint</th>
                  <th className="px-3 py-2 font-medium">Comment</th>
                  <th className="px-3 py-2 font-medium">Added</th>
                  <th className="px-3 py-2">
                    <span className="sr-only">Actions</span>
                  </th>
                </tr>
              </thead>
              <tbody className="divide-y divide-surface0">
                {(keys.data ?? []).map((k) => (
                  <tr key={k.id} className="hover:bg-mantle">
                    <td className="px-3 py-2 font-mono">{k.fingerprint}</td>
                    <td className="px-3 py-2">{k.comment || "—"}</td>
                    <td className="px-3 py-2 text-subtext0">{relativeTime(k.created_at)}</td>
                    <td className="px-3 py-2 text-right">
                      <Button variant="ghost" size="sm" onClick={() => setRemoving(k)}>
                        Remove
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <form onSubmit={add} className="max-w-2xl space-y-3 rounded-lg border border-surface1 bg-mantle p-4">
        <h3 className="text-sm font-semibold">Add a key</h3>
        <Field label="Public key" htmlFor="key-input">
          <textarea
            id="key-input"
            value={publicKey}
            onChange={(e) => setPublicKey(e.target.value)}
            rows={3}
            placeholder="ssh-ed25519 AAAA… user@host"
            className="w-full rounded-md border border-surface1 bg-base px-3 py-2 font-mono text-xs text-text placeholder:text-overlay0 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
            required
          />
        </Field>
        <Field label="Comment (optional)" htmlFor="key-comment">
          <Input
            id="key-comment"
            value={comment}
            onChange={(e) => setComment(e.target.value)}
            placeholder="work laptop"
          />
        </Field>
        {addError && <ErrorState message={addError} />}
        <Button type="submit" disabled={busy || !publicKey.trim()}>
          {busy ? "Adding…" : "Add key"}
        </Button>
      </form>

      {removing && (
        <RemoveKeyDialog
          keyRow={removing}
          onClose={() => setRemoving(null)}
          onChanged={() => void keys.reload(true)}
        />
      )}
    </div>
  );
}

function RemoveKeyDialog({
  keyRow,
  onClose,
  onChanged,
}: {
  keyRow: SSHKey;
  onClose: () => void;
  onChanged: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.deleteSSHKey(keyRow.id);
      onChanged();
      onClose();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent
        title="Remove SSH key?"
        description={
          <>
            The key <span className="font-mono">{keyRow.fingerprint}</span>
            {keyRow.comment ? ` (${keyRow.comment})` : ""} stops opening SSH connections to your
            instances.
          </>
        }
      >
        {error && (
          <div className="mt-3">
            <ErrorState message={error} />
          </div>
        )}
        <DialogFooter>
          <DialogClose asChild>
            <Button variant="ghost">Cancel</Button>
          </DialogClose>
          <Button variant="destructive" disabled={busy} onClick={() => void remove()}>
            {busy ? "Removing…" : "Remove key"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
