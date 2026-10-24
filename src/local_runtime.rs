//! Qdrant lifecycle for one-command local development, never container startup.
use std::{
    fs::{File, OpenOptions},
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::{
    process::{Child, Command},
    time::{sleep, Instant},
};
use uuid::Uuid;

pub struct LocalRuntime {
    child: Child,
    metadata: PathBuf,
    _lock: File,
}

impl LocalRuntime {
    pub async fn start(backend: &Path) -> Result<Self> {
        let binary = std::env::var_os("KB_QDRANT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                backend.join("data/tools/qdrant").join(if cfg!(windows) {
                    "qdrant.exe"
                } else {
                    "qdrant"
                })
            });
        if !binary.is_file() {
            bail!(
                "Qdrant binary is missing. Install it in data/tools/qdrant or set KB_QDRANT_BIN."
            );
        }
        let local = backend.join("data/local");
        std::fs::create_dir_all(&local).context("create local database directory")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(local.join("service.lock"))
            .context("open local service lock")?;
        lock.try_lock()
            .context("another managed local knowledge service is already running")?;
        for port in [8080, 16333, 16334] {
            check_free_port(port)?;
        }
        let qdrant_dir = local.join("qdrant");
        let logs = local.join("logs");
        for directory in [
            &qdrant_dir,
            &logs,
            &local.join("cache/dev"),
            &local.join("cache/prod"),
        ] {
            std::fs::create_dir_all(directory).context("create local runtime directory")?;
        }
        let run = Uuid::new_v4().to_string();
        let config = qdrant_dir.join("cargo.yaml");
        std::fs::write(&config, format!(
            "storage:\n  storage_path: ./storage\n  snapshots_path: ./snapshots/{run}\nservice:\n  host: 127.0.0.1\n  http_port: 16333\n  grpc_port: 16334\ntelemetry_disabled: true\n"
        )).context("write local Qdrant configuration")?;
        let stdout = File::create(logs.join(format!("{run}-qdrant.log")))?;
        let stderr = File::create(logs.join(format!("{run}-qdrant-error.log")))?;
        let mut command = Command::new(binary);
        command
            .arg("--config-path")
            .arg(config)
            .current_dir(qdrant_dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        // Keep provider credentials and unrelated Qdrant overrides out of this
        // local dependency's environment. Windows needs these OS path variables.
        command.env_clear();
        for name in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "LANG",
            "LC_ALL",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = command.spawn().context("start local Qdrant")?;
        let mut runtime = Self {
            child,
            metadata: local.join("runtime.json"),
            _lock: lock,
        };
        let ready = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        runtime
            .wait_ready(&ready, Duration::from_secs(30), &logs)
            .await?;
        std::fs::write(
            &runtime.metadata,
            serde_json::to_vec_pretty(&json!({
                "serverPid":std::process::id(), "qdrantPid":runtime.child.id(),
                "dataDir":local, "startedAt":chrono::Utc::now().to_rfc3339(), "manager":"rust"
            }))?,
        )
        .context("write local runtime metadata")?;
        tracing::info!("Qdrant is ready; uploaded originals use OSS; local files contain databases, logs and processing cache");
        Ok(runtime)
    }

    async fn wait_ready(
        &mut self,
        client: &reqwest::Client,
        limit: Duration,
        logs: &Path,
    ) -> Result<()> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!(
                    "Qdrant exited during startup ({status}); see {}",
                    logs.display()
                );
            }
            if client
                .get("http://127.0.0.1:16333/readyz")
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "Qdrant did not become ready within 30 seconds; see {}",
                    logs.display()
                );
            }
            sleep(Duration::from_millis(150)).await;
        }
    }

    pub async fn exited(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub async fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child
                .kill()
                .await
                .context("stop the Qdrant process started by this application")?;
        }
        self.remove_metadata();
        Ok(())
    }

    fn remove_metadata(&self) {
        // A failed concurrent start must never remove someone else's metadata.
        if std::fs::read(&self.metadata)
            .ok()
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .and_then(|v| v.get("serverPid").and_then(|id| id.as_u64()))
            == Some(u64::from(std::process::id()))
        {
            let _ = std::fs::remove_file(&self.metadata);
        }
    }
}

impl Drop for LocalRuntime {
    fn drop(&mut self) {
        self.remove_metadata();
        // Child is configured with kill_on_drop for startup errors/cancellation.
    }
}

fn check_free_port(port: u16) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).with_context(|| {
        format!("local port {port} is occupied; stop the previous knowledge startup command first")
    })?;
    drop(listener);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_port_owned_by_an_existing_process() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(check_free_port(listener.local_addr().unwrap().port()).is_err());
    }
}
