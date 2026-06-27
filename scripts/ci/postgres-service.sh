#!/usr/bin/env bash
set -euo pipefail

command="${1:-start}"
image="${POSTGRES_SERVICE_IMAGE:-postgres:17}"
container="${POSTGRES_SERVICE_CONTAINER:-agentdesk-postgres}"
port="${POSTGRES_SERVICE_PORT:-5432}"
password="${POSTGRES_SERVICE_PASSWORD:-postgres}"
pull_attempts="${POSTGRES_SERVICE_PULL_ATTEMPTS:-3}"
ready_timeout="${POSTGRES_SERVICE_READY_TIMEOUT_SECS:-60}"

log() {
  printf '[postgres-service] %s\n' "$*"
}

gha_error() {
  local title="$1"
  local message="$2"
  printf '::error title=%s::%s\n' "$title" "$message"
}

is_positive_int() {
  [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

require_positive_int() {
  local name="$1"
  local value="$2"

  if ! is_positive_int "$value"; then
    gha_error "Invalid Postgres service configuration" "${name} must be a positive integer; got '${value}'."
    exit 64
  fi
}

require_docker() {
  if ! command -v docker >/dev/null 2>&1; then
    gha_error "Docker unavailable" "Postgres CI service requires docker on the runner."
    exit 69
  fi

  if ! docker info >/dev/null 2>&1; then
    gha_error "Docker daemon unavailable" "Postgres CI service could not contact the docker daemon."
    exit 69
  fi
}

pull_image() {
  local attempt=1
  local delay=5

  while (( attempt <= pull_attempts )); do
    log "pulling ${image} (attempt ${attempt}/${pull_attempts})"
    if docker pull "$image"; then
      return 0
    fi

    if (( attempt < pull_attempts )); then
      log "image pull failed; retrying in ${delay}s"
      sleep "$delay"
      delay=$((delay * 2))
    fi

    attempt=$((attempt + 1))
  done

  gha_error \
    "Postgres service image pull failed" \
    "Unable to pull ${image} after ${pull_attempts} attempts. This is CI runner/image registry infrastructure, not an AgentDesk test failure."
  return 70
}

start_container() {
  local container_id

  docker rm -f "$container" >/dev/null 2>&1 || true

  log "starting ${container} from ${image} on 127.0.0.1:${port}"
  if ! container_id="$(
    docker run \
      --detach \
      --name "$container" \
      --env POSTGRES_USER=postgres \
      --env POSTGRES_PASSWORD="$password" \
      --env POSTGRES_DB=postgres \
      --publish "127.0.0.1:${port}:5432" \
      "$image"
  )"; then
    gha_error \
      "Postgres service container start failed" \
      "Unable to start ${container} from ${image}. This is CI service startup infrastructure, not an AgentDesk test failure."
    return 71
  fi

  log "started container ${container_id}"
}

wait_until_ready() {
  local elapsed=0

  while ! docker exec "$container" pg_isready -U postgres -d postgres >/dev/null 2>&1; do
    if (( elapsed >= ready_timeout )); then
      log "container logs from failed startup:"
      docker logs "$container" || true
      gha_error \
        "Postgres service startup failed" \
        "Container ${container} from ${image} did not become ready within ${ready_timeout}s. This is CI service startup infrastructure, not an AgentDesk test failure."
      return 71
    fi

    sleep 2
    elapsed=$((elapsed + 2))
  done

  log "Postgres is ready"
}

start_service() {
  require_positive_int POSTGRES_SERVICE_PULL_ATTEMPTS "$pull_attempts"
  require_positive_int POSTGRES_SERVICE_READY_TIMEOUT_SECS "$ready_timeout"
  require_docker
  pull_image
  start_container
  wait_until_ready
}

stop_service() {
  if ! command -v docker >/dev/null 2>&1; then
    log "docker unavailable; skipping cleanup"
    return 0
  fi

  if ! docker info >/dev/null 2>&1; then
    log "docker daemon unavailable; skipping cleanup"
    return 0
  fi

  log "stopping ${container}"
  docker rm -f "$container" >/dev/null 2>&1 || true
}

case "$command" in
  start)
    start_service
    ;;
  stop)
    stop_service
    ;;
  *)
    echo "Usage: $0 {start|stop}" >&2
    exit 64
    ;;
esac
