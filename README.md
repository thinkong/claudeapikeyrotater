# claude-key-router

A transparent proxy for the Claude API, written in Rust, with a web UI for managing it. Clients use it exactly like `https://api.anthropic.com`. Each request goes out on one Claude API key from your pool. Along the way the proxy:

- forwards **every** `/v1/*` endpoint: Messages, streaming, token counting, Batches, Files, Models, Skills, Managed Agents, and any beta. Request bodies are passed through byte-for-byte; only the API key is swapped.
- **meters cost** for every request (shown in the UI, and per request via a header or lookup).
- **benches a key** for a configurable time when it runs out of credits, and fails the request over to another key.
- keeps a **running estimate of remaining credits** per key.
- **routes each conversation to the same key**, so the client's own prompt caching keeps hitting.
- has a **web UI** where you add, remove, replace and refresh upstream keys, and create, rotate, budget and revoke client keys.

## Quick start

```bash
cargo build --release
```

```bash
cp config.example.toml config.toml
```

```bash
./target/release/claude-key-router config.toml
```

Open **http://127.0.0.1:8787/ui**. On first start the proxy generates an admin token and prints it to the log once. Set `admin_token` in the config to choose your own instead. Then:

1. **Upstream keys → Add & check.** Paste each Claude API key. Optionally enter its current credit balance.
2. **Client keys → Create key.** One per app or person. The token is shown once, along with ready-to-paste env vars.

Point any client at the proxy and use its client key as the API key:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8787 ANTHROPIC_API_KEY=sk-ckr-...   # Claude Code, SDKs, curl...
```

```python
import anthropic
client = anthropic.Anthropic(base_url="http://127.0.0.1:8787", api_key="sk-ckr-...")
```

The proxy never forwards client keys upstream. It swaps in a pool key.

## Docker

```bash
docker compose up -d --build
```

```bash
docker compose logs claude-key-router
```

The first start prints the generated admin token in those logs. To choose your own, set `CKR_ADMIN_TOKEN` (in the environment or a `.env` file) before the first start. Then open http://127.0.0.1:8787/ui.

Without compose:

```bash
docker build -t claude-key-router .
```

```bash
docker run -d --name claude-key-router -p 127.0.0.1:8787:8787 -v ckr-data:/data claude-key-router
```

About the image:

- **Size and contents:** about 58 MB. It's built on distroless (glibc and CA certificates only), with no shell.
- **User:** runs as non-root (uid 65532).
- **Health check:** has one built in, which runs `claude-key-router healthcheck`.
- **State:** everything lives in the `/data` volume: keys, client-token hashes and usage history. Back that volume up and keep it private. If you bind-mount a host folder instead of using a named volume, that folder must be writable by uid 65532.
- **Config:** no config file is needed. These environment variables override the defaults (and the config file, if there is one):

| Variable | Default in the image |
|---|---|
| `CKR_LISTEN` | `0.0.0.0:8787` |
| `CKR_DATABASE` | `/data/claude-key-router.db` |
| `CKR_ADMIN_TOKEN` | generated on first start |
| `CKR_PUBLIC_URL` | `http://<Host header>` |
| `CKR_UPSTREAM` | `https://api.anthropic.com` |

To tune cooldowns or routing, mount a config file at `/data/config.toml`. It's picked up automatically. To use a file elsewhere, pass its path as the container's argument.

## Releases (GitHub Actions)

[`.github/workflows/release.yml`](.github/workflows/release.yml) runs on every push to `main` or `master`. Pushes that only touch `*.md` files are skipped. Each run:

1. Runs the unit tests and the end-to-end suite. A failure stops the release, and nothing gets bumped.
2. Bumps the patch version in `Cargo.toml` and `Cargo.lock`, using [`scripts/bump_version.py`](scripts/bump_version.py).
3. Commits `Release vX.Y.Z [skip ci]`, tags it `vX.Y.Z`, and pushes both together. The `[skip ci]` marker stops that commit from starting another release.
4. Builds the image and pushes it to `ghcr.io/<owner>/<repo>`, tagged `X.Y.Z`, `X.Y`, `latest` and `sha-<commit>`.
5. Publishes a GitHub Release with auto-generated notes and a `linux-x86_64` archive, plus its `SHA256SUMS`. The archive holds the binary, the README and the example config. The binary needs glibc 2.41+ (Debian 13 / Ubuntu 25.04 or newer); on anything older, use the image.

To bump the minor or major version instead, go to **Actions → Release → Run workflow** and pick the part.

Things to know:

- **Pull before you push again.** The version commit is made by the workflow, so your local branch is one commit behind after each release.
- **Branch protection.** If the branch requires pull requests, the workflow can't push its version commit. Allow GitHub Actions to bypass that rule, or turn the rule off.
- **Image visibility.** The first image push creates the GHCR package as private. To let others `docker pull` without logging in, make it public under the package's settings.
- **Re-running a failure.** If the image or release step fails, use **Re-run failed jobs**. It reuses the version that was already tagged instead of bumping again.

## The web UI

| Tab | What you can do |
|---|---|
| **Upstream keys** | Add a key (checked immediately). See each key's state: available, cooling down (rate limit), out of credits (with time until retry), invalid, or disabled. See estimated credit left, spend and last use. |
| **Client keys** | Create a key (shown once, only its hash is stored). See spend vs. budget. |
| **Activity** | Last 30 days by model, including cache-hit rate and cost. Recent requests with key, status, tokens and cost. |

Row buttons on **Upstream keys**:

- **Refresh** clears the key's cooldown and sends a 1-output-token request on `check_model` (default Claude Haiku 5.5, a tiny fraction of a cent). This is the only reliable way to tell whether a key still has credit: a model listing succeeds even with an empty balance. The result appears as a notification and updates the key's state.
- **Balance** records a known balance, for example after a top-up. A positive balance also lifts an out-of-credits cooldown.
- **Replace** swaps in a new secret under the same name. History is kept, and the new secret is checked right away.
- **Disable**, **Remove**.

Row buttons on **Client keys**: **New token** (rotate; the old token stops working immediately), **Budget**, **Disable**, **Remove**.

Changes take effect immediately, without a restart. The page refreshes every 5 seconds.

Everything the UI does goes through a JSON admin API, so you can script it too (see below).

## How it works

### Choosing a key

Routing is sticky because of two constraints:

1. **Prompt caches are per workspace.** If one conversation's turns spread across keys from different organizations, almost every turn is a cache miss.
2. **Resources belong to the organization that created them.** A `file_id`, batch, agent, session, skill or code-execution container only exists for the key that created it.

So the proxy:

- **Pins resources.** When a `POST` creates something with an `id`, or a Messages response returns a `container.id`, the id is pinned to that key. Any later request with that id in its path or JSON body goes to the same key. If that key has since been removed, the request gets a clear 404.
- **Hashes everything else onto available keys** with rendezvous hashing, set by `routing.affinity`:
  - `conversation` (default): model + tools + system + first message. Each conversation stays on one key, and different conversations spread across the pool.
  - `prefix`: model + tools + system.
  - `client`: each client key sticks to one upstream key.

  Adding or removing a key only moves that key's share of traffic.
- **Stickiness never beats health.** Only available keys are candidates. If a conversation's key gets benched (out of credits, rate limited, invalid, disabled), it fails over:
  - **The turn that hits the error:** the same request is retried on the next key, and the client still gets a normal response.
  - **Later turns:** they go straight to that next key, which is also deterministic, so the conversation stays together there.
  - **When the original key comes back** (cooldown over, or **Refresh**): the conversation moves back to it.
  - **The cost of each move:** the first turn on the new key is a prompt-cache miss, because caches don't cross organizations.
- **Lets you override per request:**
  - `x-proxy-affinity: <anything>` sets the sticky routing key.
  - `x-proxy-key: <key name>` forces a specific upstream key. Use it for listings like `GET /v1/files`, which only show one organization's resources.

### Failover and cooldowns

Request bodies up to `max_buffer_bytes` are buffered, so when a key fails the proxy can retry the same request on another key. This happens before any byte reaches the client.

| Upstream answer | What the proxy does |
|---|---|
| `402 billing_error`, or `400` with "credit balance is too low" | Marks the key **out of credits**, skips it for `credit_exhausted_secs` (default 1 h), retries on another key |
| `401` | Marks the key invalid, skips it for `auth_failure_secs`, retries on another key |
| `429` | Skips the key for `retry-after` seconds, retries on another key |
| `403`, `5xx`, `529` | Retries on another key (optionally benching it for `overloaded_secs`) |
| any other `4xx` | Returns it to the client (it's the request's fault) |

When a cooldown ends, the key gets traffic again. If it is still empty, it is benched again. **Refresh** in the UI checks a key right away instead of waiting. Only a successful *billable* call (Messages, batch creation) clears the out-of-credits flag, so a file download on an empty key doesn't un-bench it. Pinned requests and large streamed uploads can't move keys, so they get one attempt.

If no key is available, the proxy answers in the API's own error format, always with `retry-after`, so SDKs back off correctly:

| Situation | Answer |
|---|---|
| Every key is out of credits | `402 billing_error` |
| Keys are rate limited or invalid | `429 rate_limit_error` |
| No keys at all | `503` |

All state (keys, client hashes, cooldowns, balances, pins, usage) lives in SQLite and survives restarts.

### Cost tracking

Cost is computed from the response's `usage` and its `model` (the model that actually ran). It accounts for:

- uncached input
- 5-minute and 1-hour cache writes
- cache reads (per-model multiplier: 0.1×, 0.05× for Opus/Sonnet 5.5, 0.025× for Fable 5.1)
- output
- fast mode
- `inference_geo: "us"` (×1.1)
- Haiku 5.5's pricing above 100K prompt tokens
- web search ($10 / 1K)
- Batch results (×0.5)

Prices are in [`pricing.toml`](pricing.toml), compiled in. To change them without rebuilding, set `pricing_file`. An unknown model is recorded as **unpriced**, never as $0.

Response bodies are never modified. Clients can see the cost in two ways:

| Response | Where the cost is |
|---|---|
| Non-streaming Messages | `x-proxy-cost-usd` response header |
| Anything, including streams and batches | `x-proxy-request-id` header → `GET /proxy/requests/{id}` (a client can read its own rows; usage-summary totals are at `GET /proxy/usage`) |

A stream's headers are sent before its usage is known, so a stream's cost is only available through the lookup, once the stream has finished. Batch items are metered at the 50% batch rate when their results are downloaded, once per item.

If a client disconnects mid-stream, the tokens billed so far are still recorded.

Not metered: code-execution container hours, Priority Tier commitments, and Managed Agents session runtime. Their requests still pass through and are logged.

### Remaining credits

The Claude API has no endpoint that returns a key's credit balance. So the UI shows an **estimate** per key:

```
estimated credit left = balance you entered − spend this proxy has seen on that key since
```

Things to know:

- The estimate only knows about traffic through this proxy. Usage elsewhere (the Console, other apps) makes the real balance lower.
- Batch spend is counted when results are downloaded through the proxy.
- What actually benches a key is the out-of-credits error from the API, never the estimate.
- Credits belong to the Claude organization. If two of your keys are from the **same** organization, both run dry together. Each is benched the first time it hits the error, so expect one failed-over attempt per key.

### What the proxy changes

**Requests:** only the credentials. The client's key is replaced with a pool key, and any `oauth-*` beta flag is dropped, because upstream gets an API key. The body is forwarded byte-for-byte. The proxy reads it only for routing hints: model, `stream`, and resource ids.

**Responses:** passed through as-is, with two exceptions:
- `x-proxy-*` headers are added (`x-proxy-request-id`, `x-proxy-upstream-key`, `x-proxy-attempts`, `x-proxy-cost-usd`).
- Batch objects get their `results_url` rewritten to point back at the proxy (see below).

**Prompt caching** is whatever the client sends (`cache_control`). The proxy doesn't add any. Caches are per organization, so sticky routing is what keeps a conversation's cache hits working across a pool. The cache-hit column in **Activity** shows the result.

### Batches and `results_url`

SDKs download batch results from the batch's absolute `results_url`. The proxy rewrites that URL to point back at itself, so the download uses the right pool key and gets metered. Set `public_url` if clients reach the proxy under a different address than the `Host` header shows.

## Admin API

The UI uses these endpoints. They require `x-api-key: <admin token>`.

| Endpoint | |
|---|---|
| `GET /proxy/status` | Keys, clients, settings summary |
| `GET /proxy/admin/keys` | Upstream keys (secrets are never returned, only a 4-character hint) |
| `POST /proxy/admin/keys` `{"name", "key", "balance_usd"?, "check"?}` | Add a key, and by default check it |
| `PATCH /proxy/admin/keys/{name}` `{"key"?, "enabled"?, "balance_usd"?}` | Replace secret / enable / disable / record balance (`null` clears it) |
| `POST /proxy/admin/keys/{name}/refresh` | Clear cooldown and run the 1-token check |
| `DELETE /proxy/admin/keys/{name}` | Remove |
| `GET /proxy/admin/clients` | Client keys (hints only) |
| `POST /proxy/admin/clients` `{"name", "budget_usd"?}` | Create. The response contains the token, once. |
| `PATCH /proxy/admin/clients/{name}` `{"enabled"?, "budget_usd"?}` | Enable / disable / set budget (`null` = no limit) |
| `POST /proxy/admin/clients/{name}/rotate` | New token. The old one stops working. |
| `DELETE /proxy/admin/clients/{name}` | Remove |
| `GET /proxy/admin/requests?limit=50` | Recent requests |
| `GET /proxy/usage?group_by=key\|client\|model\|day&since=<unix>` | Token and cost totals. Clients see their own. |
| `GET /proxy/requests/{id}` | One request. Clients see their own. |
| `GET /proxy/me` | The calling client's spend and budget |

## Config file

Everything in [`config.example.toml`](config.example.toml) is optional: listen address, cooldown lengths, routing, timeouts. `[[keys]]`, `keys_file` and `[[clients]]` still work as **seeds**. They are imported into the database the first time the proxy sees each name, and after that the UI is the source of truth.

## Testing

```bash
cargo test
```

```bash
pip install anthropic httpx
```

```bash
python tests/e2e/run.py target/debug/claude-key-router
```

The end-to-end test starts a mock Claude API ([`tests/e2e/mock_upstream.py`](tests/e2e/mock_upstream.py)) and the real proxy binary. It drives them with the official `anthropic` SDK, and drives the admin API the UI uses. It covers:

- key add, check, replace, disable, remove and refresh
- client create, rotate, budget, disable and remove
- credit exhaustion in both the 400 and 402 shapes
- 429 cooldown
- cost metering for non-streaming responses, streams and batches, with response bodies left untouched
- file pinning, including resources whose key was removed
- the batch `results_url` rewrite
- client disconnect mid-stream
- byte-for-byte request forwarding
- sticky routing failing over when a conversation's key is benched, and returning after a refresh

### Live smoke test

The automated tests use a mock. Before relying on the proxy, check it once against the real API:

1. Add one real key in the UI. The automatic check should say "Key works and has credit".
2. Send a non-streaming request. Check that `x-proxy-cost-usd` matches the response's token counts.
3. Send a streaming request, then look up its `x-proxy-request-id` at `/proxy/requests/{id}`. The cost should match the stream's final usage.
4. Send the same request twice with a system prompt of at least ~1K tokens and `cache_control` set. The second should show `cache_read_input_tokens > 0` and the same `x-proxy-upstream-key`.

## Security notes

- Binds to `127.0.0.1` by default. To expose it, put it behind TLS. Anyone with the admin token controls your keys.
- The SQLite database holds the upstream API keys in plain text. Protect the file like you would the keys. Client keys are stored only as SHA-256 hashes.
- The UI keeps the admin token in the browser tab's session storage. It is gone when the tab closes.
