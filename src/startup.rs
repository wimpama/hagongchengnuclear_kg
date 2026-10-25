//! Development startup configuration; deployed containers use explicit env vars.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};

pub struct Options {
    pub local_backend: Option<PathBuf>,
}

pub fn configure(arguments: &[String]) -> Result<Option<Options>> {
    let mut env_file = None;
    let mut local_preview = default_local_mode(
        cfg!(debug_assertions),
        arguments.is_empty(),
        std::env::var_os("KB_BIND_ADDR").is_some() || std::env::var_os("QDRANT_HTTP_URL").is_some(),
    );
    let mut args = arguments.iter();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--env-file" => env_file = Some(args.next().context("--env-file requires a path")?),
            "--local-preview" => local_preview = true,
            "--help" | "-h" => {
                println!("course-kb-server [--env-file .env] [--local-preview]");
                println!("cargo run: reads .env, starts local Qdrant, then starts the API.");
                println!("--local-preview explicitly enables managed local startup (Qdrant 16333/16334).");
                println!(
                    "Preconfigured/release environments use their configured external Qdrant."
                );
                println!("migrate-storage --help: inspect or migrate existing resources to OSS.");
                return Ok(None);
            }
            _ => bail!("unknown option; use --help"),
        }
    }
    if let Some(path) = env_file
        .map(String::as_str)
        .or(local_preview.then_some(".env"))
    {
        // Validate the whole file before mutating process configuration. Values
        // are literal, never shell-evaluated, interpolated, or logged.
        let text = std::fs::read_to_string(path).context("read server env file")?;
        for (name, value) in parse_env_file(&text)? {
            std::env::set_var(name, value);
        }
    }
    let local_backend = if local_preview {
        let backend = std::env::current_dir()?;
        for (name, value) in local_settings(&backend) {
            std::env::set_var(name, value);
        }
        Some(backend)
    } else {
        None
    };
    Ok(Some(Options { local_backend }))
}

fn default_local_mode(debug_build: bool, no_arguments: bool, externally_configured: bool) -> bool {
    debug_build && no_arguments && !externally_configured
}

fn parse_env_file(text: &str) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (index, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .with_context(|| format!("invalid env assignment at line {}", index + 1))?;
        let name = name.trim();
        let supported = [
            "AUTH_",
            "ARK_",
            "LLM_",
            "DEEPSEEK_",
            "ASR_",
            "KB_",
            "QDRANT_",
            "OSS_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
            || matches!(
                name,
                "COMPOSE_PROJECT_NAME"
                    | "ENCRYPTION_MASTER_KEY"
                    | "RUST_LOG"
                    | "TOKIO_WORKER_THREADS"
            );
        if !supported || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bail!("unsupported env variable at line {}", index + 1);
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        result.insert(name.to_owned(), value.to_owned());
    }
    Ok(result)
}

fn local_settings(backend: &Path) -> BTreeMap<String, String> {
    let local = backend.join("data/local");
    let path_text = |path: &Path| path.to_string_lossy().replace('\\', "/");
    let db = |path: &Path| format!("sqlite://{}?mode=rwc", path_text(path));
    BTreeMap::from([
        ("KB_BIND_ADDR".into(), "127.0.0.1:8080".into()),
        (
            "KB_DEV_DATABASE_URL".into(),
            db(&local.join("knowledge.sqlite")),
        ),
        (
            "KB_PROD_DATABASE_URL".into(),
            db(&local.join("prod/knowledge.sqlite")),
        ),
        ("KB_UPLOADS_DIR".into(), String::new()),
        ("KB_CACHE_DIR".into(), path_text(&local.join("cache"))),
        ("KB_AI_LOG_DIR".into(), path_text(&local.join("logs/ai"))),
        ("KB_STORAGE_BACKEND".into(), "oss".into()),
        ("QDRANT_HTTP_URL".into(), "http://127.0.0.1:16333".into()),
        ("QDRANT_GRPC_URL".into(), "http://127.0.0.1:16334".into()),
        ("QDRANT_API_KEY".into(), String::new()),
        ("TOKIO_WORKER_THREADS".into(), "2".into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_defaults_do_not_take_over_container_or_script_configuration() {
        assert!(default_local_mode(true, true, false));
        assert!(!default_local_mode(false, true, false));
        assert!(!default_local_mode(true, true, true));
        assert!(!default_local_mode(true, false, false));
    }

    #[test]
    fn env_values_are_literal_and_cannot_change_shell_or_system_settings() {
        let values = parse_env_file("\u{feff}# settings\r\nLLM_API_KEY='literal$()=`x`'\r\nKB_HOST_DATA_DIR=C:/space dir/data\r\n").unwrap();
        assert_eq!(values["LLM_API_KEY"], "literal$()=`x`");
        assert_eq!(values["KB_HOST_DATA_DIR"], "C:/space dir/data");
        for invalid in [
            "PATH=bad",
            "HOME=bad",
            "KB_KEY;echo=bad",
            "not-an-assignment",
        ] {
            assert!(parse_env_file(invalid).is_err());
        }
    }

    #[test]
    fn direct_local_run_keeps_the_existing_preview_database_and_tenant_isolation() {
        let values = local_settings(Path::new("/workspace/backend"));
        assert_eq!(
            values["KB_DEV_DATABASE_URL"],
            "sqlite:///workspace/backend/data/local/knowledge.sqlite?mode=rwc"
        );
        assert_eq!(
            values["KB_PROD_DATABASE_URL"],
            "sqlite:///workspace/backend/data/local/prod/knowledge.sqlite?mode=rwc"
        );
        assert_eq!(values["QDRANT_HTTP_URL"], "http://127.0.0.1:16333");
        assert_eq!(
            values["KB_AI_LOG_DIR"],
            "/workspace/backend/data/local/logs/ai"
        );
        assert_eq!(values["KB_STORAGE_BACKEND"], "oss");
        assert!(values["KB_UPLOADS_DIR"].is_empty());
    }
}
