// Image list (SPEC 15 `images`): each image, its current checksum, and
// how many instances still hold an older version.
import { api } from "../api";
import { shortChecksum } from "../format";
import { useAsync } from "../hooks";
import { Empty, ErrorState, Loading } from "../components/ui/states";

export function ImagesView() {
  const images = useAsync(api.listImages);

  if (images.loading) return <Loading what="images" />;
  if (images.error) return <ErrorState message={images.error} onRetry={() => void images.reload()} />;
  const data = images.data ?? [];

  return (
    <div className="space-y-4">
      <h2 className="text-sm font-semibold">Images</h2>
      {data.length === 0 ? (
        <Empty>
          <p>No images in the allowlist.</p>
          <p className="mt-1">
            The operator adds images to the configuration and runs{" "}
            <span className="font-mono">bentod fetch-images</span>.
          </p>
        </Empty>
      ) : (
        <div className="overflow-x-auto rounded-lg border border-surface1">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-surface1 bg-mantle text-left text-xs text-subtext0">
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 font-medium">Current checksum</th>
                <th className="px-3 py-2 font-medium">Pinned</th>
                <th className="px-3 py-2 font-medium">Instances on older versions</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-surface0">
              {data.map((img) => (
                <tr key={img.name} className="hover:bg-mantle">
                  <td className="px-3 py-2 font-mono">{img.name}</td>
                  <td className="px-3 py-2 font-mono" title={img.current_checksum}>
                    {shortChecksum(img.current_checksum)}
                  </td>
                  <td className="px-3 py-2">
                    {img.pinned_checksum ? (
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
