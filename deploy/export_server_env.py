"""Export local provider settings as a private, standalone server setup script.

The generated .env.server-init.sh contains credentials and is ignored by Git
and Docker. Only this exporter belongs in source control. No external packages
or network access are needed; the generated script requires Python 3 on Linux.
"""
import argparse
import base64
import inspect
import json
import os
from pathlib import Path
import re
import sys
import tempfile


BACKEND = Path(__file__).resolve().parents[1]
REQUIRED = ("ARK_API_KEY", "DEEPSEEK_API_KEY", "OSS_ACCESS_KEY_ID", "OSS_ACCESS_KEY_SECRET")


def parse_env(text):
    """Read literal, single-line dotenv values without evaluating shell code."""
    values = {}
    for number, line in enumerate(text.lstrip("\ufeff").splitlines(), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, separator, value = line.partition("=")
        name, value = name.strip(), value.strip()
        if not separator or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name):
            raise ValueError("Invalid env assignment at line {}.".format(number))
        if value.startswith(("'", '"')):
            if len(value) < 2 or value[-1] != value[0]:
                raise ValueError("Invalid env quoting at line {}.".format(number))
            value = value[1:-1]
        # This limited format is shared with the Rust startup parser. Reject
        # values that would need different escaping in Compose and Rust.
        if any(char in value for char in ("'", "\r", "\n", "\x00")):
            raise ValueError("Unsupported env characters at line {}.".format(number))
        values[name] = value
    return values


# This code is copied into the private export so the server does not need a Git
# update just to initialize .env. Keep it independent of the exporter module.
SERVER_CODE = r'''
import base64
import binascii
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import secrets
import sys
import tempfile

__PARSE_ENV__


def configure(directory, preset):
    target = directory / ".env"
    if target.is_symlink():
        raise ValueError("Refusing to replace a symlink .env.")
    original = target.read_text(encoding="utf-8-sig") if target.exists() else None
    existing = parse_env(original) if original is not None else {}
    values = dict(preset)
    # Preserve existing credentials, the persistent data path, custom database
    # overrides and other server settings. Fill empty template entries only.
    values.update({name: value for name, value in existing.items() if value or name not in values})
    root = Path(values["KB_HOST_DATA_DIR"])
    if not root.is_absolute():
        root = directory / root
    key = existing.get("ENCRYPTION_MASTER_KEY", "")
    if not key:
        if any(existing.get(name) for name in ("KB_DEV_DATABASE_URL", "KB_PROD_DATABASE_URL", "KB_DATABASE_URL")):
            raise ValueError("Custom database paths found. Set their original ENCRYPTION_MASTER_KEY before running this script.")
        roots = {root, directory / "data"}
        if any(path.is_file() for data_root in roots if data_root.exists()
               for pattern in ("*.sqlite*", "*.db*") for path in data_root.rglob(pattern)):
            raise ValueError("Existing database found. Restore its original ENCRYPTION_MASTER_KEY before running this script.")
        key = base64.b64encode(secrets.token_bytes(32)).decode("ascii")
    try:
        decoded = base64.b64decode(key, validate=True)
    except (ValueError, binascii.Error):
        decoded = b""
    if len(decoded) != 32:
        raise ValueError("Existing ENCRYPTION_MASTER_KEY is invalid; it was not replaced.")
    values["ENCRYPTION_MASTER_KEY"] = key
    if values.get("KB_STORAGE_BACKEND") != "oss":
        raise ValueError("KB_STORAGE_BACKEND must be oss.")
    for name in ("ARK_API_KEY", "DEEPSEEK_API_KEY", "OSS_ACCESS_KEY_ID", "OSS_ACCESS_KEY_SECRET"):
        if not values.get(name):
            raise ValueError("Missing required setting: " + name)
    text = "# Server runtime settings. Contains credentials; never commit this file.\n"
    text += "# Keep ENCRYPTION_MASTER_KEY and KB_HOST_DATA_DIR unchanged on later deployments.\n"
    # Compose single quotes keep $, backticks and spaces literal. Never source
    # or eval this file, and never print its contents in diagnostics.
    text += "".join("{}='{}'\n".format(name, value) for name, value in values.items())
    if original == text:
        target.chmod(0o600)
        print(".env is already configured; credentials and master key are unchanged.")
        return
    backup = None
    if original is not None:
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S-%f")
        backup = directory / (".env.backup-" + stamp)
        descriptor = os.open(str(backup), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(target.read_bytes())
    descriptor, temporary = tempfile.mkstemp(prefix=".env.write-", dir=str(directory))
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, target)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    print("Configured .env (mode 600). Provider keys are not printed.")
    if backup:
        print("Previous configuration backed up as " + backup.name)
    print("Data directory: " + values["KB_HOST_DATA_DIR"])
    print("Dev: futuredev.artz.zone / OSS " + values["OSS_DEV_BUCKET"])
    print("Prod: future.artz.zone / OSS " + values["OSS_PROD_BUCKET"])
    print("Next: sh deploy.sh. Nginx is configured separately.")


def main():
    if len(sys.argv) != 2:
        print("Usage: sh .env.server-init.sh (writes .env next to the script)", file=sys.stderr)
        return 2
    try:
        configure(Path(sys.argv[1]), PRESET)
    except (ValueError, OSError) as error:
        message = str(error) if isinstance(error, ValueError) else "Unable to read or write server configuration."
        print(message, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
'''


def server_preset(example, source):
    defaults = parse_env(example)
    current = parse_env(source)
    # Only copy variables supported by Compose, never Windows tool paths,
    # local database locations or the local database's encryption master key.
    values = {name: current.get(name, value) for name, value in defaults.items()
              if name != "ENCRYPTION_MASTER_KEY"}
    for name in ("ASR_BASE_URL", "ASR_MODEL", "ASR_API_KEY", "QDRANT_API_KEY", "RUST_LOG"):
        if name in current:
            values[name] = current[name]
    values["KB_HOST_DATA_DIR"] = "/srv/course-knowledge-data"
    values["COMPOSE_PROJECT_NAME"] = "course-knowledge"
    values["KB_STORAGE_BACKEND"] = "oss"
    missing = [name for name in REQUIRED if not values.get(name)]
    if missing:
        raise ValueError("Configure the local .env first: " + ", ".join(missing))
    return values


def render_script(preset):
    # Base64 is a transport encoding, not encryption. The complete file is a
    # private credential file, just like .env, even though it is executable.
    encoded = base64.b64encode(json.dumps(preset, ensure_ascii=True).encode("utf-8")).decode("ascii")
    code = SERVER_CODE.replace("__PARSE_ENV__", inspect.getsource(parse_env).rstrip())
    code = code.replace("\ndef configure(", '\nPRESET = json.loads(base64.b64decode("' + encoded + '"))\n\ndef configure(', 1)
    return (
        "#!/usr/bin/env sh\n"
        "# PRIVATE: contains provider credentials. Never commit or publish.\n"
        "set -eu\n"
        "umask 077\n"
        "command -v python3 >/dev/null 2>&1 || { printf '%s\\n' 'Python 3 is required.' >&2; exit 1; }\n"
        'kb_env_script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\n'
        'exec python3 - "$kb_env_script_dir" "$@" <<\'KB_SERVER_ENV_PYTHON\'\n'
        + code + "\nKB_SERVER_ENV_PYTHON\n"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=BACKEND / ".env")
    args = parser.parse_args()
    output = BACKEND / ".env.server-init.sh"
    try:
        preset = server_preset((BACKEND / "env.example").read_text(encoding="utf-8-sig"),
                               args.source.read_text(encoding="utf-8-sig"))
        script = render_script(preset)
        descriptor, temporary = tempfile.mkstemp(prefix=".env.export-", dir=str(BACKEND))
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as stream:
                stream.write(script)
            os.replace(temporary, output)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        output.chmod(0o600)
    except (ValueError, OSError) as error:
        message = str(error) if isinstance(error, ValueError) else "Unable to read local configuration or write the private script."
        print(message, file=sys.stderr)
        return 1
    print("Created .env.server-init.sh with current provider settings (contains credentials; do not commit).")
    print("Copy it to the server backend directory, then run: sh .env.server-init.sh")
    return 0


if __name__ == "__main__":
    sys.exit(main())
