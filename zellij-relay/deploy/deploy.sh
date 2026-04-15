#!/usr/bin/env bash
# One-click relay deploy for the zellij-relay testbed.
#
# Drives everything on the VPS over an SSH docker context — no files are
# copied to the host, no shell sessions are opened, no manual steps on the VPS
# beyond the initial SSH key setup.
#
# Usage:
#     ./deploy.sh [COMMAND] [FLAGS]
#
# Commands (default: deploy):
#     deploy    build images, obtain cert if missing, start the stack, health check
#     logs      tail compose logs (optionally for one service)
#     ps        compose status
#     restart   restart the stack
#     destroy   compose down -v (prompts; deletes the cert volume)
#
# Flags:
#     --vps-ip     <ipv4>    public IPv4 of the VPS                 (required for deploy/destroy)
#     --vps-user   <user>    SSH user on the VPS (e.g. debian)      (required for deploy/destroy)
#     --le-email   <email>   contact email for LetsEncrypt          (required for deploy)
#     --service    <name>    restrict `logs` to one compose service (optional)
#     -h, --help             show this help
#
# Example:
#     ./deploy.sh deploy \
#         --vps-ip    203.0.113.42 \
#         --vps-user  debian \
#         --le-email  you@example.com
#
# The public hostname is derived automatically from --vps-ip via sslip.io:
#     203.0.113.42 -> 203-0-113-42.sslip.io

set -euo pipefail

cd "$(dirname "$0")"

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

# ---------- argument parsing ----------

CMD=""
VPS_IP=""
VPS_USER=""
LE_EMAIL=""
LOG_SERVICE=""

while [ $# -gt 0 ]; do
    case "$1" in
        deploy|logs|ps|restart|destroy)
            [ -z "$CMD" ] || { echo "error: multiple commands given: $CMD, $1" >&2; exit 2; }
            CMD="$1"; shift ;;
        --vps-ip)       VPS_IP="${2:-}";      shift 2 ;;
        --vps-ip=*)     VPS_IP="${1#*=}";     shift ;;
        --vps-user)     VPS_USER="${2:-}";    shift 2 ;;
        --vps-user=*)   VPS_USER="${1#*=}";   shift ;;
        --le-email)     LE_EMAIL="${2:-}";    shift 2 ;;
        --le-email=*)   LE_EMAIL="${1#*=}";   shift ;;
        --service)      LOG_SERVICE="${2:-}"; shift 2 ;;
        --service=*)    LOG_SERVICE="${1#*=}"; shift ;;
        -h|--help)      usage 0 ;;
        *)              echo "error: unknown argument: $1" >&2; usage 2 ;;
    esac
done

CMD="${CMD:-deploy}"

need_host_flags() {
    [ -n "$VPS_IP" ]   || { echo "error: --vps-ip is required for '$CMD'" >&2; exit 2; }
    [ -n "$VPS_USER" ] || { echo "error: --vps-user is required for '$CMD'" >&2; exit 2; }
}

# ---------- derived settings ----------

if [ -n "$VPS_IP" ]; then
    PUBLIC_HOST="$(echo "$VPS_IP" | tr . -).sslip.io"
    export PUBLIC_HOST
    export DOCKER_HOST="ssh://${VPS_USER}@${VPS_IP}"
fi
export COMPOSE_PROJECT_NAME="zellij-relay"

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

do_deploy() {
    need_host_flags
    [ -n "$LE_EMAIL" ] || { echo "error: --le-email is required for 'deploy'" >&2; exit 2; }

    echo "[deploy] VPS              : ${VPS_USER}@${VPS_IP}"
    echo "[deploy] PUBLIC_HOST      : ${PUBLIC_HOST}"
    echo "[deploy] DOCKER_HOST      : ${DOCKER_HOST}"
    echo "[deploy] COMPOSE_PROJECT  : ${COMPOSE_PROJECT_NAME}"
    echo

    ensure_docker

    echo "[deploy] building images on VPS (first build downloads crates and compiles from source; this can take a few minutes)..."
    docker compose build

    echo "[deploy] ensuring TLS cert..."
    LE_EMAIL="$LE_EMAIL" PUBLIC_HOST="$PUBLIC_HOST" ./bootstrap-cert.sh

    echo "[deploy] starting stack..."
    docker compose up -d

    echo "[deploy] waiting for https://${PUBLIC_HOST}/health ..."
    for i in $(seq 1 30); do
        if curl -fsS --max-time 3 "https://${PUBLIC_HOST}/health" >/dev/null 2>&1; then
            echo "[deploy]     healthy"
            break
        fi
        if [ "${i}" -eq 30 ]; then
            echo "[deploy] WARN: health check did not succeed within 60s. Check './deploy.sh logs --vps-ip ${VPS_IP} --vps-user ${VPS_USER}'." >&2
        fi
        sleep 2
    done

    cat <<EOF

──────────────────────────────────────────────────────────────────────
  Deploy complete.

  Configure local Zellij:
      cargo x run -- options --relay-server-url wss://${PUBLIC_HOST}
    or add to KDL config:
      options { relay_server_url "wss://${PUBLIC_HOST}"; }

  Share plugin:  Ctrl-o → share → 't' → 'n' to generate a token,
                 then press 'i' to open the tunnel.

  Public URL pattern:
      https://${PUBLIC_HOST}/r/<slug>

  Operate the stack (shorthand: save VPS_IP/VPS_USER in a shell alias):
      ./deploy.sh logs    --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh ps      --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh restart --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
      ./deploy.sh destroy --vps-ip ${VPS_IP} --vps-user ${VPS_USER}
──────────────────────────────────────────────────────────────────────
EOF
}

# ---------- dispatch ----------

case "$CMD" in
    deploy)
        do_deploy
        ;;
    logs)
        need_host_flags
        docker compose logs -f --tail=200 ${LOG_SERVICE:+$LOG_SERVICE}
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
        printf "[deploy] This will run 'docker compose down -v' on %s (deletes cert volume!). Type YES to continue: " "${VPS_IP}"
        read -r ans
        [ "${ans}" = "YES" ] || { echo "[deploy] aborted"; exit 1; }
        docker compose down -v
        ;;
    *)
        usage 2
        ;;
esac
