#!/usr/bin/env sh
# Start only the independent knowledge service and its private Qdrant instance.
set -eu

backend_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$backend_dir"

build_option=--build
env_file="$backend_dir/.env"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --no-build) build_option=--no-build; shift ;;
    --env-file)
      if [ "$#" -lt 2 ]; then
        printf '%s\n' '--env-file requires a file path.' >&2
        exit 2
      fi
      env_file=$2
      shift 2
      ;;
    -h|--help)
    printf '%s\n' 'Usage: sh start.sh [--env-file .env] [--no-build]' \
      'Starts Rust and Qdrant only; does not build or publish the frontend.' \
      'Linux/Docker: --no-build uses images that already exist locally.' \
      'Windows without Docker: starts local binaries; --no-build skips cargo build.'
    exit 0
    ;;
    *)
    printf '%s\n' 'Usage: sh start.sh [--env-file .env] [--no-build]' >&2
    exit 2
    ;;
  esac
done

if [ ! -f "$env_file" ]; then
  printf '%s\n' 'Copy env.example to the selected env file and configure it first.' >&2
  exit 1
fi

# Git Bash on Windows can run the preview without installing Docker.
# Linux and hosts with Docker retain the independent Compose deployment.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    if ! command -v docker >/dev/null 2>&1; then
      if command -v powershell.exe >/dev/null 2>&1; then
        powershell_command=powershell.exe
      elif command -v pwsh.exe >/dev/null 2>&1; then
        powershell_command=pwsh.exe
      else
        printf '%s\n' 'PowerShell is required for the Windows local preview.' >&2
        exit 1
      fi
      windows_env_file=$env_file
      if command -v cygpath >/dev/null 2>&1; then
        windows_env_file=$(cygpath -w "$env_file")
      fi
      if [ "$build_option" = --no-build ]; then
        exec "$powershell_command" -NoProfile -ExecutionPolicy Bypass -File ./start-local.ps1 -EnvFile "$windows_env_file" -NoBuild
      fi
      exec "$powershell_command" -NoProfile -ExecutionPolicy Bypass -File ./start-local.ps1 -EnvFile "$windows_env_file"
    fi
    ;;
esac

if ! command -v docker >/dev/null 2>&1; then
  printf '%s\n' 'Docker with the Compose plugin is required. See back_end/README.md.' >&2
  exit 1
fi
if ! docker compose version >/dev/null 2>&1; then
  printf '%s\n' 'The Docker Compose plugin is unavailable. See back_end/README.md.' >&2
  exit 1
fi

compose() {
  docker compose --project-directory "$backend_dir" --file "$backend_dir/compose.yaml" \
    --env-file "$env_file" "$@"
}

# --wait returns failure when either service fails its readiness check.
# The 120-second bound applies after build/start; image builds can take longer.
compose up --detach "$build_option" --wait --wait-timeout 120 qdrant knowledge-service
compose ps qdrant knowledge-service
printf '%s\n' 'Knowledge backend is ready. The frontend is published separately.'
