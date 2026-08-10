// Every instance operation of SPEC 15 that needs input or confirmation.
// Destructive dialogs name the instance (SPEC 14.4); the rename dialog
// states both consequences of SPEC 7.3.
import { type FormEvent, useEffect, useState } from "react";
import { api, ApiError, type Image, type Instance } from "../api";
import { cooldownText } from "../format";
import { useAsync } from "../hooks";
import { Button } from "./ui/button";
import { Dialog, DialogClose, DialogContent, DialogFooter } from "./ui/dialog";
import { Field, Input, Select } from "./ui/input";
import { ErrorState, Loading } from "./ui/states";

function errText(e: unknown): string {
  if (e instanceof ApiError && e.cooldownSeconds) {
    return `${e.message} (about ${cooldownText(e.cooldownSeconds)} left)`;
  }
  return e instanceof Error ? e.message : String(e);
}

// useSubmit wraps a dialog's submit action with busy/error state.
function useSubmit(action: () => Promise<void>, onDone: () => void) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await action();
      onDone();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, submit };
}

interface DialogProps {
  instance: Instance;
  onClose: () => void;
  onChanged: () => void;
}

function onOpenChange(onClose: () => void) {
  return (open: boolean) => {
    if (!open) onClose();
  };
}

export function NewInstanceDialog({
  open,
  onClose,
  onChanged,
}: {
  open: boolean;
  onClose: () => void;
  onChanged: () => void;
}) {
  const images = useAsync<Image[]>(api.listImages);
  const [name, setName] = useState("");
  const [image, setImage] = useState("");
  const [vcpu, setVcpu] = useState("");
  const [memory, setMemory] = useState("");
  const [disk, setDisk] = useState("");
  const [nested, setNested] = useState(false);
  const [ksm, setKsm] = useState(true);

  const { busy, error, submit } = useSubmit(async () => {
    await api.createInstance({
      name,
      image: image || images.data?.[0]?.name || "",
      vcpu: vcpu ? Number(vcpu) : undefined,
      memory_mib: memory ? Number(memory) : undefined,
      disk_gib: disk ? Number(disk) : undefined,
      nested: nested || undefined,
      ksm: ksm ? undefined : false,
    });
  }, () => {
    setName("");
    onChanged();
    onClose();
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        wide
        title="New instance"
        description="The name becomes the URL and the SSH user name: lower-case letters, digits, and hyphens."
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <Field label="Name" htmlFor="new-name">
            <Input
              id="new-name"
              className="font-mono"
              value={name}
              onChange={(e) => setName(e.target.value)}
              pattern="[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?"
              placeholder="my-machine"
              required
              autoFocus
            />
          </Field>
          <Field label="Image" htmlFor="new-image">
            {images.loading ? (
              <Loading what="images" />
            ) : images.error ? (
              <ErrorState message={images.error} onRetry={() => void images.reload()} />
            ) : (
              <Select
                id="new-image"
                className="font-mono"
                value={image || images.data?.[0]?.name || ""}
                onChange={(e) => setImage(e.target.value)}
              >
                {(images.data ?? []).map((img) => (
                  <option key={img.name} value={img.name}>
                    {img.name}
                  </option>
                ))}
              </Select>
            )}
          </Field>
          <div className="grid grid-cols-3 gap-3">
            <Field label="vCPU" htmlFor="new-vcpu" hint="empty = default">
              <Input
                id="new-vcpu"
                className="font-mono"
                type="number"
                min={1}
                value={vcpu}
                onChange={(e) => setVcpu(e.target.value)}
              />
            </Field>
            <Field label="Memory (MiB)" htmlFor="new-memory" hint="empty = default">
              <Input
                id="new-memory"
                className="font-mono"
                type="number"
                min={128}
                step={128}
                value={memory}
                onChange={(e) => setMemory(e.target.value)}
              />
            </Field>
            <Field label="Disk (GiB)" htmlFor="new-disk" hint="empty = default">
              <Input
                id="new-disk"
                className="font-mono"
                type="number"
                min={1}
                value={disk}
                onChange={(e) => setDisk(e.target.value)}
              />
            </Field>
          </div>
          <div className="flex gap-6">
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={nested}
                onChange={(e) => setNested(e.target.checked)}
                className="accent-(--accent)"
              />
              Nested virtualization
            </label>
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={ksm}
                onChange={(e) => setKsm(e.target.checked)}
                className="accent-(--accent)"
              />
              KSM (memory dedup)
            </label>
          </div>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" disabled={busy || !name}>
              {busy ? "Creating…" : "Create instance"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

// DeleteDialog is the rm confirmation (SPEC 15): destructive, names the
// instance, and requires typing the name to arm the button.
export function DeleteDialog({ instance, onClose, onChanged }: DialogProps) {
  const [confirm, setConfirm] = useState("");
  const { busy, error, submit } = useSubmit(async () => {
    await api.deleteInstance(instance.uuid);
  }, () => {
    onChanged();
    onClose();
  });
  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`Delete ${instance.name}?`}
        description={
          <>
            This destroys the machine and its disk of{" "}
            <span className="font-mono">{instance.name}</span> for good. The name enters a
            cooldown before another user can take it.
          </>
        }
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <Field
            label={`Type the instance name to confirm`}
            htmlFor="rm-confirm"
          >
            <Input
              id="rm-confirm"
              className="font-mono"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              placeholder={instance.name}
              autoFocus
            />
          </Field>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" variant="destructive" disabled={busy || confirm !== instance.name}>
              {busy ? "Deleting…" : `Delete ${instance.name}`}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

// RenameDialog states both facts of SPEC 7.3: every existing link to the
// old URL stops working, and the SSH user name changes too.
export function RenameDialog({ instance, onClose, onChanged }: DialogProps) {
  const [newName, setNewName] = useState("");
  const { busy, error, submit } = useSubmit(async () => {
    await api.rename(instance.uuid, newName);
  }, () => {
    onChanged();
    onClose();
  });
  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`Rename ${instance.name}`}
        description={
          <div className="space-y-2">
            {instance.visibility === "public" && (
              <p className="rounded border border-yellow/60 bg-yellow/10 px-2 py-1.5 text-text">
                This instance is <strong>public</strong>.
              </p>
            )}
            <p>
              Every existing link to{" "}
              <span className="font-mono">{instance.name}.bento.foid.space</span> stops working.
              Bento does not redirect the old name.
            </p>
            <p>
              The SSH user name changes: connect with{" "}
              <span className="font-mono">ssh {newName || "<new-name>"}@bento.foid.space</span>{" "}
              afterwards.
            </p>
            <p>The old name enters a cooldown before another user can take it.</p>
          </div>
        }
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <Field label="New name" htmlFor="rename-name">
            <Input
              id="rename-name"
              className="font-mono"
              value={newName}
              onChange={(e) => setNewName(e.target.value)}
              pattern="[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?"
              required
              autoFocus
            />
          </Field>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" variant="destructive" disabled={busy || !newName}>
              {busy ? "Renaming…" : `Rename ${instance.name}`}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function ResizeDialog({ instance, onClose, onChanged }: DialogProps) {
  const [vcpu, setVcpu] = useState(String(instance.vcpu));
  const [memory, setMemory] = useState(String(instance.memory_mib));
  const [disk, setDisk] = useState(String(instance.disk_gib));
  const [nested, setNested] = useState(instance.nested);
  const { busy, error, submit } = useSubmit(async () => {
    await api.resize(instance.uuid, {
      vcpu: Number(vcpu),
      memory_mib: Number(memory),
      disk_gib: Number(disk),
      nested,
    });
  }, () => {
    onChanged();
    onClose();
  });
  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`Resize ${instance.name}`}
        description="A change to memory, vCPU, or nested virtualization needs a restart to take effect. The disk grows only; the guest sees the new size after a restart."
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <div className="grid grid-cols-3 gap-3">
            <Field label="vCPU" htmlFor="rs-vcpu">
              <Input
                id="rs-vcpu"
                className="font-mono"
                type="number"
                min={1}
                value={vcpu}
                onChange={(e) => setVcpu(e.target.value)}
                required
              />
            </Field>
            <Field label="Memory (MiB)" htmlFor="rs-memory">
              <Input
                id="rs-memory"
                className="font-mono"
                type="number"
                min={128}
                step={128}
                value={memory}
                onChange={(e) => setMemory(e.target.value)}
                required
              />
            </Field>
            <Field label="Disk (GiB)" htmlFor="rs-disk">
              <Input
                id="rs-disk"
                className="font-mono"
                type="number"
                min={instance.disk_gib}
                value={disk}
                onChange={(e) => setDisk(e.target.value)}
                required
              />
            </Field>
          </div>
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={nested}
              onChange={(e) => setNested(e.target.checked)}
              className="accent-(--accent)"
            />
            Nested virtualization
          </label>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" disabled={busy}>
              {busy ? "Resizing…" : "Resize"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function PortDialog({ instance, onClose, onChanged }: DialogProps) {
  const [port, setPort] = useState(String(instance.http_port || 80));
  const { busy, error, submit } = useSubmit(async () => {
    await api.setPort(instance.uuid, Number(port));
  }, () => {
    onChanged();
    onClose();
  });
  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`HTTP port of ${instance.name}`}
        description={
          <>
            The port that{" "}
            <span className="font-mono">https://{instance.name}.bento.foid.space/</span> forwards
            to inside the instance.
          </>
        }
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <Field label="Port" htmlFor="port-input">
            <Input
              id="port-input"
              className="font-mono"
              type="number"
              min={1}
              max={65535}
              value={port}
              onChange={(e) => setPort(e.target.value)}
              required
              autoFocus
            />
          </Field>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : "Set port"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

const visibilityHelp: Record<string, string> = {
  off: "The name answers 404, the same as a name that does not exist.",
  private: "Visitors must log in to the dashboard first.",
  public: "Anyone can reach the default HTTP port without logging in.",
};

export function VisibilityDialog({ instance, onClose, onChanged }: DialogProps) {
  const [value, setValue] = useState<string>(instance.visibility);
  const { busy, error, submit } = useSubmit(async () => {
    await api.setVisibility(instance.uuid, value);
  }, () => {
    onChanged();
    onClose();
  });
  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`Visibility of ${instance.name}`}
        description="How the HTTP proxy treats requests for this name. Ports 3000–9999 are always private."
      >
        <form onSubmit={submit} className="mt-4 space-y-3">
          <Field label="Visibility" htmlFor="vis-select">
            <Select id="vis-select" value={value} onChange={(e) => setValue(e.target.value)}>
              <option value="off">off</option>
              <option value="private">private</option>
              <option value="public">public</option>
            </Select>
          </Field>
          <p className="text-sm text-subtext0">{visibilityHelp[value]}</p>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="ghost">Cancel</Button>
            </DialogClose>
            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : "Set visibility"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

export function SharesDialog({ instance, onClose, onChanged }: DialogProps) {
  const shares = useAsync(() => api.listShares(instance.uuid));
  const [user, setUser] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setError(null);
  }, [instance.uuid]);

  const add = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.addShare(instance.uuid, user);
      setUser("");
      await shares.reload(true);
      onChanged();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (name: string) => {
    setError(null);
    try {
      await api.removeShare(instance.uuid, name);
      await shares.reload(true);
      onChanged();
    } catch (err) {
      setError(errText(err));
    }
  };

  return (
    <Dialog open onOpenChange={onOpenChange(onClose)}>
      <DialogContent
        title={`Sharing ${instance.name}`}
        description="A shared user can reach the instance over SSH and see it here. Shares follow the instance, not the name."
      >
        <div className="mt-4 space-y-3">
          {shares.loading ? (
            <Loading what="shares" />
          ) : shares.error ? (
            <ErrorState message={shares.error} onRetry={() => void shares.reload()} />
          ) : (shares.data ?? []).length === 0 ? (
            <p className="text-sm text-subtext0">Not shared with anyone.</p>
          ) : (
            <ul className="divide-y divide-surface0 rounded-md border border-surface1">
              {(shares.data ?? []).map((sh) => (
                <li key={sh.user} className="flex items-center justify-between px-3 py-2 text-sm">
                  <span className="font-mono">{sh.user}</span>
                  <Button variant="ghost" size="sm" onClick={() => void remove(sh.user)}>
                    Revoke
                  </Button>
                </li>
              ))}
            </ul>
          )}
          <form onSubmit={add} className="flex gap-2">
            <Input
              aria-label="User to share with"
              className="font-mono"
              value={user}
              onChange={(e) => setUser(e.target.value)}
              placeholder="user name"
            />
            <Button type="submit" disabled={busy || !user}>
              Share
            </Button>
          </form>
          {error && <ErrorState message={error} />}
          <DialogFooter>
            <DialogClose asChild>
              <Button variant="secondary">Done</Button>
            </DialogClose>
          </DialogFooter>
        </div>
      </DialogContent>
    </Dialog>
  );
}
