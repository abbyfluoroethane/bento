// Typed client for the Bento control plane API (internal/api).

export interface Instance {
  uuid: string;
  name: string;
  owner: string;
  state: "running" | "starting" | "stopped" | string;
  desired_state: string;
  address: string;
  mac: string;
  image: string;
  base_checksum: string;
  vcpu: number;
  memory_mib: number;
  disk_gib: number;
  nested: boolean;
  ksm: boolean;
  http_port: number;
  visibility: "off" | "private" | "public";
  created_at: string;
  last_seen_at: string;
  shared_with_me: boolean;
}

export interface Quota {
  max_instances: number;
  max_vcpu: number;
  max_memory_mib: number;
  max_disk_gib: number;
}

export interface Usage {
  instances: number;
  vcpu: number;
  memory_mib: number;
  disk_gib: number;
}

export interface InstanceList {
  instances: Instance[];
  quota: Quota | null;
  usage: Usage;
}

export interface Whoami {
  user: { id: number; name: string; email: string; created_at: string };
  quota: Quota | null;
  usage: Usage;
  operator: boolean;
  db_path?: string;
}

export interface Image {
  name: string;
  url: string;
  pinned_checksum: string;
  current_checksum: string;
  instances_on_older_versions: number;
}

export interface SSHKey {
  id: number;
  fingerprint: string;
  comment: string;
  public_key: string;
  created_at: string;
}

export interface Share {
  user: string;
  created_at: string;
}

export interface CreateRequest {
  name: string;
  image: string;
  vcpu?: number;
  memory_mib?: number;
  disk_gib?: number;
  nested?: boolean;
  ksm?: boolean;
}

export class ApiError extends Error {
  status: number;
  cooldownSeconds?: number;
  constructor(status: number, message: string, cooldownSeconds?: number) {
    super(message);
    this.status = status;
    this.cooldownSeconds = cooldownSeconds;
  }
}

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    credentials: "same-origin",
  });
  if (res.status === 204) return undefined as T;
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    let cooldown: number | undefined;
    try {
      const data = await res.json();
      if (data && typeof data.error === "string") message = data.error;
      if (data && typeof data.cooldown_seconds === "number") cooldown = data.cooldown_seconds;
    } catch {
      // Not JSON; keep the status text.
    }
    throw new ApiError(res.status, message, cooldown);
  }
  return (await res.json()) as T;
}

export const api = {
  whoami: () => req<Whoami>("GET", "/api/whoami"),
  listInstances: () => req<InstanceList>("GET", "/api/instances"),
  createInstance: (r: CreateRequest) => req<Instance>("POST", "/api/instances", r),
  deleteInstance: (uuid: string) => req<void>("DELETE", `/api/instances/${uuid}`),
  start: (uuid: string) => req<unknown>("POST", `/api/instances/${uuid}/start`),
  stop: (uuid: string) => req<unknown>("POST", `/api/instances/${uuid}/stop`),
  restart: (uuid: string) => req<unknown>("POST", `/api/instances/${uuid}/restart`),
  rename: (uuid: string, newName: string) =>
    req<Instance>("POST", `/api/instances/${uuid}/rename`, { new_name: newName }),
  resize: (
    uuid: string,
    r: { vcpu?: number; memory_mib?: number; disk_gib?: number; nested?: boolean },
  ) => req<Instance>("POST", `/api/instances/${uuid}/resize`, r),
  setPort: (uuid: string, port: number) =>
    req<Instance>("POST", `/api/instances/${uuid}/port`, { port }),
  setVisibility: (uuid: string, visibility: string) =>
    req<Instance>("POST", `/api/instances/${uuid}/visibility`, { visibility }),
  listShares: (uuid: string) => req<Share[]>("GET", `/api/instances/${uuid}/shares`),
  addShare: (uuid: string, user: string) =>
    req<Share>("POST", `/api/instances/${uuid}/shares`, { user }),
  removeShare: (uuid: string, user: string) =>
    req<void>("DELETE", `/api/instances/${uuid}/shares/${encodeURIComponent(user)}`),
  listImages: () => req<Image[]>("GET", "/api/images"),
  listSSHKeys: () => req<SSHKey[]>("GET", "/api/ssh-keys"),
  addSSHKey: (publicKey: string, comment: string) =>
    req<SSHKey>("POST", "/api/ssh-keys", { public_key: publicKey, comment }),
  deleteSSHKey: (id: number) => req<void>("DELETE", `/api/ssh-keys/${id}`),
};

// The download-database control (SPEC 12.1) is a plain link, not an XHR.
export const dbDownloadPath = "/api/db.sqlite";
