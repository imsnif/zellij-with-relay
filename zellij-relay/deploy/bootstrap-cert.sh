#!/usr/bin/env bash
# Obtain a Let's Encrypt cert for $PUBLIC_HOST using certbot --standalone.
#
# Idempotent: if a cert for $PUBLIC_HOST is already present in the
# `zellij-relay_letsencrypt` Docker volume, this exits with no action.
#
# Expects DOCKER_HOST to already be set (deploy.sh does this).
# Expects $PUBLIC_HOST, $LE_EMAIL to be exported.

set -euo pipefail

: "${PUBLIC_HOST:?PUBLIC_HOST must be set}"
: "${LE_EMAIL:?LE_EMAIL must be set}"

PROJECT_NAME="${COMPOSE_PROJECT_NAME:-zellij-relay}"
LETSENCRYPT_VOL="${PROJECT_NAME}_letsencrypt"
WEBROOT_VOL="${PROJECT_NAME}_certbot-webroot"

# Make sure the named volumes exist — compose would create them when it
# starts services, but we need them before certbot runs.
docker volume inspect "$LETSENCRYPT_VOL"  >/dev/null 2>&1 || docker volume create "$LETSENCRYPT_VOL"  >/dev/null
docker volume inspect "$WEBROOT_VOL"      >/dev/null 2>&1 || docker volume create "$WEBROOT_VOL"      >/dev/null

# Skip if the cert already exists.
if docker run --rm -v "${LETSENCRYPT_VOL}:/etc/letsencrypt" certbot/certbot:latest \
     certificates 2>/dev/null | grep -q "Domains: ${PUBLIC_HOST}"; then
    echo "[bootstrap-cert] cert for ${PUBLIC_HOST} already present — skipping."
    exit 0
fi

echo "[bootstrap-cert] no cert for ${PUBLIC_HOST}; running certbot --standalone on port 80."
echo "[bootstrap-cert] port 80 on the VPS must be free for this step."

# Stop nginx if it is currently bound to port 80.
if docker compose ps --services --status=running 2>/dev/null | grep -qx nginx; then
    echo "[bootstrap-cert] stopping nginx to free port 80..."
    docker compose stop nginx
fi

docker run --rm \
    -p 80:80 \
    -v "${LETSENCRYPT_VOL}:/etc/letsencrypt" \
    -v "${WEBROOT_VOL}:/var/www/certbot" \
    certbot/certbot:latest \
    certonly --standalone \
             --non-interactive --agree-tos \
             --email "${LE_EMAIL}" \
             -d "${PUBLIC_HOST}"

echo "[bootstrap-cert] cert obtained for ${PUBLIC_HOST}."
