"""Validate resolved Compose config and prepare only this service's data paths.

Input comes from `docker compose config --format json`, never from a sourced
shell env file. It contains credentials: do not log the input or field values.
No external dependencies. Existing files and directory ownership stay intact.
"""
import base64
import binascii
import json
import os
from pathlib import Path
import socket
import subprocess
import sys


class ConfigurationError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ConfigurationError(message)


def writable_by_service(path):
    info = path.stat()
    mode = info.st_mode
    bits = (mode >> 6) if info.st_uid == 10001 else ((mode >> 3) if info.st_gid == 10001 else mode)
    needed = 0o7 if path.is_dir() else 0o6
    return bits & needed == needed


def validate(config):
    require(config.get("name") == "course-knowledge", "Use the separate course-knowledge Compose project.")
    services = config["services"]
    require(set(services) == {"knowledge-service", "qdrant"}, "Unexpected Compose services; deployment stopped.")
    service = services["knowledge-service"]
    environment = service.get("environment", {})
    for name in ("OSS_ACCESS_KEY_ID", "OSS_ACCESS_KEY_SECRET", "ARK_API_KEY", "ENCRYPTION_MASTER_KEY"):
        require(bool(str(environment.get(name) or "").strip()), "Configure {} in the selected env file.".format(name))
    llm_key = "DEEPSEEK_API_KEY" if environment.get("LLM_PROVIDER", "deepseek") == "deepseek" else "LLM_API_KEY"
    require(bool(environment.get(llm_key) or environment.get("LLM_API_KEY")), "Configure {} in the selected env file.".format(llm_key))
    try:
        key = base64.b64decode(environment["ENCRYPTION_MASTER_KEY"], validate=True)
    except (ValueError, binascii.Error):
        key = b""
    require(len(key) == 32, "ENCRYPTION_MASTER_KEY must be the persistent base64-encoded 32-byte key.")
    ports = service.get("ports", [])
    require(len(ports) == 1 and ports[0].get("host_ip") == "127.0.0.1" and int(ports[0]["target"]) == 8080,
            "Publish only the Rust API on 127.0.0.1; container target port must be 8080.")
    port = int(ports[0]["published"])
    require(1 <= port <= 65535, "KB_PORT is invalid.")
    require(not services["qdrant"].get("ports"), "Qdrant must stay on the private Compose network.")
    volumes = [v for v in service.get("volumes", []) if v.get("target") == "/var/lib/knowledge-service/data" and v.get("type") == "bind"]
    require(len(volumes) == 1, "Configure a persistent bind directory with KB_HOST_DATA_DIR.")
    root = Path(volumes[0]["source"]).resolve()
    require(root.is_absolute() and root.parent != root and len(root.parts) >= 3,
            "KB_HOST_DATA_DIR must be a dedicated directory, for example /srv/course-knowledge-data.")
    qdrant = [v for v in services["qdrant"].get("volumes", []) if v.get("target") == "/qdrant/storage" and v.get("type") == "bind"]
    require(len(qdrant) == 1 and Path(qdrant[0]["source"]).resolve() == root / "qdrant",
            "Qdrant storage must be the qdrant child of KB_HOST_DATA_DIR.")
    paths = [root, root / "dev", root / "prod", root / "cache", root / "qdrant"]
    for path in paths:
        require(path.resolve() == path, "Data directories must not redirect to other paths through symlinks.")
        require(not path.exists() or path.is_dir(), "A required data directory is occupied by a file.")
    # Check before mkdir or container replacement; distinguish our running API
    # from an unrelated service already using the requested host port.
    probe = socket.socket()
    try:
        probe.bind(("127.0.0.1", port))
    except OSError:
        result = subprocess.run(
            ["docker", "ps", "--quiet", "--filter", "label=com.docker.compose.project=course-knowledge",
             "--filter", "label=com.docker.compose.service=knowledge-service"],
            check=True, capture_output=True, text=True,
        )
        ids = result.stdout.split()
        allowed = False
        if ids:
            inspected = subprocess.run(["docker", "inspect"] + ids, check=True, capture_output=True, text=True)
            for container in json.loads(inspected.stdout):
                mappings = (container.get("NetworkSettings", {}).get("Ports") or {}).get("8080/tcp") or []
                allowed |= any(m.get("HostIp") == "127.0.0.1" and m.get("HostPort") == str(port) for m in mappings)
        require(allowed, "KB_PORT is occupied by another service. Select a free port and adjust the Nginx snippet.")
    finally:
        probe.close()
    return root, paths


def prepare(root, paths):
    for path in paths:
        created = not path.exists()
        path.mkdir(mode=0o750, parents=True, exist_ok=True)
        if created and getattr(os, "geteuid", lambda: -1)() == 0 and path != root / "qdrant":
            os.chown(path, 10001, 10001)
        if path != root / "qdrant":
            require(writable_by_service(path),
                    "Rust data directory must allow uid/gid 10001 to read and write. See deployment instructions.")
    # SQLite WAL also needs a writable parent. Never recursively chown existing data.
    for path in (root / "dev", root / "prod"):
        for database in path.glob("knowledge.sqlite*"):
            require(database.is_file() and writable_by_service(database),
                    "Existing SQLite files are not writable by uid/gid 10001; restore their intended ownership first.")


def main():
    try:
        config = json.load(sys.stdin)
        root, paths = validate(config)
        prepare(root, paths)
    except (ConfigurationError, KeyError, ValueError, OSError, subprocess.SubprocessError):
        # Only ConfigurationError has messages written by us; provider keys and
        # subprocess output must never be included in exception diagnostics.
        error = sys.exc_info()[1]
        message = str(error) if isinstance(error, ConfigurationError) else "Unable to validate deployment configuration or data directories."
        print(message, file=sys.stderr)
        return 1
    print("Deployment configuration and data directories are ready.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
