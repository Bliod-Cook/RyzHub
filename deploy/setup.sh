#!/usr/bin/env bash
# Prepares and starts a RyzHub instance with Docker Compose.
#
#   deploy/setup.sh ryz.example.com   the first time: the web app is served at the domain,
#                                     the API at api.<domain>, plugin pages at apps.<domain>
#   deploy/setup.sh                   after a git pull, to upgrade
#
# It never replaces .env, whose vault key encrypts stored credentials, or etc/config.toml.
set -euo pipefail
cd "$(dirname "$0")"

fail() { printf 'setup.sh: %s\n' "$*" >&2; exit 1; }
[[ "$(uname -m)" == x86_64 ]] || fail "execution nodes run on x86_64 Linux only"
docker info >/dev/null 2>&1 || fail "cannot reach Docker; is it running, and may $(id -un) use it?"
docker compose version >/dev/null 2>&1 || fail "Docker Compose v2 is required"

# URL-safe base64 without padding: usable in database URLs, and the vault key's format.
random() { head -c "$1" /dev/urandom | base64 | tr '+/' '-_' | tr -d '=\n'; }

domain=${1:-}
if [[ ! -f .env ]]; then
  [[ "$domain" =~ ^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$ ]] ||
    fail "usage: $0 DOMAIN, for example: $0 ryz.example.com"
  (
    umask 077
    cat > .env <<EOF
# Written by setup.sh. Back this file up: without RYZHUB_VAULT_KEY the credentials stored
# in the database cannot be decrypted.
RYZHUB_DOMAIN=$domain
RYZHUB_UID=$(id -u)
RYZHUB_GID=$(id -g)
POSTGRES_PASSWORD=$(random 24)
RYZHUB_APP_PASSWORD=$(random 24)
RYZHUB_WORKER_PASSWORD=$(random 24)
RYZHUB_VAULT_KEY=$(random 32)
# Disk set aside for sandboxes, allocated when the execution node first starts. Changing it
# later has no effect.
RYZHUB_QUOTA_VOLUME_SIZE=$((20 << 30))

# Model provider keys: Anthropic for Chat and Claude Code, OpenAI for Codex.
ANTHROPIC_API_KEY=
OPENAI_API_KEY=
GEMINI_API_KEY=
GITHUB_CLIENT_SECRET=
EOF
  )
  echo "Wrote .env"
else
  current=$(sed -n 's/^RYZHUB_DOMAIN=//p' .env)
  [[ -z "$domain" || "$domain" == "$current" ]] ||
    fail ".env is for $current; to move the instance, change the domain in .env and etc/config.toml"
  domain=$current
fi
# The server rewrites its configuration from the admin panel, so it has a directory of its own.
mkdir -p etc
if [[ -f config.toml && ! -f etc/config.toml ]]; then
  mv config.toml etc/config.toml
  echo "Moved config.toml to etc/config.toml"
fi
if [[ ! -f etc/config.toml ]]; then
  sed "s/ryz\.example\.com/$domain/g" config.example.toml > etc/config.toml
  echo "Wrote etc/config.toml"
fi
mkdir -p data/blobs data/node/gateway
chmod 700 data/node

# One at a time: building them together can exhaust memory on a small machine.
for service in postgres server node caddy; do
  echo "Building $service..."
  docker compose build --pull "$service"
done

echo "Loading the sandbox image into the execution node..."
docker compose up -d --wait node
image=$(RYZHUB_NODE=$(docker compose ps -q node) ../scripts/build-harness-image.sh)
# Only an empty value, or an image this script set before, is replaced.
sed -i -E "s#^image = \"(docker\.io/library/ryzhub-harness@sha256:[0-9a-f]{64})?\"\$#image = \"$image\"#" etc/config.toml

docker compose up -d --wait server maintain caddy

if ! grep -Eq '^(ANTHROPIC|OPENAI|GEMINI)_API_KEY=.' .env; then
  keys="
  * Add a model provider key to deploy/.env, then run: docker compose up -d"
fi
cat <<EOF

RyzHub is running. To finish:
  * Point $domain, api.$domain and apps.$domain at this machine; ports 80 and 443
    must be reachable for HTTPS certificates.${keys:-}
  * Open https://$domain and create your account. Make it the instance admin with:
      docker compose exec postgres psql -U postgres ryzhub -c \\
        "UPDATE users SET is_instance_admin = true WHERE email = 'you@example.com'"
    Settings then has an Instance admin page for users and configuration.
  * When everyone has joined, turn off sign-up there (Configuration, Sign-in).
EOF
