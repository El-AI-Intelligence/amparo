# Amparo web surface — deployment

Contract: `docs/web-surface.md` v4 (binding). This app is a face for the
shipped `amparo` binary — the gate chain, approvals, and secrets all live in
the spawned `amparo` process, not here.

## What runs

- **amparo-web.service** — `node /srv/amparo/app/server.mjs`, user `amparo`,
  binds `127.0.0.1:47910` only. Sandboxed (`NoNewPrivileges`,
  `ProtectSystem=strict`, `ReadWritePaths=/srv/amparo/workspaces`).
- **Caddy** — `amparo.ellmstack.dev` → `reverse_proxy 127.0.0.1:47910`,
  config at `/etc/caddy/amparo.Caddyfile` (imported from the main Caddyfile).
  `/approvals*` is blocked at Caddy: that path is the loopback-only gate seam
  (the shipped `--approval-endpoint`'s endpoint).
- **amparo binary** — `/srv/amparo/bin/amparo`, built on the box from the
  source tree at tag **v0.14.0** (`/srv/amparo/src`).

## Layout

| Path | What |
|---|---|
| `/srv/amparo/app/` | the app (`server.mjs`, `public/`) |
| `/srv/amparo/bin/amparo` | the shipped binary the app spawns |
| `/srv/amparo/src/` | source tree pinned at v0.14.0 (build here) |
| `/srv/amparo/workspaces/<operator>/` | per-operator state root (`.amparo/`) |
| `/etc/amparo-web/amparo-web.env` | config + token (0640 root:amparo) |
| `/etc/caddy/amparo.Caddyfile` | vhost |
| `/etc/systemd/system/amparo-web.service` | unit |

## Config

All config is env in `/etc/amparo-web/amparo-web.env`:

- `AMPARO_WEB_TOKEN` — bearer token the UI asks for (generated at install).
- `AMPARO_PORT` (47910), `AMPARO_BIN`, `AMPARO_WORKSPACES`, `AMPARO_OPERATOR`.
- `AMPARO_APPROVAL_ENDPOINT=1` — flip only when the binary is v0.12.0+; makes
  spawned runs pass `--approval-endpoint http://127.0.0.1:47910/approvals`.
- `AMPARO_INFERENCE_URL` / `AMPARO_INFERENCE_MODEL` / `AMPARO_INFERENCE_KEY`,
  `AMPARO_POLICY_KEY` — inherited by spawned runs; never stored by the app
  itself beyond this env file. Without a `--policy-url` per task (or these),
  spawned runs are deny-all by default — that is the intended posture.

## Operate

```sh
systemctl restart amparo-web        # restart
journalctl -u amparo-web -f         # logs (lifecycle only, never task bodies)
cat /etc/amparo-web/amparo-web.env  # retrieve the operator token
caddy validate --config /etc/caddy/Caddyfile && systemctl reload caddy
```

## Update the app

From the dev machine: rsync the repo, then on the box
`/srv/amparo/src/web/deploy/deploy.sh` (idempotent; preserves the env file).

## Roll back

- App: push a previous copy of the app from the dev machine
  (`rsync -a --delete --exclude deploy --exclude test <older-web-dir>/ /srv/amparo/app/`),
  then re-run `deploy.sh`. Note: `web/` is not part of the amparo repo
  (untracked by design), so `git checkout -- web/` has nothing to restore.
- Binary: `git -C /srv/amparo/src checkout <previous-tag>`,
  `cargo build --release`, reinstall to `/srv/amparo/bin/amparo`, restart.
- Caddy: remove the `import` line from `/etc/caddy/Caddyfile`, reload.

## DNS

The operator creates the **grey-cloud** (DNS-only) A record
`amparo.ellmstack.dev → 204.168.163.161` in Cloudflare. Caddy issues the
Let's Encrypt cert automatically on first request after that.
