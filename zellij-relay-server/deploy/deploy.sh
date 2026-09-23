#!/usr/bin/env bash
# One-click relay deploy for the zellij-relay testbed.
#
# Drives everything on the VPS over an SSH docker context — no separately
# maintained source checkout or shell session on the host is needed beyond the
# initial SSH key setup. In hosted mode, the service secret is streamed
# directly into a Docker volume.
#
# Usage:
#     ./deploy.sh [COMMAND] [FLAGS]
#
# Commands (default: deploy):
#     deploy         build images, obtain cert if missing, start the stack, health check
#     logs           tail compose logs (optionally for one service)
#     ps             compose status
#     restart        restart the stack
#     destroy        compose down -v (prompts; deletes data, certs, and hosted secret)
#     create-token   mint a relay tunnel auth token (optional positional label or --label)
#     list-tokens    list relay tunnel auth tokens stored on the relay
#     revoke-token   revoke a relay tunnel auth token by label or raw token
#
# Flags:
#     --vps-ip       <host>  SSH/deploy target of the VPS                 (optional; default relay.zellij.online)
#     --vps-user     <user>  SSH user on the VPS (e.g. root)              (required for all host-bound commands)
#     --le-email     <email> contact email for LetsEncrypt                (required for deploy)
#     --public-host  <fqdn>  relay host for TLS cert + server_name        (optional; default relay.zellij.online)
#     --mode         <mode>  deploy mode: standalone or hosted            (optional; default standalone)
#     --control-plane-url <url> hosted control-plane base URL             (required for hosted deploy)
#     --control-plane-secret-file <path> local file containing the hosted
#                                  service secret                          (required for hosted deploy)
#     --service      <name>  restrict `logs` to one compose service       (optional)
#     --label        <name>  label for create-token / revoke-token        (optional for create-token; required for revoke-token unless given positionally)
#     -h, --help             show this help
#
# Example:
#     ./deploy.sh deploy --vps-user root --le-email you@zellij.online
#     ./deploy.sh deploy --mode hosted --control-plane-url https://example.com \
#         --control-plane-secret-file /secure/relay-secret \
#         --vps-user root --le-email you@zellij.online
#
#     ./deploy.sh create-token my-laptop --vps-user root
#     ./deploy.sh list-tokens             --vps-user root
#     ./deploy.sh revoke-token my-laptop  --vps-user root
#
# This deploy is transport-only: the relay host (cert + server_name + SSH
# target) defaults to relay.zellij.online and serves no HTML. The browser
# viewer is served from a SEPARATE app origin (APP_ORIGIN, default
# https://zellij.online), configured out-of-band; APP_ORIGIN drives the
# public URL template and the relay's CORS allowlist.
#
# The SSH target and relay host both default to relay.zellij.online; its DNS A
# record must point at the VPS before deploy, or cert issuance fails. To deploy
# by raw IP (e.g. first bring-up before DNS is pointed) or to a sslip.io
# testbed, pass --vps-ip and/or --public-host:
#     --vps-ip 203.0.113.42 --public-host 203-0-113-42.sslip.io

set -euo pipefail

CALLER_PWD="$PWD"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
SCRIPT_PATH="${SCRIPT_DIR}/$(basename "$0")"
cd "$SCRIPT_DIR"

usage() {
    sed -n '2,/^$/p' "$SCRIPT_PATH" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# ---------- argument parsing ----------

CMD=""
VPS_IP=""
VPS_USER=""
LE_EMAIL=""
LOG_SERVICE=""
TOKEN_LABEL=""
PUBLIC_HOST_OVERRIDE=""
DEPLOY_MODE="standalone"
DEPLOY_MODE_EXPLICIT=0
CONTROL_PLANE_URL=""
CONTROL_PLANE_SECRET_FILE=""

while [ $# -gt 0 ]; do
    case "$1" in
        deploy|logs|ps|restart|destroy|create-token|list-tokens|revoke-token)
            [ -z "$CMD" ] || { echo "error: multiple commands given: $CMD, $1" >&2; exit 2; }
            CMD="$1"; shift ;;
        --vps-ip)       VPS_IP="${2:-}";      shift 2 ;;
        --vps-ip=*)     VPS_IP="${1#*=}";     shift ;;
        --vps-user)     VPS_USER="${2:-}";    shift 2 ;;
        --vps-user=*)   VPS_USER="${1#*=}";   shift ;;
        --le-email)     LE_EMAIL="${2:-}";    shift 2 ;;
        --le-email=*)   LE_EMAIL="${1#*=}";   shift ;;
        --public-host)   PUBLIC_HOST_OVERRIDE="${2:-}";  shift 2 ;;
        --public-host=*) PUBLIC_HOST_OVERRIDE="${1#*=}"; shift ;;
        --mode)          DEPLOY_MODE="${2:-}"; DEPLOY_MODE_EXPLICIT=1; shift 2 ;;
        --mode=*)        DEPLOY_MODE="${1#*=}"; DEPLOY_MODE_EXPLICIT=1; shift ;;
        --control-plane-url)   CONTROL_PLANE_URL="${2:-}";         shift 2 ;;
        --control-plane-url=*) CONTROL_PLANE_URL="${1#*=}";        shift ;;
        --control-plane-secret-file)   CONTROL_PLANE_SECRET_FILE="${2:-}";  shift 2 ;;
        --control-plane-secret-file=*) CONTROL_PLANE_SECRET_FILE="${1#*=}"; shift ;;
        --service)      LOG_SERVICE="${2:-}"; shift 2 ;;
        --service=*)    LOG_SERVICE="${1#*=}"; shift ;;
        --label)        TOKEN_LABEL="${2:-}"; shift 2 ;;
        --label=*)      TOKEN_LABEL="${1#*=}"; shift ;;
        -h|--help)      usage 0 ;;
        *)
            # Allow a single positional argument after token subcommands as a
            # convenience: `./deploy.sh create-token my-laptop` ≡ `--label my-laptop`.
            if [ -z "$TOKEN_LABEL" ] && { [ "$CMD" = "create-token" ] || [ "$CMD" = "revoke-token" ]; }; then
                TOKEN_LABEL="$1"; shift
            else
                echo "error: unknown argument: $1" >&2; usage 2
            fi
            ;;
    esac
done

CMD="${CMD:-deploy}"

if [ "$CMD" != "deploy" ] \
    && { [ "$DEPLOY_MODE_EXPLICIT" -eq 1 ] \
        || [ -n "$CONTROL_PLANE_URL" ] \
        || [ -n "$CONTROL_PLANE_SECRET_FILE" ]; }; then
    echo "error: --mode and control-plane flags are only valid with 'deploy'" >&2
    exit 2
fi

if [ -n "$CONTROL_PLANE_SECRET_FILE" ]; then
    case "$CONTROL_PLANE_SECRET_FILE" in
        /*) ;;
        *) CONTROL_PLANE_SECRET_FILE="${CALLER_PWD}/${CONTROL_PLANE_SECRET_FILE}" ;;
    esac
fi

need_host_flags() {
    [ -n "$VPS_USER" ] || { echo "error: --vps-user is required for '$CMD'" >&2; exit 2; }
}

# ---------- derived settings ----------

DEFAULT_PUBLIC_HOST="relay.zellij.online"
DEFAULT_APP_ORIGIN="https://zellij.online"

VPS_IP="${VPS_IP:-$DEFAULT_PUBLIC_HOST}"
PUBLIC_HOST="${PUBLIC_HOST_OVERRIDE:-$DEFAULT_PUBLIC_HOST}"
APP_ORIGIN="${APP_ORIGIN:-$DEFAULT_APP_ORIGIN}"

export PUBLIC_HOST
export APP_ORIGIN
export DOCKER_HOST="ssh://${VPS_USER}@${VPS_IP}"
export COMPOSE_PROJECT_NAME="zellij-relay"

BASE_COMPOSE_FILE="${PWD}/compose.yml"
HOSTED_COMPOSE_FILE="${PWD}/compose.hosted.yml"
HOSTED_SECRET_VOLUME="${COMPOSE_PROJECT_NAME}_control-plane-secret"
BUILD_CONTEXT_DIR="$(cd ../.. && pwd -P)"

# Non-deploy commands operate on the existing Compose project and do not need
# the hosted overlay to find its containers. do_deploy opts into the overlay
# after validating all hosted-only inputs.
export COMPOSE_FILE="$BASE_COMPOSE_FILE"

# ---------- helpers ----------

ssh_cmd() {
    ssh -o StrictHostKeyChecking=accept-new -o BatchMode=yes "${VPS_USER}@${VPS_IP}" "$@"
}

ensure_docker() {
    if ssh_cmd 'command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1'; then
        return 0
    fi

    echo "[deploy] docker not present (or current user lacks access); installing..."
    ssh_cmd 'curl -fsSL https://get.docker.com | sudo sh'
    ssh_cmd "sudo usermod -aG docker ${VPS_USER}"
    ssh -O exit "${VPS_USER}@${VPS_IP}" >/dev/null 2>&1 || true

    if ! ssh_cmd 'docker info >/dev/null 2>&1'; then
        cat >&2 <<EOF
[deploy] Docker installed but the SSH user still cannot talk to it.
         Log out and back in once, then re-run:
             ssh ${VPS_USER}@${VPS_IP} exit
             ./deploy.sh deploy --vps-ip ${VPS_IP} --vps-user ${VPS_USER} --le-email ${LE_EMAIL}
EOF
        exit 1
    fi
}

validate_deploy_mode() {
    case "$DEPLOY_MODE" in
        standalone)
            if [ -n "$CONTROL_PLANE_URL" ] || [ -n "$CONTROL_PLANE_SECRET_FILE" ]; then
                echo "error: --control-plane-url and --control-plane-secret-file require --mode hosted" >&2
                exit 2
            fi
            ;;
        hosted)
            [ -n "$CONTROL_PLANE_URL" ] || {
                echo "error: --control-plane-url is required for a hosted deploy" >&2
                exit 2
            }
            [ -n "$CONTROL_PLANE_SECRET_FILE" ] || {
                echo "error: --control-plane-secret-file is required for a hosted deploy" >&2
                exit 2
            }
            [ -f "$CONTROL_PLANE_SECRET_FILE" ] && [ -r "$CONTROL_PLANE_SECRET_FILE" ] || {
                echo "error: control-plane secret file is not a readable regular file: ${CONTROL_PLANE_SECRET_FILE}" >&2
                exit 2
            }
            grep -q '[^[:space:]]' "$CONTROL_PLANE_SECRET_FILE" || {
                echo "error: control-plane secret file is empty" >&2
                exit 2
            }
            case "$CONTROL_PLANE_URL" in
                https://*) ;;
                *)
                    echo "error: hosted control-plane URL must use https" >&2
                    exit 2
                    ;;
            esac
            local secret_dir secret_path
            secret_dir="$(cd "$(dirname "$CONTROL_PLANE_SECRET_FILE")" && pwd -P)"
            secret_path="${secret_dir}/$(basename "$CONTROL_PLANE_SECRET_FILE")"
            case "$secret_path" in
                "${BUILD_CONTEXT_DIR}"/*)
                    echo "error: control-plane secret file must be stored outside the Docker build context (${BUILD_CONTEXT_DIR})" >&2
                    exit 2
                    ;;
            esac
            export RELAY_CONTROL_PLANE_URL="$CONTROL_PLANE_URL"
            export RELAY_CONTROL_PLANE_SECRET_VOLUME="$HOSTED_SECRET_VOLUME"
            export COMPOSE_FILE="${BASE_COMPOSE_FILE}:${HOSTED_COMPOSE_FILE}"
            ;;
        *)
            echo "error: --mode must be 'standalone' or 'hosted' (got '${DEPLOY_MODE}')" >&2
            exit 2
            ;;
    esac
}

guard_deployment_mode_transition() {
    if [ "$DEPLOY_MODE" = "standalone" ] \
        && [ "$DEPLOY_MODE_EXPLICIT" -ne 1 ] \
        && relay_is_hosted; then
        cat >&2 <<'EOF'
error: the running relay is hosted, but this deploy did not specify a mode.
       Re-run with `--mode hosted` to update it, or explicitly pass
       `--mode standalone` to switch back to the local token store.
EOF
        exit 2
    fi
}

install_control_plane_secret() {
    echo "[deploy] installing hosted control-plane secret in protected Docker volume..."
    docker volume inspect "$HOSTED_SECRET_VOLUME" >/dev/null 2>&1 \
        || docker volume create "$HOSTED_SECRET_VOLUME" >/dev/null

    # Stream the value over the Docker SSH connection on stdin. It never
    # appears in argv, the relay environment, the image, or command output.
    # Write to a temporary path first so an interrupted rotation preserves the
    # previously installed secret.
    docker run --rm -i \
        --user 0:0 \
        --volume "${HOSTED_SECRET_VOLUME}:/run/secrets" \
        --entrypoint sh \
        debian:bookworm-slim \
        -c 'set -eu
            umask 077
            tmp=/run/secrets/.relay-control-plane.tmp
            cat > "$tmp"
            test -s "$tmp"
            chown 10001:10001 "$tmp"
            chmod 0400 "$tmp"
            mv -f "$tmp" /run/secrets/relay-control-plane' \
        < "$CONTROL_PLANE_SECRET_FILE"

    # Verify the runtime UID can read a non-empty file before replacing the
    # relay container.
    docker run --rm \
        --user 10001:10001 \
        --volume "${HOSTED_SECRET_VOLUME}:/run/secrets:ro" \
        --entrypoint sh \
        debian:bookworm-slim \
        -c 'test -r /run/secrets/relay-control-plane && test -s /run/secrets/relay-control-plane'
}

relay_is_hosted() {
    local relay_container mode
    relay_container="$(docker compose ps -q relay 2>/dev/null)"
    [ -n "$relay_container" ] || return 1
    mode="$(docker inspect --format '{{ index .Config.Labels "org.zellij.relay.mode" }}' "$relay_container" 2>/dev/null || true)"
    [ "$mode" = "hosted" ]
}

require_standalone_token_store() {
    if relay_is_hosted; then
        cat >&2 <<'EOF'
error: the running relay uses hosted control-plane authentication.
       Manage credentials through the control plane; the local SQLite token
       commands only apply to standalone deployments.
EOF
        exit 2
    fi
}

verify_hosted_runtime() {
    local relay_container
    relay_container="$(docker compose ps -q relay)"
    [ -n "$relay_container" ] || {
        echo "[deploy] ERROR: hosted relay container was not created" >&2
        return 1
    }

    if docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$relay_container" \
        | grep '^RELAY_CONTROL_PLANE_SECRET=' >/dev/null; then
        echo "[deploy] ERROR: hosted service secret was exposed in the container environment" >&2
        return 1
    fi

    docker compose exec -T relay sh -c \
        'test -n "${RELAY_CONTROL_PLANE_URL:-}" && test -r "${RELAY_CONTROL_PLANE_SECRET_FILE:-}" && test -s "${RELAY_CONTROL_PLANE_SECRET_FILE:-}"' || {
        echo "[deploy] ERROR: hosted relay cannot read its control-plane configuration" >&2
        return 1
    }

    docker compose logs --no-color --tail=200 relay \
        | grep 'hosted mode: tunnel auth via control plane' >/dev/null || {
        echo "[deploy] ERROR: relay did not report hosted-control-plane mode" >&2
        return 1
    }
}

do_deploy() {
    need_host_flags
    [ -n "$LE_EMAIL" ] || { echo "error: --le-email is required for 'deploy'" >&2; exit 2; }
    validate_deploy_mode

    echo "[deploy] VPS              : ${VPS_USER}@${VPS_IP}"
    echo "[deploy] PUBLIC_HOST      : ${PUBLIC_HOST}"
    echo "[deploy] APP_ORIGIN       : ${APP_ORIGIN}"
    echo "[deploy] MODE             : ${DEPLOY_MODE}"
    if [ "$DEPLOY_MODE" = "hosted" ]; then
        echo "[deploy] CONTROL_PLANE    : ${CONTROL_PLANE_URL}"
    fi
    echo "[deploy] DOCKER_HOST      : ${DOCKER_HOST}"
    echo "[deploy] COMPOSE_PROJECT  : ${COMPOSE_PROJECT_NAME}"
    echo

    ensure_docker
    guard_deployment_mode_transition

    if [ "$DEPLOY_MODE" = "hosted" ]; then
        install_control_plane_secret
    fi

    echo "[deploy] building images on VPS (--no-cache; full rebuild every run)..."
    docker compose build --no-cache

    echo "[deploy] ensuring TLS cert..."
    LE_EMAIL="$LE_EMAIL" PUBLIC_HOST="$PUBLIC_HOST" ./bootstrap-cert.sh

    echo "[deploy] starting stack..."
    docker compose up -d --force-recreate

    echo "[deploy] waiting for https://${PUBLIC_HOST}/health ..."
    healthy=0
    health_deadline=$((SECONDS + 60))
    while [ "$SECONDS" -lt "$health_deadline" ]; do
        if curl -fsS --max-time 3 "https://${PUBLIC_HOST}/health" >/dev/null 2>&1; then
            echo "[deploy]     healthy"
            healthy=1
            break
        fi
        sleep 2
    done

    if [ "$healthy" -ne 1 ]; then
        echo "[deploy] ERROR: health check did not succeed within 60s." >&2
        docker compose ps >&2 || true
        docker compose logs --no-color --tail=100 relay nginx >&2 || true
        return 1
    fi

    if [ "$DEPLOY_MODE" = "hosted" ]; then
        verify_hosted_runtime
        echo "[deploy]     hosted control-plane configuration verified"
    fi

    cat <<EOF

──────────────────────────────────────────────────────────────────────
  Deploy complete.

  Mode: ${DEPLOY_MODE}

  Configure local Zellij (skip on the default host — wss://zellij.online
  is the built-in default relay):
      cargo x run -- options --relay-server-url wss://${PUBLIC_HOST}
    or add to KDL config:
      options { relay_server_url "wss://${PUBLIC_HOST}"; }

  Share plugin:  Ctrl-o → share → 't' → 'n' to generate a token,
                 then press 'i' to open the tunnel.

  Public URL pattern (points at the app origin serving the viewer):
      ${APP_ORIGIN}/r/<slug>

  Operate the stack (shorthand: save VPS_IP/VPS_USER in a shell alias):
      ./deploy.sh logs    --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh ps      --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh restart --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh destroy --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
EOF

    if [ "$DEPLOY_MODE" = "hosted" ]; then
        cat <<'EOF'

  Tunnel credentials are managed by the hosted control plane. The local
  create-token, list-tokens, and revoke-token commands are intentionally
  unavailable while this deployment is running in hosted mode.
EOF
    else
        cat <<EOF

  Manage standalone tunnel-auth tokens:
      ./deploy.sh create-token <label> --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh list-tokens           --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
EOF
    fi

    echo "──────────────────────────────────────────────────────────────────────"
}

# ---------- dispatch ----------

case "$CMD" in
    deploy)
        do_deploy
        ;;
    logs)
        need_host_flags
        if [ -n "$LOG_SERVICE" ]; then
            docker compose logs -f --tail=200 "$LOG_SERVICE"
        else
            docker compose logs -f --tail=200
        fi
        ;;
    ps)
        need_host_flags
        docker compose ps
        ;;
    restart)
        need_host_flags
        docker compose restart
        ;;
    destroy)
        need_host_flags
        printf "[deploy] This will run 'docker compose down -v' on %s and delete certificates, relay data, and any hosted service-secret volume. Type YES to continue: " "${VPS_IP}"
        read -r ans
        [ "${ans}" = "YES" ] || { echo "[deploy] aborted"; exit 1; }
        docker compose down -v
        if docker volume inspect "$HOSTED_SECRET_VOLUME" >/dev/null 2>&1; then
            docker volume rm "$HOSTED_SECRET_VOLUME" >/dev/null
            echo "[deploy] deleted hosted service-secret volume"
        fi
        ;;
    create-token)
        need_host_flags
        require_standalone_token_store
        # `docker compose exec` bypasses the image ENTRYPOINT, so the binary
        # path is supplied explicitly. Token DB lives on the `relay-data`
        # named volume mounted at /var/lib/zellij-relay.
        if [ -n "$TOKEN_LABEL" ]; then
            docker compose exec relay zellij-relay-server create-token "$TOKEN_LABEL"
        else
            docker compose exec relay zellij-relay-server create-token
        fi
        ;;
    list-tokens)
        need_host_flags
        require_standalone_token_store
        docker compose exec relay zellij-relay-server list-tokens
        ;;
    revoke-token)
        need_host_flags
        require_standalone_token_store
        [ -n "$TOKEN_LABEL" ] || { echo "error: 'revoke-token' requires a label or token (positional or --label)" >&2; exit 2; }
        docker compose exec relay zellij-relay-server revoke-token "$TOKEN_LABEL"
        ;;
    *)
        usage 2
        ;;
esac
