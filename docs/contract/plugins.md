# Plugin contract — M8 (SEED.md §6, Phase 5)

Third-party plugins without trusting third-party code: a plugin is an
MCP-stdio subprocess the **bridge** spawns inside a bubblewrap sandbox.
Installing a plugin grants nothing — every scope is a separate operator
grant (`ka-agent grant add <scope>`), and enabling is CLI-only policy.

Decisions recorded for SEED.md §12-Q3: the model is **MCP-stdio +
bubblewrap** (not WASM, not manifest-only); **v1 plugins have no network
egress** (`--unshare-net`), so `requires.network` must be empty and
network-capable connectors stay first-party (`http_get`, `github.*`).

## Manifest v1

`manifest.toml` at the archive root. Strict TOML — unknown keys are errors
(mirroring `ka-agent.toml`). Top-level tables may appear in any order, but
**scalar keys (`[plugin]`'s fields) must live inside their table section**.

```toml
[plugin]
id = "probe"                       # [a-z0-9-]+, no doubled/dangling '-'
version = "0.1.0"                  # MAJOR.MINOR.PATCH, numeric only
api = 1                            # this host supports 1
kind = "tool"                      # tool | connector | skill-pack
description = "what it does"
entry = { type = "mcp-stdio", command = "/plugin/bin/probe", args = [] }

[requires]
secrets = ["probe.token"]          # names resolved from the secret store
network = []                       # must be empty in v1 (no egress)

[[tools]]
name = "probe.sandbox"             # must start with "<id>."
effect = "read"                    # read | local_write | external_write
scopes = []                        # granted separately by the operator
```

Validation (rejects with a visible reason, never half-loads):

- `plugin.id` charset; `api` must equal 1; semver numeric only.
- `entry.type` is `mcp-stdio`; `entry.command` non-empty.
- **`requires.network` must be `[]`** — the sandbox unshares the network, so
  a declared host would be a lie. Use a first-party connector for network.
- Tool names are `<id>.<tail>` ([a-z0-9_.]), unique.
- Effects are `read | local_write | external_write`. `exec` and
  `irreversible` require an ask round-trip the bridge does not own — they
  are not loadable in v1.

## Sandbox profile (per tool call)

One call = one `bwrap`-wrapped subprocess (spawn, MCP handshake, call,
kill — no reuse, nothing survives):

```
bwrap --unshare-net --die-with-parent
      --ro-bind-try /usr /usr
      --symlink usr/bin /bin   --symlink usr/lib /lib
      --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin
      --proc /proc --dev /dev --tmpfs /tmp
      --ro-bind <plugins>/<id> /plugin
      --clearenv --setenv PATH /usr/bin:/bin --setenv HOME /tmp/home
      --setenv <SECRET_ENV> <value>      # one per declared secret
      /plugin/bin/probe -- <args>
```

- **No network** (`--unshare-net`): any `connect()` fails.
- **Read-only system**: only `/usr` (+ the merged-usr symlinks), the plugin
  root at `/plugin` (read-only), private `/tmp` tmpfs and `/tmp/home`.
  `/etc`, the operator's home and the daemon's data are absent.
- **Secrets**: only the names in `requires.secrets`, resolved from the
  secret store and injected as `KA_AGENT_SECRET_*`-convention env names
  (`probe.token` → `PROBE_TOKEN`). Undeclared secrets, the bot token and
  provider keys are never in the environment.
- **Limits**: 30 s wall clock per call (watchdog `kill -9`), 64 KiB output
  cap (truncated), and the bridge caps concurrent plugin connections at 64.
- `KAGENT_PLUGIN_DEBUG=1` streams the plugin's stderr (authoring).

### Host prerequisites

Bubblewrap needs **unprivileged user namespaces**. Ubuntu 23.10+ restricts
them by default, so an unprivileged operator must either

```
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0   # or set it in sysctl.d
```

or ship an AppArmor profile that grants `userns` to `/usr/bin/bwrap`.
Without it `bwrap` exits immediately; the bridge reports the plugin's exit
status and stderr tail (e.g. `setting up uid map: Permission denied`) rather
than a bare timeout.

## Signing and distribution

- `ka-agent plugin keygen` — one ed25519 keypair per instance under
  `<data>/plugins/keys/` (seed 0600). The public line goes to vendors.
- `ka-agent plugin pack <dir> --out <archive>` — tars the directory
  (deterministic order) and signs the exact archive bytes; the signature is
  written next to the archive as `.sig`.
- `ka-agent plugin install <archive>` — verifies the signature against
  every trusted key, checks the **TOFU pin** (the key that signed an id's
  first install is the only key allowed to sign its updates), unpacks to
  `<data>/plugins/<id>/`, and records the tree digest in
  `installed.json`. Install never enables.
- Updates keep the operator's enable state; installs of new ids start
  disabled.
- **Tampering with installed files is detected at bridge load**: the
  recomputed tree digest must match the pinned one, otherwise the plugin is
  disabled with a visible reason and its tools disappear from `tools/list`.

## Lifecycle

```
ka-agent plugin keygen
ka-agent plugin pack ./my-plugin --out ./my-plugin-0.1.0.kplug
ka-agent plugin install ./my-plugin-0.1.0.kplug
ka-agent plugin enable my-plugin
ka-agent plugin list          # id, version, state, tools
ka-agent plugin disable my-plugin
ka-agent plugin rm my-plugin  # refuses while enabled
```

Enable/disable is policy and therefore CLI-only (SEED.md §5); a change takes
effect the next time the engine spawns the bridge.

## Policy at call time

For each `tools/call` of a plugin tool the bridge:

1. checks every declared scope against the grants store
   (refusal message names the `ka-agent grant add` line);
2. for `local_write`/`external_write` records an intent
   (`executing` → `done`/`failed`) — the same audit trail as first-party
   writes;
3. spawns the sandbox and forwards the call;
4. settles the intent.

Effect floors in v1: `read` needs only the plugin to be enabled;
`local_write`/`external_write` need their scopes granted (the grant is the
approval, as for the first-party GitHub tools). Plugin tools reach engines
as `kagent__<tool>` through the single `[[mcp]]` bridge, so ka's own
permission logic never gates them.

## Authoring

- `examples/plugins/probe` is the reference: a dependency-free Rust MCP
  server with three read tools and the sandbox-escape report
  (`probe.sandbox`).
- Tool answers are plain text (put in `content[0].text`), truncated at
  64 KiB; errors set `isError: true`.
- Print one JSON-RPC line per response, flush per line, and answer
  `initialize` → `tools/call`. The process is killed after each call:
  stateless tools only.
- Test locally with `KAGENT_PLUGIN_DEBUG=1 ka-agent __mcp` and JSON-RPC on
  stdin.
