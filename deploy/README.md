# Self-hosting RyzHub

This directory runs a complete RyzHub instance on one Linux machine with Docker Compose:
the web app, the API, the database, and an execution node for sandboxes. `setup.sh` does
the work. This guide explains what it sets up and how to look after the instance.

## What runs

```text
             ryz.example.com        web app ┐
  browser ─▶ api.ryz.example.com    API     ├─▶ caddy ─▶ server ─▶ postgres, nats
             apps.ryz.example.com   plugins ┘               │
                                                            ▼
                                            node: containerd + gVisor ─▶ sandboxes
```

| Service | What it does |
|---|---|
| `caddy` | Serves the web app, gets HTTPS certificates for all three names, proxies to the server |
| `server` | The API, the conversation owners and the model gateway (`ryzhub-server all`) |
| `migrate` | Applies database migrations, then exits; the server waits for it |
| `maintain` | Daily database upkeep: new partitions, retention |
| `postgres` | PostgreSQL 18 with PGroonga and pgvector |
| `nats` | Messaging between server roles |
| `node` | Runs sandboxes in its own containerd, inside a privileged container |

Everything the instance keeps is in this directory: `.env` holds the secrets, `config.toml`
the settings, and `data/` the database, files, sandbox disks and certificates.

## Requirements

- Linux on x86_64 with cgroup v2 (any current distribution), and Docker 24 or later with
  the Compose plugin. The node needs privileged containers, so rootless Docker won't do.
- 4 CPU cores and 8 GB of memory for the first build.
- About 40 GB of free disk: roughly 15 GB of build cache, plus a 20 GB volume for sandboxes.
- Three names pointing at the machine, such as `ryz.example.com`, `api.ryz.example.com`
  and `apps.ryz.example.com` (a wildcard record works), with ports 80 and 443 open.
- An API key from Anthropic (Chat and Claude Code), OpenAI (Codex), or both.

The first build compiles the server and the web app from source and takes 30 to 60 minutes.

## Install

```sh
git clone https://github.com/Bliod-Cook/RyzHub.git
cd RyzHub
deploy/setup.sh ryz.example.com
```

`setup.sh` writes `.env` with random database passwords and a vault key, copies
`config.example.toml` to `config.toml` with your domain in it, builds the images, loads the
sandbox image into the execution node and starts everything. Then, in `deploy/` (where all
`docker compose` commands run):

1. **Add your provider keys** to `.env` (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) and run
   `docker compose up -d`.
2. **Create your account** at `https://ryz.example.com` and make it the instance admin:

   ```sh
   docker compose exec postgres psql -U postgres ryzhub -c \
     "UPDATE users SET is_instance_admin = true WHERE email = 'you@example.com'"
   ```

3. **Close sign-up** once everyone has joined: set `registration = false` under `[auth]` in
   `config.toml` and run `docker compose restart server`. Existing members still sign in.

**Back up `.env`**: the vault key in it encrypts the credentials members store, and nothing
can recover them without it.

## Configuration

`config.toml` is commented, and `crates/server/src/config.rs` describes every setting.
Apply a change with `docker compose restart server maintain`. The server refuses to start
on a key it doesn't know and names it in the log, so typos don't go unnoticed.

Settings that need extra environment variables or files go in `compose.override.yaml`.
Compose reads it automatically, and git ignores it, so `git pull` never touches it:

```yaml
services:
  server:
    environment:
      DEEPSEEK_API_KEY: ${DEEPSEEK_API_KEY}   # the value itself goes in .env
    volumes:
      - ./s3.toml:/etc/ryzhub/s3.toml:ro
```

### Models

The template starts chats on Claude Sonnet 5.5, and runs Claude Code on it and Codex on
GPT-6. Each `[[llm.models]]` entry offers a model for chat and for the Work agents it lists;
the comments show how to add Gemini, any OpenAI-compatible service, or several upstreams
with failover. Members can also add their own keys in Settings unless `[policy]` turns
`allow_personal_credentials` off.

### Sandboxes and Work

Sandboxes run under gVisor, which gives each one its own kernel, and the node refuses
anything else while `RYZHUB_MULTI_TENANT=true`. If you trust everyone with an account, you
can trade that isolation for speed: set `runtime = "runc"` in `config.toml` and
`RYZHUB_MULTI_TENANT=false` in `.env`.

- **Disk:** sandboxes share the volume `RYZHUB_QUOTA_VOLUME_SIZE` sets aside (20 GB),
  allocated when the node first starts and fixed after that. Each sandbox is limited to
  `sandbox.disk_bytes` (10 GB by default).
- **Network:** sandboxes reach only the hosts in `sandbox.egress_allow`: package registries
  and GitHub by default.
- **Extra tools:** a dependency layer adds tools to every sandbox without rebuilding the
  image. Write a Dockerfile in a directory of its own (`scripts/build-deps.sh` explains what
  it may contain), build it, and put the printed reference in `[sandbox] deps`:

  ```sh
  RYZHUB_NODE=$(docker compose ps -q node) ../scripts/build-deps.sh --dockerfile "$PWD/tools/Dockerfile" "$PWD/tools"
  ```

### Storage

Workspace checkpoints and everyone's file library go to `data/blobs`. To use S3-compatible
storage instead, describe the bucket in `s3.toml`:

```toml
endpoint = "https://s3.example.com"
bucket = "ryzhub"
region = "us-east-1"
access_env = "S3_ACCESS_KEY"   # variables holding the keys, passed to the server
secret_env = "S3_SECRET_KEY"
```

Mount it and pass the two keys to the server in `compose.override.yaml`, then set
`storage_root = "s3config:/etc/ryzhub/s3.toml"` under `[work]`. Browsers upload large files
straight to the bucket, so its CORS rules must allow `PUT` from `https://ryz.example.com`
and expose the `ETag` header. `secondary_root` keeps a second copy of everything.

### Sign-in

Besides email and password, people can sign in with any OpenID Connect provider or GitHub;
`config.toml` has an example of each, with the callback URL to register. Someone signing
in this way for the first time joins the account that has the same email, if the provider
has verified it (`auth.link_verified_email`).

### Notifications, plugins and policy

- **Android notifications** need a Firebase project: enable `[push]`, mount the service
  account's key file, and build the app with its `google-services.json`.
- **Plugins** are on by default. Members install remote MCP servers, skills and commands.
  Add publisher keys to `plugins.trusted_keys` to mark their packages as signed, and set
  `policy.allow_unverified_plugins = false` to refuse packages that are neither signed nor
  listed in your `plugins.catalog_url`.
- **`[policy]`** caps what every organization may do: models, sandboxes, uploads, memory,
  full-auto mode and more. Leaving a setting out means no limit.

## Operations

### Upgrading

```sh
git pull
deploy/setup.sh
```

This rebuilds the images, migrates the database before the new server starts, and keeps
your `.env` and `config.toml`. If the execution node's image changed, the node restarts and
stops the sandboxes running on it, so pick a quiet moment.

### Logs and health

```sh
docker compose ps
docker compose logs -f server
curl https://api.ryz.example.com/healthz
```

`RUST_LOG` in `.env` sets how much the server logs (`ryzhub_server=debug` for more).

### Backups

Keep a database dump, `data/blobs`, `.env` and `config.toml`. The node's state and the
certificates are rebuilt on their own.

```sh
docker compose exec -T postgres pg_dump -U postgres -Fc ryzhub > ryzhub.dump
```

To restore, put the old `.env`, `config.toml` and `data/blobs` in place, run `setup.sh`,
then replace the new database with the dump:

```sh
docker compose stop server maintain
docker compose exec -T postgres dropdb -U postgres ryzhub
docker compose exec -T postgres createdb -U postgres ryzhub
docker compose exec -T postgres pg_restore -U postgres -d ryzhub < ryzhub.dump
docker compose start server maintain
```

### Rotating the vault key

1. In `.env`, rename `RYZHUB_VAULT_KEY` to `RYZHUB_VAULT_KEY_0` and add a new
   `RYZHUB_VAULT_KEY`: `head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'`.
2. Under `[llm]` in `config.toml`, set `vault_key_id = 1` and
   `vault_retired_keys = { "0" = "RYZHUB_VAULT_KEY_0" }`.
3. Pass `RYZHUB_VAULT_KEY_0: ${RYZHUB_VAULT_KEY_0}` to `server` and `migrate` in
   `compose.override.yaml`, then run `docker compose up -d`.
4. Re-encrypt the stored keys: `docker compose run --rm migrate migrate-secrets`. After
   that the old key is no longer needed; remove it from all three files.

### Search

After changing the embedding model, rebuild the search index with
`docker compose run --rm migrate reindex`.

### Removing the instance

```sh
docker compose down
docker run --rm --privileged -v "$PWD/data:/data" debian:trixie-slim sh -c \
  'losetup -j /data/node/volume.img | cut -d: -f1 | xargs -r -n1 losetup -d; rm -rf /data/node /data/postgres /data/caddy'
```

The sandbox volume stays attached to a loop device after the node stops, and part of
`data/` belongs to root: the second command detaches the one and deletes the other.
`data/blobs`, `.env` and `config.toml` are yours to keep or delete.

## Security

- **The node is privileged.** Something that escapes a sandbox and then the node's
  container controls the host, so give RyzHub a machine of its own.
- **Open sign-up spends your money.** Every account can use your provider keys, and usage
  budgets are recorded but not enforced yet. Close sign-up when you can.
- **Only Caddy is exposed.** PostgreSQL, NATS and the model gateway listen on the Compose
  network only. Keep it that way.
- **`.env` holds every secret.** It is readable by you alone; keep it that way, and never
  run `git clean -x` in this checkout, which would delete it along with `data/`.

## More than one machine

`setup.sh` covers a single machine. RyzHub can spread out, but expect to wire it yourself:

- **Server roles.** `ryzhub-server all` runs every long-running role in one process; they
  also run separately as `gateway` (the client API), `owner` (conversations and background
  work) and `llm-gateway` (the model gateway), any number of each against one database and
  NATS. The one-shot roles are `migrate`, `maintain [--loop]`, `reindex` and
  `migrate-secrets`.
- **Database accounts.** Running roles connect as `DATABASE_URL`, which must be neither a
  superuser nor `BYPASSRLS` (row-level security keeps organizations apart). Owners also
  read across organizations as `WORKER_DATABASE_URL`, a read-only `BYPASSRLS` role; the
  one-shot roles use `MIGRATION_DATABASE_URL`, a superuser. `postgres/` builds a database
  with the extensions and roles they expect.
- **Sticky sessions.** Clients that fall back from WebSocket to server-sent events need
  `POST /stream/{id}` to reach the gateway that opened the stream.
- **Remote execution nodes** listen with mutual TLS: set `RYZHUB_TLS_LISTEN`,
  `RYZHUB_TLS_CERT`, `RYZHUB_TLS_KEY` and `RYZHUB_TLS_CLIENT_CA` on the node, and list it
  under `[[sandbox.nodes]]` with `address`, `server_name` and `tls_ca`, `tls_cert`,
  `tls_key`. The node's certificate must name `server_name`. Use a CA just for this: any
  client certificate it signs can control the node.
- **Sandboxes reach the model gateway** only through a Unix socket in the node's state
  directory. Run an `llm-gateway` on every node machine, with `llm.unix_socket` in that
  node's `gateway/` directory and the same file name everywhere.

## Troubleshooting

| Symptom | Cause |
|---|---|
| The server exits with `unknown configuration keys: …` | A misspelled or outdated key in `config.toml` |
| No certificate, the browser can't connect | DNS for one of the three names, or ports 80/443 blocked: `docker compose logs caddy` |
| The page loads but can't reach the server | The web app was built for another domain; rerun `setup.sh` after changing it |
| Sandboxes fail with `ISOLATION_POLICY_DENIED` | `runtime = "runc"` while `RYZHUB_MULTI_TENANT=true` |
| The node restarts or stays unhealthy | No cgroup v2 or no privileged containers: `docker compose logs node` and `data/node/containerd.log` |
| Permission errors on an SELinux host | Bind mounts need `:z`; add them in `compose.override.yaml` |
