// Image list (SPEC 15 `images`): each image, its current checksum, and
// how many instances still hold an older version.
import { api } from "../api";
import { useState, type FormEvent } from "react";
import { shortChecksum } from "../format";
import { useAsync } from "../hooks";
import { Empty, ErrorState, Loading } from "../components/ui/states";
import { Button } from "../components/ui/button";
import { Field, Input } from "../components/ui/input";

export function ImagesView() {
  const images = useAsync(api.listImages);
  const who = useAsync(api.whoami);
  const [name, setName] = useState("");
  const [reference, setReference] = useState("");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  async function addImage(event: FormEvent) {
    event.preventDefault();
    setAdding(true);
    setAddError(null);
    try {
      await api.addOciImage(name, reference);
      setName("");
      setReference("");
      await images.reload(true);
    } catch (error) {
      setAddError(error instanceof Error ? error.message : String(error));
    } finally {
      setAdding(false);
    }
  }

  if (images.loading) return <Loading what="images" />;
  if (images.error) return <ErrorState message={images.error} onRetry={() => void images.reload()} />;
  const data = images.data ?? [];

  return (
    <div className="space-y-6">
      <h2 className="text-sm font-semibold">Images</h2>
      {who.data?.operator && (
        <form
          onSubmit={(event) => void addImage(event)}
          className="space-y-3 rounded-lg border border-surface1 bg-mantle p-4"
        >
          <div>
            <h3 className="text-sm font-medium">Add bootc OCI image</h3>
            <p className="mt-1 text-xs text-subtext0">
              Bento pulls the operating-system image and builds a qcow2 disk now. This can take
              several minutes.
            </p>
          </div>
          <div className="grid gap-3 md:grid-cols-[12rem_1fr_auto] md:items-end">
            <Field label="Allowed image name" htmlFor="oci-name">
              <Input
                id="oci-name"
                required
                pattern="[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?"
                placeholder="web-os"
                value={name}
                onChange={(event) => setName(event.target.value)}
              />
            </Field>
            <Field label="OCI reference" htmlFor="oci-reference">
              <Input
                id="oci-reference"
                required
                placeholder="quay.io/example/web-os@sha256:…"
                value={reference}
                onChange={(event) => setReference(event.target.value)}
              />
            </Field>
            <Button type="submit" disabled={adding}>
              {adding ? "Building…" : "Add image"}
            </Button>
          </div>
          {addError && <p className="text-sm text-red">{addError}</p>}
        </form>
      )}
      {data.length === 0 ? (
        <Empty>
          <p>No images in the allowlist.</p>
          <p className="mt-1">
            An operator can add a bootc OCI image above, or configure an image and run{" "}
            <span className="font-mono">bentod fetch-images</span>.
          </p>
        </Empty>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-surface1">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-surface1 bg-mantle text-left text-xs text-subtext0">
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 font-medium">Kind</th>
                <th className="px-3 py-2 font-medium">Source</th>
                <th className="px-3 py-2 font-medium">Current checksum</th>
                <th className="px-3 py-2 font-medium">Pinned</th>
                <th className="px-3 py-2 font-medium">Instances on older versions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-surface0">
              {data.map((img) => (
                <tr key={img.name} className="hover:bg-mantle">
                  <td className="px-3 py-2 font-mono">{img.name}</td>
                  <td className="px-3 py-2">{img.kind}</td>
                  <td className="max-w-xs truncate px-3 py-2 font-mono" title={img.source}>
                    {img.source}
                  </td>
                  <td className="px-3 py-2 font-mono" title={img.current_checksum}>
                    {shortChecksum(img.current_checksum)}
                  </td>
                  <td className="px-3 py-2">
                    {img.kind === "oci" ? (
                      <span className="text-subtext0">source digest tracked</span>
                    ) : img.pinned_checksum ? (
                      <span className="font-mono" title={img.pinned_checksum}>
                        {shortChecksum(img.pinned_checksum)}
                      </span>
                    ) : (
                      <span className="text-subtext0">no (trust on first use)</span>
                    )}
                  </td>
                  <td className="px-3 py-2">{img.instances_on_older_versions}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
