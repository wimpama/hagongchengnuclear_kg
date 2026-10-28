#!/usr/bin/env sh
# Update and deploy only this repository's independent knowledge services.
set -eu

usage() {
  cat <<'USAGE'
Usage: sh deploy.sh [--branch master] [--remote origin] [--env-file .env] [--no-pull] [--no-build]

Linux server only. Requires Git, Docker Compose, Python 3 and flock.
Updates a clean checkout by fast-forward, builds images, then starts Rust + Qdrant.
--no-pull   Deploy the current clean checkout without contacting Git.
--no-build  Use the images configured in .env (e.g. images loaded with docker load).
Nginx and the existing business/frontend services are managed separately.
USAGE
}

main() {
  backend_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  cd "$backend_dir"
  branch=master
  remote=origin
  env_file="$backend_dir/.env"
  pull_code=yes
  build_images=yes
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --branch|--remote|--env-file)
        [ "$#" -ge 2 ] || { usage >&2; exit 2; }
        case "$1" in
          --branch) branch=$2 ;;
          --remote) remote=$2 ;;
          --env-file) env_file=$2 ;;
        esac
        shift 2
        ;;
      --no-pull) pull_code=no; shift ;;
      --no-build) build_images=no; shift ;;
      -h|--help) usage; exit 0 ;;
      *) usage >&2; exit 2 ;;
    esac
  done

  [ "$(uname -s)" = Linux ] || {
    printf '%s\n' 'deploy.sh is for the Linux server. Use start-local.ps1 on Windows.' >&2
    exit 1
  }
  for dependency in git docker python3 flock; do
    command -v "$dependency" >/dev/null 2>&1 || {
      printf 'Missing required command: %s\n' "$dependency" >&2
      exit 1
    }
  done
  git check-ref-format --branch "$branch" >/dev/null
  case "$remote" in
    ''|-*|*[!A-Za-z0-9_.-]*) printf '%s\n' 'Invalid Git remote name.' >&2; exit 2 ;;
  esac
  [ "$(git rev-parse --show-toplevel)" = "$backend_dir" ] || {
    printf '%s\n' 'Run this script from the standalone backend Git repository.' >&2
    exit 1
  }

  # File-descriptor locks release automatically even after a failed deployment.
  lock_file=$(git rev-parse --git-path course-knowledge-deploy.lock)
  exec 9>"$lock_file"
  flock -n 9 || { printf '%s\n' 'Another backend deployment is running.' >&2; exit 1; }
  [ "$(git symbolic-ref --quiet --short HEAD)" = "$branch" ] || {
    printf 'Checkout must already be on branch %s. No branch was switched.\n' "$branch" >&2
    exit 1
  }
  [ -z "$(git status --porcelain --untracked-files=normal)" ] || {
    printf '%s\n' 'Working tree has local changes. Commit or move them before deployment.' >&2
    exit 1
  }
  [ -f "$env_file" ] || {
    printf '%s\n' 'Missing environment file. Copy env.example and configure credentials and KB_HOST_DATA_DIR.' >&2
    exit 1
  }
  if git ls-files --error-unmatch -- "$env_file" >/dev/null 2>&1; then
    printf '%s\n' 'The environment file must stay outside Git tracking.' >&2
    exit 1
  fi
  docker compose version >/dev/null
  docker info >/dev/null

  if [ "$pull_code" = yes ]; then
    printf 'Fetching %s/%s...\n' "$remote" "$branch"
    git fetch --no-tags "$remote" "refs/heads/$branch"
    git merge-base --is-ancestor HEAD FETCH_HEAD || {
      printf '%s\n' 'Local branch is ahead of or diverged from the remote. Resolve it manually.' >&2
      exit 1
    }
    git merge --ff-only --no-edit FETCH_HEAD
  fi
  revision=$(git rev-parse HEAD)
  if [ "$build_images" = yes ]; then
    # A commit-specific tag preserves the previous image for manual rollback.
    KB_SERVICE_IMAGE="3dcourse-knowledge-service:$revision"
    export KB_SERVICE_IMAGE
  fi

  compose() {
    # Explicit project name prevents COMPOSE_PROJECT_NAME inherited from another
    # deployment from selecting the existing business containers.
    docker compose --project-name course-knowledge --project-directory "$backend_dir" \
      --file "$backend_dir/compose.yaml" --env-file "$env_file" "$@"
  }
  # Never source/eval .env or print the resolved configuration, which has secrets.
  compose config --quiet
  # A temporary file lets us check both commands' exit status in POSIX sh.
  config_file=$(mktemp)
  chmod 600 "$config_file"
  trap 'rm -f -- "$config_file"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  compose config --format json >"$config_file"
  python3 "$backend_dir/deploy/prepare.py" <"$config_file"
  rm -f -- "$config_file"
  trap - EXIT INT TERM

  if [ "$build_images" = yes ]; then
    printf 'Building backend images for %s; current containers keep running...\n' "$revision"
    compose --progress "${BUILDKIT_PROGRESS:-plain}" build qdrant knowledge-service
  fi
  printf '%s\n' 'Starting the knowledge services and waiting for readiness...'
  if ! compose up --detach --no-build --wait --wait-timeout 120 qdrant knowledge-service; then
    printf '%s\n' 'Startup failed. Inspect this project with: docker compose -p course-knowledge logs --tail 100' >&2
    exit 1
  fi
  # The image healthcheck uses the default dev route; also verify the prod state.
  for site in futuredev.artz.zone future.artz.zone; do
    compose exec -T knowledge-service curl --fail --silent --show-error --max-time 10 \
      --header "Host: $site" http://127.0.0.1:8080/readyz >/dev/null
    printf 'Ready: %s\n' "$site"
  done
  compose ps qdrant knowledge-service
  printf 'Backend deployed: %s\n' "$revision"
  printf '%s\n' 'Configure Nginx separately using deploy/部署说明.md.'
}

# Parse the function before Git updates the file on disk.
main "$@"
