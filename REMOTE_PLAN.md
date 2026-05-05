# Remote Clust — Implementation Strategy

This document proposes a strategy for letting the `clust` CLI connect to a `clust-hub` running on **another machine** (typically the user's home desktop). The goal is to be able to start an agent or a batch from a laptop, close the lid, and have everything keep running on the remote machine.

It is a planning document only. No code is written here — see the **Phased Implementation** section for the build order.

---

## 1. Goals & Non‑Goals

### Goals

1. **Persistent execution.** Agents and batches keep running on the remote hub when the local CLI/laptop disconnects, sleeps, or is shut down.
2. **Multi‑hub usage from a single CLI.** A user can switch between a local hub and one (or more) remote hubs from the same `clust` binary.
3. **Strong default security** for a residential setup:
   - No inbound ports opened on the home router.
   - No services published to the public internet.
   - End‑to‑end encryption of every byte the CLI exchanges with a remote hub.
   - Mutual authentication: the hub only accepts paired clients, and the client only trusts paired hubs.
4. **Low‑friction pairing.** Adding a new client device should be a one‑command flow that completes in seconds.
5. **Streaming parity.** Live PTY streaming (attach, focus mode, terminal sessions) must work over the remote transport with reconnect/replay semantics that are at least as robust as today's local streaming.
6. **Backward compatibility** with today's local‑only workflow. Running `clust` without remote configuration must behave exactly as it does in v0.0.22.

### Non‑Goals (initial release)

- **Remote filesystem mirroring.** We do not try to make `pwd` on the laptop look like a path on the desktop. Operations on remote hubs are scoped to **registered repos** that exist on the remote machine.
- **Multi‑user / shared hubs.** A hub serves a single user. We do not implement role‑based access, audit logs for multiple users, or quota systems.
- **Public exposure on a domain name.** We explicitly avoid this in v1. A user who wants it can layer it on with a reverse proxy themselves.
- **Cross‑hub operations** (e.g., move an agent from local to remote). Out of scope.
- **Mobile clients / web UI.** CLI + TUI only.

---

## 2. High‑Level Architecture

### Today

```
┌─────────────────────────────────────────────┐
│              User's Laptop                   │
│                                              │
│  clust-cli  ──► ~/.clust/clust.sock ──► clust-hub
│                  (Unix Domain Socket)         │
└─────────────────────────────────────────────┘
```

### Target

```
┌────────────────────────────┐         ┌─────────────────────────────┐
│       Laptop (client)      │         │    Home Desktop (server)    │
│                            │         │                             │
│  clust-cli                 │         │  clust-hub --serve          │
│   │                        │         │   ▲                         │
│   │ profile=home           │         │   │ Unix socket             │
│   ▼                        │         │   │ (local CLI access)      │
│   transport router         │         │   │                         │
│   ├─► Unix socket (local)  │         │   ├─ TLS+token listener     │
│   └─► TLS over secure WAN ─┼────────►│   │  on tailnet/VPN address │
│                            │         │   │                         │
│                            │         │   ▼                         │
│                            │         │   Agents (PTYs)             │
│                            │         │   Batch engine (SQLite)     │
└────────────────────────────┘         └─────────────────────────────┘
        │                                      │
        └──── secure overlay (Tailscale, ──────┘
              WireGuard, or Cloudflare Tunnel)
```

Two layers of defense:

1. **Network layer** — the byte stream travels over a private overlay (Tailscale / WireGuard / Cloudflare Tunnel). The home router has **no inbound port open to the public internet**.
2. **Application layer** — even on the overlay, `clust-hub` only accepts paired clients via mTLS or a pre‑shared token over TLS with cert pinning. A compromised peer on the same overlay still cannot drive the hub.

---

## 3. Threat Model

| Adversary | Reachable surface | Mitigation |
|---|---|---|
| Random internet attacker | None (no inbound ports, no public DNS) | Network‑layer overlay; nothing is published. |
| Attacker who phishes the user's Tailscale/CF account | Hub TCP port on overlay | Application‑layer mTLS pinning + token; lost tokens rotate by re‑pairing. |
| Roommate on home LAN | Hub bound to overlay IP only, **not** `0.0.0.0` | Bind address restricted by default; explicit opt‑in to LAN. |
| Stolen laptop | Stored token + client cert | Tokens are revocable at the hub (`clust-hub revoke <client>`); client cert stored in OS keychain when possible. |
| Hostile process on the hub machine | Hub's Unix socket + SQLite | Same as today: filesystem permissions on `~/.clust/`. |
| MITM on overlay | Cipher downgrade, replay | TLS 1.3 only; cert fingerprint pinning verified by client. |

What we explicitly do **not** defend against:

- Code execution on the desktop itself (the hub trivially gives the operator agent capabilities, by design).
- Side‑channel timing attacks against TLS.
- The `--dangerously-skip-permissions` agent flag — that is an existing, documented foot‑gun.

---

## 4. Network Transport — Recommended Approach

The connection must traverse the public internet from a coffee shop to the user's home, but we want zero open ports at home. Three viable options, ranked by recommendation:

### 4.1 Tailscale (recommended default)

- The user installs Tailscale on both laptop and desktop and signs in.
- Each device gets a stable `100.x.x.x` (or MagicDNS name like `desktop.tail-xxxx.ts.net`).
- `clust-hub --serve` binds to the Tailscale interface only (e.g., `--bind 100.0.0.0/8:7464`).
- Connections are end‑to‑end WireGuard between the two devices; Tailscale's coordination server only sees metadata (no traffic).
- Tailscale ACLs let the user explicitly restrict which devices can even reach the hub port.

**Why this is the default recommendation:**

- Zero inbound ports on the home router (NAT traversal is solved by Tailscale's relay/STUN).
- Free for personal use up to 100 devices.
- Audited, well‑maintained, with no unusual cryptographic claims.
- Works through carrier‑grade NAT, which a self‑hosted WireGuard often does not.

**Documentation deliverable:** A short "Set up Tailscale for clust" section in `docs/remote.md` with the exact two commands the user runs on each device.

### 4.2 Cloudflare Tunnel + Cloudflare Access

- The desktop runs `cloudflared tunnel` outbound; no inbound ports.
- The laptop reaches a hostname like `clust.example.com` which Cloudflare gates with Cloudflare Access (Google/GitHub OAuth).
- Cloudflare proxies bytes to the local `clust-hub` listener.

**Tradeoffs vs Tailscale:**

- Cloudflare can see plaintext bytes between the edge and your origin if you terminate TLS at the edge. We mitigate by **not** terminating TLS at the edge: the hub presents its own TLS cert, Cloudflare proxies TCP only, the client pins the hub's fingerprint. Cloudflare still sees encrypted flow metadata.
- Useful if the user already has Cloudflare for their domain and prefers OAuth identity over Tailscale device identity.

### 4.3 Self‑hosted WireGuard

- Same security properties as Tailscale, but the user runs their own WireGuard server.
- Requires a UDP port forwarded on the home router (the only setup that does), and manual peer config.
- Recommended only for users who already have WireGuard infrastructure.

### 4.4 Explicitly **not** recommended for v1

- **Direct TCP+TLS exposure** with a port forwarded on the home router. Even with mTLS, this puts an authenticated‑listener on the public internet, and any future protocol bug is now globally reachable. We will not document this path.
- **SSH port forwarding** as the primary transport. It works for ad‑hoc tinkering, but tying long‑lived agent streams to an SSH session creates flaky reconnect behavior that conflicts with goal #5.

### 4.5 What `clust` itself ships

`clust` is **transport‑agnostic**. It ships a TLS+token listener that binds to a configurable address. Whether that address is reached over Tailscale, Cloudflare, or WireGuard is the user's choice. Documentation guides them to Tailscale by default.

---

## 5. Application‑Layer Security

Even on a private overlay, the hub authenticates every connection. This is defense in depth.

### 5.1 Transport: TLS 1.3, mutual auth

- On first `clust-hub --serve`, the hub generates a self‑signed server cert (Ed25519 or ECDSA P‑256) into `~/.clust/server.crt` + `~/.clust/server.key` (mode 0600).
- Each paired client gets a **client certificate** signed by a tiny per‑hub CA, also generated on first serve.
- Both sides require TLS 1.3, no fallback. Cipher suites limited to AEAD (AES‑GCM, ChaCha20‑Poly1305).

### 5.2 Pairing flow (one‑shot, no copy‑paste of secrets in plaintext where possible)

On the **server**:

```
$ clust-hub pair --name "laptop"
clust pair token (valid for 10 minutes, single use):
   clust://pair/v1?host=desktop.tail-xxxx.ts.net&port=7464&fp=sha256:ab12...&t=...
```

The token encodes:

- Hub host:port (the address the client should dial)
- Server cert fingerprint (so the client can pin it)
- A 256‑bit one‑time pairing nonce (HMACed by a server secret)
- A short label for the client ("laptop")

On the **client**:

```
$ clust connect "clust://pair/v1?...."
Connecting to desktop.tail-xxxx.ts.net:7464...
Server fingerprint matches: sha256:ab12...
Generated client key in ~/.clust/clients/home.key
Received client certificate signed by hub.
Saved profile: home (set as default)
```

The pairing exchange:

1. Client TLS‑connects, validates server fingerprint.
2. Client sends `Pair { nonce, label, csr }` (CSR = client's freshly generated public key).
3. Server validates the nonce (single use, time‑bound), signs the CSR, stores the client record in SQLite, returns `Paired { client_cert, client_id }`.
4. After this exchange the nonce is burned. All future connections use the issued cert.

This means **no long‑lived shared secrets are ever copy‑pasted**. The pairing token is single‑use and short‑lived; after pairing, auth is via mTLS only.

### 5.3 Revocation

- Each client cert has a short serial recorded in SQLite (`paired_clients` table).
- `clust-hub clients ls` lists paired clients with last‑seen timestamps.
- `clust-hub clients revoke <id>` removes the row; the hub maintains a revocation set in memory and rejects revoked certs on TLS handshake.
- A lost laptop ⇒ revoke + re‑pair on the next device. Other devices keep working.

### 5.4 Bind address defaults

- Default bind is `127.0.0.1:7464` (loopback only). The hub refuses to start with that bind in `--serve` mode unless `--allow-loopback` is passed, because loopback‑only is the wrong default for a network‑facing daemon — but we want the failure mode to be a clear error, not "silently listening on all interfaces."
- Recommended bind: the Tailscale interface address (we'll show how to discover it in the docs).
- Explicit opt‑in to `0.0.0.0` requires `--bind 0.0.0.0:PORT --i-understand-this-is-public`. Documenting the foot‑gun via a long flag is a deliberate friction point.

### 5.5 Secrets at rest

- Server private key: `~/.clust/server.key`, mode 0600.
- Hub CA private key: `~/.clust/ca.key`, mode 0600.
- Client private key (on the laptop): `~/.clust/clients/<profile>.key`, mode 0600. On macOS we additionally try the keychain (best effort, fall back to file).
- Pairing nonces: never written to disk, only held in memory until burned.

---

## 6. Protocol Changes

### 6.1 Transport abstraction

Currently `clust-ipc` opens a `tokio::net::UnixStream`. We replace the concrete socket with a transport trait:

```
trait HubTransport: AsyncRead + AsyncWrite + Send + Unpin { ... }
```

Two implementations:

- `LocalTransport` → `UnixStream` (today's behavior).
- `RemoteTransport` → `tokio_rustls::client::TlsStream<TcpStream>` with cert pinning.

Both implementations carry the same length‑prefixed MessagePack framing on top, so `CliMessage` / `HubMessage` definitions are **unchanged**.

### 6.2 Handshake

Independent of transport, every connection now begins with:

```
1. (TLS layer establishes; client verifies pinned fingerprint; server verifies client cert.)
2. CLI sends Hello { protocol_version, client_id, client_label }.
3. Hub replies HelloAck { protocol_version, hub_version, hub_capabilities }.
   - On version mismatch with a *local* hub, behavior is unchanged: stop and respawn.
   - On version mismatch with a *remote* hub, the CLI shows a clear "remote hub is older/newer than your CLI" message and does not attempt to restart the remote hub.
4. CLI proceeds with command messages as today.
```

`Ping`/`Pong` is renamed to `Hello`/`HelloAck` to make it clear this is an authenticated session bring‑up, not a liveness ping. (Liveness can be added later as a separate `Keepalive` message.) `PROTOCOL_VERSION` is bumped.

### 6.3 Streaming reconnect & replay

This is the most user‑visible robustness change. Today, if the CLI's TCP connection drops mid‑attach, the user is back at a prompt and has to re‑attach. Over the public internet, drops are common (laptop suspend, NAT rebinding, Wi‑Fi changes).

We add a **session token** layer above MessagePack:

- When a CLI begins an attached session (`AttachAgent` / `StartTerminal`), the hub returns a `SessionToken { id, last_seq }`.
- Output frames carry a monotonically increasing sequence number per session.
- On reconnect, the CLI sends `Resume { session_token, last_seq_received }`. The hub re‑sends any frames the client missed from the replay buffer, then resumes live streaming.
- If too much was missed (replay buffer overflow), the hub responds `ReplayLost { session_token }` and the client falls back to a fresh `AttachAgent`.

This change benefits local users too — it makes brief Unix socket hiccups invisible.

### 6.4 No path‑sensitive defaults over remote

Several existing messages carry `working_dir`, which is interpreted server‑side. To keep this safe and unambiguous on a remote hub:

- The CLI knows whether its current profile is `local` or `remote`.
- For a **remote** profile, messages that take `working_dir` from the local `pwd` are **rejected client‑side** with a clear error: *"This command needs an explicit repo (use `-r <name>`) when targeting a remote hub."*
- Messages that already accept `repo_name` instead of `working_dir` are unaffected.
- `clust ui` works as today: it shows the **remote**'s registered repos and worktrees, because the TUI's data already comes from the hub's view of disk.

This is intentionally restrictive in v1. Loosening it (e.g., a `clust mount` that pretends a remote repo path is local) is a future feature.

### 6.5 Tray icon

The macOS tray icon is a property of the host the hub runs on. A remote `clust-hub --serve` running on a desktop continues to show its own tray on the desktop. The CLI on the laptop never communicates about tray state. No change required.

---

## 7. Connection Profiles in the CLI

The CLI gains a profiles concept similar to AWS CLI profiles or kubectl contexts.

**File:** `~/.clust/profiles.toml`

```
default = "home"

[profiles.local]
type = "local"
# (no other fields — uses ~/.clust/clust.sock)

[profiles.home]
type = "remote"
endpoint = "desktop.tail-xxxx.ts.net:7464"
fingerprint = "sha256:ab12...ef"
client_cert = "~/.clust/clients/home.crt"
client_key  = "~/.clust/clients/home.key"
ca_cert     = "~/.clust/clients/home.ca.crt"
label       = "laptop"
```

Profile selection precedence (highest first):

1. `--profile <name>` flag on any command.
2. `CLUST_PROFILE` environment variable.
3. `default = ` line in `profiles.toml`.
4. Fallback: implicit `local` profile.

### New CLI subcommands

| Command | Behavior |
|---|---|
| `clust connect <pair-url>` | Run pairing flow, write the new profile. |
| `clust profiles ls` | List configured profiles, marking the default. |
| `clust profiles use <name>` | Set the default profile. |
| `clust profiles rm <name>` | Forget a profile (and its key material). |
| `clust profiles current` | Print the active profile name. |

### New hub subcommands (run on the server)

| Command | Behavior |
|---|---|
| `clust-hub --serve --bind <addr>` | Run as a foreground server. |
| `clust-hub pair --name <label>` | Mint a single‑use pairing URL. |
| `clust-hub clients ls` | List paired clients. |
| `clust-hub clients revoke <id>` | Revoke a client cert. |

### Service supervision (server)

The hub already daemonizes itself. To keep it alive across reboots on the home machine, we provide:

- A `homebrew services` recipe (already used by users on macOS) wrapping `clust-hub --serve --bind <tailscale_ip>:7464`.
- A `systemd` unit file template in `docs/remote.md` for Linux desktops.

These are **documentation deliverables**, not code in the binary itself.

---

## 8. Storage Implications

The hub's SQLite already holds enough state to survive restarts (config, repos, queued/idle/scheduled/running batches). New tables for remote support:

```
CREATE TABLE paired_clients (
    id           TEXT PRIMARY KEY,         -- short hex
    label        TEXT NOT NULL,            -- human label like "laptop"
    cert_pem     TEXT NOT NULL,            -- issued client cert
    cert_serial  BLOB NOT NULL UNIQUE,
    paired_at    TEXT NOT NULL,            -- RFC 3339
    last_seen_at TEXT,                     -- RFC 3339
    revoked_at   TEXT                      -- RFC 3339; non-null = revoked
);

CREATE TABLE pairing_nonces (
    nonce_hash   BLOB PRIMARY KEY,         -- HMAC of nonce; never store the raw nonce
    label        TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    consumed_at  TEXT                      -- non-null = burned
);
```

Schema migrations follow the existing version‑table pattern (next free version: v10 / v11).

The CLI side stores no SQLite — only `profiles.toml` and key files in `~/.clust/clients/`.

---

## 9. CLI UX When Remote Is Active

A small but important detail: when the user's default profile is remote, the bottom status bar in attached mode and in `clust ui` should show that — they're acting on a different machine. Mistakes are cheap when you're sure which hub you're driving.

Concretely:

- The status bar gains a profile chip: `[home]` on the right side.
- `clust ls` includes a header line: `Hub: home (desktop.tail-xxxx.ts.net) — connected as laptop`.
- Destructive operations against remote profiles get an extra confirmation prompt the first time they're used in a session (e.g., `clust -s` to stop the entire remote hub).

Color usage follows `docs/theme.md` conventions (Graphite palette).

---

## 10. Resilience & Reconnection (laptop close / suspend / network change)

The user's main scenario: kick off a batch, close the laptop, walk away.

What happens today (locally) is fine because the hub is a separate process. The new behaviors needed for remote:

1. **Disconnects don't kill agents.** Already true — the hub owns the PTY.
2. **Re‑attach after reconnect** uses the session token from §6.3 to resume streaming without losing buffered output.
3. **Background submission.** Existing `clust -b` returns immediately after the agent is started. With remote profiles, `-b` becomes the recommended way to fire-and-forget. The CLI prints the agent ID and the profile, so the user can check status later from anywhere.
4. **Heartbeat.** A 30‑second `Keepalive` from CLI to hub on attached sessions; if 3 are missed, the CLI cleanly tears down its side and tells the user, without affecting the hub.

We do **not** change the hub's agent‑death detection — that's PTY‑driven and unchanged.

---

## 11. Phased Implementation

A workable build order, assuming each phase ships behind feature gates and is testable on its own:

### Phase 0 — Audit & invariants (no user‑visible change)

- Move the IPC connection setup into a `HubTransport` trait. Today it has only the Unix‑socket implementation. Confirm everything compiles and behaves identically.
- Refactor protocol bring‑up so the existing `Ping`/`Pong` fits the new `Hello`/`HelloAck` shape (the wire bytes can be identical for now).

### Phase 1 — Server mode + pairing (no remote client yet)

- `clust-hub --serve` flag, with `--bind` and the safety checks from §5.4.
- Self‑signed cert + per‑hub CA generation on first `--serve`.
- `clust-hub pair` subcommand that prints the `clust://pair/v1?...` URL.
- `paired_clients` and `pairing_nonces` tables (migration v10).
- Unit + integration tests using a localhost loopback bind, no real overlay required.

### Phase 2 — Remote client + profiles

- `clust connect <pair-url>` subcommand that walks the pairing protocol end‑to‑end and writes `profiles.toml`.
- `--profile` flag, `CLUST_PROFILE` env var, `clust profiles ...` subcommands.
- `clust ls` and other one‑shot commands work end‑to‑end against a remote hub bound to loopback.

### Phase 3 — Streaming over remote

- Implement the session token / sequence number layer.
- `clust -a <id>`, `clust -b ...`, focus mode, terminal sessions, `clust ui` all work over remote.
- Reconnect on TCP drop is invisible up to the replay buffer size; beyond that, falls back to a fresh attach with a clear message.

### Phase 4 — Operational polish

- `clust-hub clients ls` / `revoke`.
- Profile chip in TUI status bar; first‑use destructive‑operation confirmation.
- Documentation: `docs/remote.md` covering Tailscale setup, Cloudflare Tunnel alternative, and a homebrew‑services / systemd‑unit recipe.

### Phase 5 (deferred, not v1)

- Browser/web client.
- Multi‑user hubs with role‑based access.
- Local‑path mounting (making a remote repo appear at a local path for `pwd`‑sensitive flows).

---

## 12. Risks & Open Questions

| Risk | Mitigation |
|---|---|
| Self‑signed cert handling in `tokio-rustls` is finicky; it's easy to ship something that silently accepts any cert. | Pin by SPKI fingerprint, not by full cert. Add a deliberate negative test that swaps the server cert and expects failure. Code review every TLS config builder call. |
| Pairing nonces leaked to clipboard / shoulder‑surfed. | Single‑use, 10‑minute TTL, hub‑side HMAC so the token is useless without server state. After pairing, future auth is cert‑based. |
| Users binding to `0.0.0.0` because Tailscale "is too much work." | The `--i-understand-this-is-public` flag is intentional friction; the docs put Tailscale first because it really is the path of least resistance. |
| `working_dir` confusion on remote profiles. | Hard client‑side block in the CLI when running a `working_dir`‑sensitive command against a remote profile (§6.4). Error message tells the user which command to use instead. |
| Replay buffer size (512 KB) is too small over flaky links. | Make replay buffer size configurable in the hub; document the tradeoff between memory use and reconnect grace. Consider per‑session disk spill for very long batches. |
| Time skew between client and server affects pairing nonce TTL and TLS cert validity. | Use monotonic time on the server side for nonce TTL, not wall‑clock. Cert NotBefore/NotAfter set with generous skew tolerance. |

### Open questions to confirm before Phase 1

1. **Cert lifetime.** Do we want client certs to expire (forcing re‑pair every 90 days) or be long‑lived until revoked? Long‑lived + revocable is simpler; expiring is more "best practice." Lean toward long‑lived for v1.
2. **Default port.** Suggest **`7464`** (the digits look vaguely like `clust` on a phone keypad). Need to make sure it doesn't conflict with anything common in the Tailscale ecosystem.
3. **Should `clust-hub --serve` also keep the Unix socket open?** Recommendation: **yes**, for the user's local CLI on the desktop. They can still type `clust ls` on the desktop directly.
4. **Discovery / mDNS for LAN** — explicitly out of scope. We don't want to surface hub presence on a network the user shares with anyone else.

---

## 13. Summary

The recommended path is:

1. **Run `clust-hub --serve` on the home desktop**, bound to its Tailscale interface.
2. **Pair the laptop** with a one‑shot `clust connect clust://pair/v1?...` URL minted by the hub.
3. **Use `clust --profile home ...`** (or set it as the default) from anywhere; agents and batches survive disconnects because they live on the desktop and the hub already persists state across restarts.

Security rests on two layers — a private overlay (Tailscale) so the home router exposes nothing publicly, and TLS+mTLS with cert pinning at the application layer so a compromised peer on the overlay still can't drive the hub. Pairing uses single‑use, time‑bound nonces; long‑term auth is cert‑based and revocable per device.

Implementation is phased so each step ships independently, with the local‑only behavior remaining untouched until the user opts into a remote profile.
