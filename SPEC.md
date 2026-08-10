# Bento System Specification

Version 0.3. Draft.

Bento is a self-hosted platform for Linux instances. A user creates an instance from the command line interface or from the dashboard. Bento publishes the instance on the internet as a subdomain of a domain that the operator owns. An instance exists until the user deletes it.

## 1. Terms

| Term | Meaning |
| --- | --- |
| instance | One container or one virtual machine that Incus manages. |
| the operator | The person who installs and runs Bento on a server. |
| the user | A person with a Bento account. A user creates instances. |
| the control plane | The Bento server process. The binary is `bentod`. |
| the dashboard | The web interface of Bento. The control plane serves the dashboard at the base domain. |
| the command line interface | The text interface of Bento. The SSH frontend serves the command line interface. |
| the HTTP proxy | The component that terminates TLS and forwards HTTP requests to an instance. |
| the SSH frontend | The component that accepts SSH connections and forwards them to an instance. |
| the base domain | The domain that the operator chooses. This document uses `bento.foid.space`. |

The term "instance" covers containers and virtual machines. This document uses "container" or "virtual machine" only where the difference changes the design.

## 2. Scope

Bento version 1 does the following:

- Create, list, start, stop, resize, and delete instances.
- Optionally publish each instance at `$NAME.bento.foid.space` over HTTPS.
- Accept SSH connections to each instance through one public port.
- Support more than one user on one server.
- Enforce a per-user quota on the instance count, the memory, the vCPU count, and the disk.

Bento targets one server and fewer than about 20 users. This number is a design target, not a measured limit. Section 3 follows from it.

## 3. Non-goals

Bento does not implement virtualization. Incus does this.

Bento does not build container images. The operator uses existing OCI images or Incus images.

Bento does not schedule instances across more than one server. One Bento deployment controls one server.

Bento does not bill users.

Bento does not use a network database. SQLite is sufficient at the scale in section 2.

**Bento never deletes an instance.** Only the user deletes an instance. Bento has no expiry timer, no grace period, and no idle detection. A quota is the pressure on resource use, not a clock. Section 6 defines the quota.

Idle detection is the reason for this rule. Idle detection needs a definition of activity, and every definition is wrong for some workload. A long build with no network traffic and no SSH session looks idle. A tool that deletes work is worse than a tool that runs out of disk.

## 4. Architecture

Three Bento processes and one Incus daemon run on the server.

- **Incus** owns the instances, the storage pool, the bridge networks, and the network ACLs. Bento calls the Incus REST API over a local Unix socket.
- **The control plane** holds the database, applies policy, calls Incus, and serves the dashboard. The control plane is the only component that writes to the database.
- **The HTTP proxy** listens on port 443 and on ports 3000 to 9999. The HTTP proxy terminates TLS. The HTTP proxy forwards a request for the base domain to the control plane. The HTTP proxy forwards every other request to an instance.
- **The SSH frontend** listens on port 22. The SSH frontend authenticates the client, then opens a second SSH connection to the instance. The SSH frontend also serves the command line interface.

The HTTP proxy and the SSH frontend read instance state from the control plane. Build all three processes as one binary with subcommands. This keeps deployment simple.

### 4.1 Incus configuration

Leave `core.https_address` unset. Incus then listens only on `/var/lib/incus/unix.socket`. The Incus API is not reachable from the network.

Do not install the `incus-ui-canonical` package. Incus ships no web interface by default. The package installs static files that `incusd` serves at the `/ui` path.

Both settings are deliberate. Record them in the deployment documentation, because an absent listener leaves no evidence of the decision.

The control plane needs a member account in the `incus-admin` group. This group is equivalent to root on the host. Treat every string that reaches an Incus API call as hostile. Instance names reach project names, network names, and DNS records.

Do not create Incus ephemeral instances. Incus deletes an ephemeral instance when the instance stops. This conflicts with the rule in section 3.

## 5. Instance layer

Incus provides the instance layer. Do not write a hypervisor integration.

Incus gives Bento these things without new code:

- One REST API for containers and for virtual machines.
- Copy-on-write instance creation from a cached image on ZFS or on Btrfs. Creation takes about two seconds. This number is a common report, not a measurement of this design.
- Projects. A project is a namespace with its own instances, networks, profiles, and resource limits.
- Snapshots and instance copy.

**Decision required: containers or virtual machines.** A container starts faster and uses less memory. A virtual machine gives a separate kernel. Choose virtual machines only if instances must run untrusted code, load kernel modules, use nested virtualization, or access raw devices. Bento supports both types because Incus supports both types. Set the default type in the operator configuration.

## 6. Multi-user model

Map one Bento user to one Incus project. The project gives per-user isolation of instances, networks, and limits without new code in Bento.

Apply resource limits at the project level. Limit the instance count, the total memory, the total vCPU count, and the total disk size. Incus enforces these limits. Bento does not need its own quota accounting.

The quota is the only backstop against a full disk. Section 3 removed the expiry timer, so a user who forgets an instance holds the disk until that user deletes the instance. Set the sum of the per-project disk limits below the size of the storage pool. Do not oversubscribe disk.

Report quota use in the `ls` command and on the dashboard. A user cannot manage a quota that the user cannot see.

Give each project its own bridge network. Do not put all instances on one shared bridge. On a shared bridge, an attacker who controls one instance can reach every other instance.

Apply an Incus network ACL to each project network. The default rule denies traffic between instances. The ACL permits egress to the internet and permits ingress from the HTTP proxy and from the SSH frontend.

## 7. Naming and DNS

Each instance has a name. The name is unique across the deployment, not per user. A shared namespace keeps the URL short and keeps the SSH login name simple.

Create these DNS records:

1. Create an `A` record for `bento.foid.space` that points to the server.
2. Create an `A` record for `*.bento.foid.space` that points to the server.
3. Create the equivalent `AAAA` records if the server has IPv6.

The dashboard runs at `https://bento.foid.space`. An instance runs at `https://$NAME.bento.foid.space`.

## 8. TLS

Get one wildcard certificate for `*.bento.foid.space` and `bento.foid.space`. Use the ACME DNS-01 challenge. The DNS-01 challenge is required for a wildcard certificate.

Do not issue one certificate for each instance. Let's Encrypt limits new certificates to 50 per registered domain per 7 days. The limit refills at one certificate every 202 minutes. A deployment that creates and destroys instances often will exceed this limit. Check the current limit before you build, because Let's Encrypt changes these numbers.

A wildcard certificate also keeps instance names out of the Certificate Transparency logs. A per-instance certificate publishes the name of every instance.

The control plane needs write access to the DNS zone for the DNS-01 challenge. Use an API token that is limited to the `_acme-challenge` records if the DNS provider supports this limit.

## 9. HTTP proxy

The HTTP proxy reads the TLS Server Name Indication field and extracts the instance name. The HTTP proxy then looks up the target address. Write the proxy in Go with `net/http/httputil.ReverseProxy`. A reverse proxy of this size is about 200 lines.

A request for the base domain goes to the control plane. The control plane serves the dashboard and the OIDC login flow.

Do not use an external reverse proxy for version 1. Bento must apply per-instance access control before it forwards a request. This logic is easier to write directly than to express in the configuration language of an external proxy.

Set these headers on each forwarded request:

- `X-Forwarded-Proto`
- `X-Forwarded-Host`
- `X-Forwarded-For`

### 9.1 Port selection

Each instance has one default HTTP port. The control plane sets this port at creation time. The default value is 80. A user changes the port with the `port` command.

The HTTP proxy also listens on ports 3000 to 9999. A request to `https://$NAME.bento.foid.space:3456/` goes to port 3456 on the instance. The wildcard certificate covers these ports, because the host name does not change.

Do not use a name prefix such as `3456-$NAME.bento.foid.space`. This form needs a certificate for `*.*.bento.foid.space`. Such a certificate does not exist.

### 9.2 Visibility

An instance has one of three visibility values. The default value is `off`.

| Value | Behavior of the HTTP proxy |
| --- | --- |
| `off` | Return HTTP 404 for the name. |
| `private` | Redirect an unauthenticated request to the login page of the dashboard. |
| `public` | Forward the request without authentication. |

The `off` value binds nothing to the name. A user who runs a database or a message broker in an instance needs this value. A login page in front of the port is not the same protection.

The HTTP 404 response for `off` matches the response for a name that does not exist. This hides the existence of the instance.

Ports 3000 to 9999 are always private. The `public` value applies only to the default HTTP port.

A user changes the value with the `visibility` command. A user grants access to a named second user with the `share` command. A share applies to a `private` instance.

## 10. SSH frontend

The user connects with `ssh $NAME@bento.foid.space`.

SSH has no Server Name Indication field. The server cannot read the requested host name from the connection. The instance name must therefore travel in the SSH user name field.

The SSH frontend does the following:

1. Accept the connection and read the client public key.
2. Look up the public key in the database to find the user.
3. Reject the connection if the key is unknown.
4. Read the SSH user name and treat it as the instance name.
5. Check that the user owns the instance or has a share for the instance.
6. Start the instance if the state is `stopped`.
7. Open a new SSH connection to the instance on the internal address.
8. Join the two connections.

Write the SSH frontend in Go with `gliderlabs/ssh`. This component is about 150 lines.

Step 6 makes a stopped instance feel persistent. The user connects and the instance answers. The user does not need a separate `start` command in the normal case.

The SSH frontend also answers `ssh bento.foid.space` with no user name. This session runs the command line interface. The user needs no client software, because the user already has an SSH client and an SSH key.

## 11. Instance lifecycle

An instance has one of these states: `starting`, `running`, `stopped`.

The user controls every state change. Bento changes the state only in response to a user action.

| Action | Result |
| --- | --- |
| `new` | Create the instance and set the state to `starting`. |
| `stop` | Stop the instance. Keep the disk and the data. |
| `start` | Start the instance. |
| SSH connection to a stopped instance | Start the instance, then connect. |
| `rm` | Delete the instance and the disk. |

A stopped instance uses disk but no memory and no CPU. Tell the user to stop an instance rather than delete it. This is the cheap action that does not lose work.

The `rm` command asks for confirmation. Accept a `--force` flag for scripts.

## 12. Data model

Use SQLite with write-ahead logging.

| Table | Columns |
| --- | --- |
| `users` | `id`, `name`, `email`, `incus_project`, `oidc_subject`, `created_at` |
| `ssh_keys` | `id`, `user_id`, `public_key`, `fingerprint`, `comment`, `created_at` |
| `instances` | `name`, `owner_id`, `type`, `image`, `state`, `internal_ip`, `http_port`, `visibility`, `created_at`, `last_seen_at` |
| `shares` | `instance_name`, `user_id`, `created_at` |
| `tokens` | `id`, `user_id`, `hash`, `expires_at` |

The `visibility` column holds `off`, `private`, or `public`.

The `last_seen_at` column records the time of the last SSH connection or HTTP request. Bento does not act on this column. The column exists so that a user can find a forgotten instance in the `ls` output.

Index `ssh_keys.fingerprint`, because the SSH frontend reads this column on every connection.

The `instances` table duplicates state that Incus also holds. Treat Incus as the authoritative source for the state and for the internal address. Reconcile the table from the Incus API at startup and every 30 seconds.

## 13. Identity

The dashboard uses OIDC. Pocket ID is a suitable provider.

The command line interface uses SSH public key authentication. A new user registers by connecting to `ssh bento.foid.space`. The registration flow records the presented public key and asks for a name and an email address.

The HTTP proxy needs a session for private instances. Issue a cookie for the base domain after an OIDC login. The cookie is valid for every subdomain, so one login covers every instance that the user can reach.

A token in the `tokens` table gives programmatic access. A script uses this token instead of an SSH key.

## 14. Command line interface

The interface runs over SSH. The form is `ssh bento.foid.space <command> [arguments]`. The dashboard exposes the same operations.

| Command | Action |
| --- | --- |
| `ls` | List the instances of the user. Show the state, the quota use, and the last use time. |
| `new <name>` | Create an instance. |
| `rm <name>` | Delete an instance. Ask for confirmation. |
| `start <name>` | Start a stopped instance. |
| `stop <name>` | Stop a running instance. |
| `restart <name>` | Restart an instance. |
| `rename <old> <new>` | Rename an instance. |
| `cp <source> <target>` | Copy an instance. |
| `resize <name>` | Change the memory, the vCPU count, or the disk size. |
| `port <name> <port>` | Set the default HTTP port. |
| `visibility <name> <off\|private\|public>` | Set the visibility value. |
| `share <name> <user>` | Grant or revoke access for a second user. |
| `ssh-key` | Add, list, or remove an SSH key. |
| `whoami` | Show the account and the quota of the user. |

The `new` command accepts `--type`, `--image`, `--memory`, `--cpu`, and `--disk`.

## 15. Build order

1. Write the control plane with the Incus client and the database. Test with the Incus command line tool.
2. Write the SSH frontend and the command line interface. The system is usable at this point without HTTP.
3. Get the wildcard certificate. Write the HTTP proxy with `off` and `public` instances only.
4. Add OIDC, sessions, the dashboard, and `private` instances.
5. Add projects, project limits, and network ACLs.

Do not ship to a second user before step 5. Until then, every instance can reach every other instance, and no quota applies.

The dashboard is at step 4 because it needs the OIDC session that `private` instances also need. Steps 1 to 3 give a usable system for the operator alone.

## 16. Open items

The behavior of the HTTP proxy during an instance restart is not defined. Options are to return HTTP 503, or to hold the request until the instance answers. Choose one. The choice matters more now, because section 10 starts a stopped instance on connection, and a start takes seconds.

Backup is not specified. Incus snapshots cover instance state. The SQLite database needs a separate backup.

IPv6 for instances is not specified. A separate public address for each instance would remove the need for the user name trick in the SSH frontend. This costs one address for each instance, and it removes the single point of control for SSH access.

An automatic stop is not in version 1. An automatic stop frees memory and CPU without loss of data, so it does not break the rule in section 3. The same idle detection problem applies, but the cost of a wrong decision is a slow reconnect rather than lost work. Consider this feature after the disk quota proves too coarse.
