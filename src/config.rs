use std::{env, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use reqwest::Url;

#[derive(Clone)]
pub struct AppConfig {
    pub environment: Environment,
    pub bind_addr: SocketAddr,
    pub database_url: String,
    pub qdrant: QdrantConfig,
    pub auth: AuthConfig,
    pub storage: StorageConfig,
}

#[derive(Clone, Debug)]
pub struct QdrantConfig {
    /// REST endpoint used for health checks and the initial adapter path.
    pub http_url: Url,
    /// Kept separately so the vector store can switch to the official gRPC client
    /// without changing application configuration.
    pub grpc_url: String,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// Existing backend URL. Leave empty only when the auth adapter is not used yet.
    pub base_url: Option<Url>,
    pub me_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Environment {
    Development,
    Production,
}
impl Environment {
    pub fn name(self) -> &'static str {
        match self {
            Self::Development => "dev",
            Self::Production => "prod",
        }
    }
}

/// Course resources and user attachments always use private OSS storage.
/// The local directory is disposable processing cache, never a fallback store.
#[derive(Clone)]
pub enum StorageConfig {
    #[cfg(test)]
    Local { local_dir: PathBuf },
    Oss {
        local_dir: PathBuf,
        endpoint: Box<Url>,
        region: String,
        bucket: String,
        access_key_id: String,
        access_key_secret: String,
        prefix: String,
    },
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Self::for_environment(Environment::Development)
    }

    pub fn for_environment(environment: Environment) -> Result<Self> {
        let bind_addr = read_env("KB_BIND_ADDR", "127.0.0.1:8080")
            .parse()
            .context("KB_BIND_ADDR must be a valid socket address")?;
        let data_dir = env::var_os("KB_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        let prod = environment == Environment::Production;
        let database_url = optional_env(if prod {
            "KB_PROD_DATABASE_URL"
        } else {
            "KB_DEV_DATABASE_URL"
        })
        .or_else(|| {
            if prod {
                None
            } else {
                optional_env("KB_DATABASE_URL")
            }
        })
        .unwrap_or_else(|| {
            format!(
                "sqlite://{}/knowledge.sqlite?mode=rwc",
                data_dir
                    .join(environment.name())
                    .to_string_lossy()
                    .replace('\\', "/")
            )
        });
        let local_dir = if !prod {
            optional_env("KB_UPLOADS_DIR").map(PathBuf::from)
        } else {
            None
        }
        .unwrap_or_else(|| {
            env::var_os("KB_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| data_dir.join("cache"))
                .join(environment.name())
        });
        if optional_env("KB_STORAGE_BACKEND").is_some_and(|value| value != "oss") {
            anyhow::bail!(
                "KB_STORAGE_BACKEND must be oss; local resource storage is not supported"
            );
        }
        let endpoint = parse_url(
            "OSS_ENDPOINT",
            &read_env("OSS_ENDPOINT", "https://oss-cn-beijing.aliyuncs.com"),
        )?;
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || endpoint.port().is_some()
            || endpoint.path() != "/"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            anyhow::bail!(
                "OSS_ENDPOINT must be an HTTPS origin without a path, credentials, port or query"
            );
        }
        let region = read_env("OSS_REGION", "cn-beijing");
        if region.is_empty()
            || region.len() > 64
            || region
                .bytes()
                .any(|b| !(b.is_ascii_alphanumeric() || b == b'-'))
        {
            anyhow::bail!("OSS_REGION is invalid");
        }
        let bucket = read_env(
            if prod {
                "OSS_PROD_BUCKET"
            } else {
                "OSS_DEV_BUCKET"
            },
            if prod { "futureprod" } else { "futuredev" },
        );
        if !(3..=63).contains(&bucket.len())
            || !bucket.as_bytes()[0].is_ascii_alphanumeric()
            || !bucket.as_bytes()[bucket.len() - 1].is_ascii_alphanumeric()
            || bucket
                .bytes()
                .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        {
            anyhow::bail!("OSS bucket name is invalid");
        }
        let storage = StorageConfig::Oss {
            local_dir,
            endpoint: Box::new(endpoint),
            region,
            bucket,
            access_key_id: required_env("OSS_ACCESS_KEY_ID")?,
            access_key_secret: required_env("OSS_ACCESS_KEY_SECRET")?,
            prefix: "knowledge/".into(),
        };

        let http_url = parse_url(
            "QDRANT_HTTP_URL",
            &read_env("QDRANT_HTTP_URL", "http://127.0.0.1:6333"),
        )?;
        let grpc_url = read_env("QDRANT_GRPC_URL", "http://127.0.0.1:6334");
        let api_key = optional_env("QDRANT_API_KEY");

        let base_url = Some(parse_url(
            "AUTH_UPSTREAM_BASE_URL",
            &optional_env(if prod {
                "AUTH_PROD_UPSTREAM_BASE_URL"
            } else {
                "AUTH_DEV_UPSTREAM_BASE_URL"
            })
            .or_else(|| {
                if prod {
                    None
                } else {
                    optional_env("AUTH_UPSTREAM_BASE_URL")
                }
            })
            .unwrap_or_else(|| {
                if prod {
                    "https://future.artz.zone/api".into()
                } else {
                    "https://futuredev.artz.zone/api".into()
                }
            }),
        )?);
        let me_path = read_env("AUTH_UPSTREAM_ME_PATH", "/auth/me");

        Ok(Self {
            environment,
            bind_addr,
            database_url,
            qdrant: QdrantConfig {
                http_url,
                grpc_url,
                api_key,
            },
            auth: AuthConfig { base_url, me_path },
            storage,
        })
    }
}

fn read_env(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn required_env(name: &str) -> Result<String> {
    optional_env(name)
        .ok_or_else(|| anyhow::anyhow!("{name} is required when OSS storage is enabled"))
}

fn parse_url(name: &str, value: &str) -> Result<Url> {
    Url::parse(value).with_context(|| format!("{name} must be a valid URL"))
}

#[cfg(test)]
impl AppConfig {
    pub fn test_config() -> Self {
        Self {
            environment: Environment::Development,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            database_url: "sqlite::memory:".into(),
            qdrant: QdrantConfig {
                http_url: "http://127.0.0.1:6333".parse().unwrap(),
                grpc_url: "http://127.0.0.1:6334".into(),
                api_key: None,
            },
            auth: AuthConfig {
                base_url: None,
                me_path: "/auth/me".into(),
            },
            storage: StorageConfig::Local {
                local_dir: crate::ingest::uploads_dir(),
            },
        }
    }
}
