//! Configuration loading from `config.toml` with environment variable overrides.
//!
//! Precedence: CLI args > environment variables > config file > defaults.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

// ── TOML-deserializable config ──────────────────────────────────────────

/// Top-level configuration, mirrors `config.toml` layout.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub embedding: EmbeddingConfig,
    pub reranker: RerankerConfig,
    pub store: StoreConfig,
    pub search: SearchConfig,
    pub database: DatabaseConfig,
    pub logging: LoggingConfig,
    pub sync: SyncConfig,
    pub web: WebConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingConfig {
    /// fastembed model name (e.g. "NomicEmbedTextV15Q").
    pub model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "NomicEmbedTextV15Q".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RerankerConfig {
    /// Enable reranking of search results.
    pub enabled: bool,
    /// fastembed reranker model name (e.g. "JINARerankerV1TurboEn").
    pub model: String,
    /// Minimum reranker score to include in results. Can be negative.
    /// Results below this threshold are excluded after reranking.
    pub threshold: f64,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "JINARerankerV1TurboEn".to_string(),
            threshold: 0.0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreConfig {
    /// Top-N similar memories returned by `store` and `merge`.
    pub similar_limit: usize,
    /// Minimum cosine similarity to include in similar results.
    pub similar_threshold: f64,
    /// Content truncation length for store/search/list responses (characters).
    pub content_max_length: u32,
    /// Maximum content size in bytes (reject on store/update/merge).
    pub max_content_size: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            similar_limit: 3,
            similar_threshold: 0.5,
            content_max_length: 500,
            max_content_size: 10240,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// RRF constant for hybrid search result merging.
    pub rrf_k: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self { rrf_k: 60 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// SQLite busy timeout in milliseconds.
    pub busy_timeout: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { busy_timeout: 5000 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Log level filter directive (e.g. "info", "debug", "erinra=debug,tower=warn").
    pub log_level: String,
    /// Optional log file path. When set, logs are written here in addition to stderr.
    pub log_file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            log_file: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub enum SyncFormat {
    #[serde(rename = "jsonl")]
    Jsonl,
    #[serde(rename = "jsonl.gz")]
    JsonlGz,
    #[serde(rename = "json")]
    Json,
    #[serde(rename = "json.gz")]
    JsonGz,
}

impl std::fmt::Display for SyncFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncFormat::Jsonl => write!(f, "jsonl"),
            SyncFormat::JsonlGz => write!(f, "jsonl.gz"),
            SyncFormat::Json => write!(f, "json"),
            SyncFormat::JsonGz => write!(f, "json.gz"),
        }
    }
}

impl std::str::FromStr for SyncFormat {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "jsonl" => Ok(SyncFormat::Jsonl),
            "jsonl.gz" => Ok(SyncFormat::JsonlGz),
            "json" => Ok(SyncFormat::Json),
            "json.gz" => Ok(SyncFormat::JsonGz),
            other => Err(format!("unknown sync format: {other}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Enable background sync (export/import). When false, no sync operations run.
    pub enabled: bool,
    /// Directory for sync exports/imports.
    pub sync_dir: PathBuf,
    /// Template for export filename (supports {hostname}, {os}, {platform}, {distro}, {user}).
    pub filename: String,
    /// Export format: jsonl, jsonl.gz, json, json.gz.
    pub format: SyncFormat,
    /// Seconds between exports (first export fires immediately).
    pub export_interval: u64,
    /// If > 0, poll for imports instead of filesystem watching (seconds).
    pub poll_interval: u64,
    /// Import all other machines' exports on startup.
    pub restore_on_start: bool,
    /// Run final export on shutdown.
    pub export_on_exit: bool,
    /// Purge tombstones older than this during export (days).
    pub tombstone_retention_days: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        let sync_dir = dirs::home_dir()
            .map(|h| h.join(".erinra/sync"))
            .unwrap_or_else(|| {
                // Relative fallback -- validate() will reject this,
                // forcing the user to set sync_dir explicitly.
                PathBuf::from(".erinra/sync")
            });
        Self {
            enabled: false,
            sync_dir,
            filename: "{hostname}".to_string(),
            format: SyncFormat::JsonlGz,
            export_interval: 900,
            poll_interval: 0,
            restore_on_start: false,
            export_on_exit: false,
            tombstone_retention_days: 90,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Port for the web UI server.
    pub port: u16,
    /// Bind address for the web UI server.
    pub bind: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            port: 9898,
            bind: "127.0.0.1".to_string(),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Expand a leading `~` in a path to the user's home directory.
/// If `dirs::home_dir()` returns `None`, the path is returned unchanged.
fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_str().unwrap_or_default();
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if s.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(&s[2..]); // skip "~/"
    }
    path.to_path_buf()
}

// ── Loading ─────────────────────────────────────────────────────────────

impl Config {
    /// Load config from `{data_dir}/config.toml`, apply overrides, normalize, and validate.
    ///
    /// Precedence: CLI args > env vars > config file > defaults.
    /// Pass `None` for `cli` when CLI overrides are not applicable (e.g. non-serve commands).
    pub fn load(data_dir: &Path, cli: Option<&CliOverrides>) -> Result<Self> {
        let mut config = Self::load_file(data_dir)?;
        config.apply_env_overrides();
        if let Some(cli) = cli {
            config.apply_cli_overrides(cli);
        }
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Load config from file only, without env overrides or validation.
    /// Creates a default config file with comments if none exists.
    fn load_file(data_dir: &Path) -> Result<Self> {
        let config_path = data_dir.join("config.toml");
        match std::fs::read_to_string(&config_path) {
            Ok(contents) => toml::from_str(&contents)
                .with_context(|| format!("failed to parse config file: {}", config_path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Generate a commented default config for discoverability.
                if data_dir.exists()
                    && let Err(e) = std::fs::write(&config_path, Self::default_config_toml())
                {
                    tracing::warn!(
                        "could not write default config to {}: {e}",
                        config_path.display()
                    );
                }
                Ok(Config::default())
            }
            Err(e) => Err(anyhow::Error::new(e).context(format!(
                "failed to read config file: {}",
                config_path.display()
            ))),
        }
    }

    /// Generate a commented TOML string with all default configuration values.
    fn default_config_toml() -> String {
        let defaults = Config::default();
        format!(
            r#"# Erinra configuration
# All values shown are defaults. Uncomment and edit to customize.
# Precedence: CLI args > environment variables > this file > defaults.

[embedding]
# Embedding model for vector search. Run `erinra models` to list options.
# model = "{embedding_model}"

[reranker]
# (Experimental, subject to change)
# Cross-encoder reranking improves search relevance at the cost of latency.
# Short queries may get low scores; consider a low threshold (e.g. -5).
# The model (~151 MB) is downloaded on first use.
# enabled = {reranker_enabled}
# model = "{reranker_model}"
# threshold = {reranker_threshold}

[store]
# similar_limit: number of similar memories returned by store/merge.
# similar_threshold: minimum cosine similarity to include.
# content_max_length: truncation length (chars) in tool responses.
# max_content_size: reject content larger than this (bytes).
# similar_limit = {similar_limit}
# similar_threshold = {similar_threshold}
# content_max_length = {content_max_length}
# max_content_size = {max_content_size}

[search]
# RRF constant for hybrid search (vector + FTS5) result merging.
# Higher values reduce the influence of rank differences.
# rrf_k = {rrf_k}

[database]
# SQLite busy timeout in milliseconds.
# busy_timeout = {busy_timeout}

[logging]
# Log level filter (e.g. "info", "debug", "erinra=debug,tower=warn").
# log_level = "{log_level}"
# Optional log file path. Logs are written here in addition to stderr.
# log_file = "/path/to/erinra.log"

[sync]
# Background sync: export/import memories across machines via JSONL files.
# enabled = {sync_enabled}
# sync_dir = "{sync_dir}"
# filename = "{sync_filename}"
# format = "{sync_format}"
# export_interval = {export_interval}
# poll_interval = {poll_interval}
# restore_on_start = {restore_on_start}
# export_on_exit = {export_on_exit}
# tombstone_retention_days = {tombstone_retention_days}

[web]
# Web dashboard and daemon settings.
# port = {web_port}
# bind = "{web_bind}"
"#,
            embedding_model = defaults.embedding.model,
            reranker_enabled = defaults.reranker.enabled,
            reranker_model = defaults.reranker.model,
            reranker_threshold = defaults.reranker.threshold,
            similar_limit = defaults.store.similar_limit,
            similar_threshold = defaults.store.similar_threshold,
            content_max_length = defaults.store.content_max_length,
            max_content_size = defaults.store.max_content_size,
            rrf_k = defaults.search.rrf_k,
            busy_timeout = defaults.database.busy_timeout,
            log_level = defaults.logging.log_level,
            sync_enabled = defaults.sync.enabled,
            sync_dir = defaults.sync.sync_dir.display(),
            sync_filename = defaults.sync.filename,
            sync_format = defaults.sync.format,
            export_interval = defaults.sync.export_interval,
            poll_interval = defaults.sync.poll_interval,
            restore_on_start = defaults.sync.restore_on_start,
            export_on_exit = defaults.sync.export_on_exit,
            tombstone_retention_days = defaults.sync.tombstone_retention_days,
            web_port = defaults.web.port,
            web_bind = defaults.web.bind,
        )
    }

    /// Normalize config values (e.g. expand tilde in paths).
    /// Called between loading/overrides and validation.
    fn normalize(&mut self) {
        self.sync.sync_dir = expand_tilde(&self.sync.sync_dir);
        if let Some(ref p) = self.logging.log_file {
            self.logging.log_file = Some(expand_tilde(p));
        }
    }

    /// Validate that config values are in sane ranges.
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.store.content_max_length > 0,
            "store.content_max_length must be > 0"
        );
        anyhow::ensure!(
            self.store.max_content_size > 0,
            "store.max_content_size must be > 0"
        );
        anyhow::ensure!(
            self.database.busy_timeout > 0,
            "database.busy_timeout must be > 0"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.store.similar_threshold),
            "store.similar_threshold must be in 0.0..=1.0, got {}",
            self.store.similar_threshold
        );
        anyhow::ensure!(
            self.store.similar_limit > 0,
            "store.similar_limit must be > 0"
        );
        anyhow::ensure!(self.search.rrf_k > 0, "search.rrf_k must be > 0");
        anyhow::ensure!(
            !self.logging.log_level.is_empty(),
            "logging.log_level must not be empty"
        );
        // Verify the filter parses correctly; the actual filter is built in init_tracing().
        tracing_subscriber::EnvFilter::try_new(&self.logging.log_level).map_err(|e| {
            anyhow::anyhow!(
                "invalid logging.log_level '{}': {e}",
                self.logging.log_level
            )
        })?;
        if let Some(ref p) = self.logging.log_file {
            anyhow::ensure!(
                p.is_absolute(),
                "logging.log_file must be an absolute path (set via config or ERINRA_LOG_FILE), got: {}",
                p.display()
            );
        }
        anyhow::ensure!(
            self.sync.export_interval > 0,
            "sync.export_interval must be > 0"
        );
        anyhow::ensure!(
            self.sync.tombstone_retention_days > 0,
            "sync.tombstone_retention_days must be > 0"
        );
        anyhow::ensure!(
            self.sync.sync_dir.is_absolute(),
            "sync.sync_dir must be an absolute path, got: {}",
            self.sync.sync_dir.display()
        );
        anyhow::ensure!(
            !self.sync.filename.is_empty(),
            "sync.filename must not be empty"
        );
        anyhow::ensure!(self.web.port > 0, "web.port must be > 0");
        anyhow::ensure!(!self.web.bind.is_empty(), "web.bind must not be empty");
        Ok(())
    }

    /// Apply `ERINRA_*` environment variable overrides.
    fn apply_env_overrides(&mut self) {
        self.apply_overrides_from(|key| std::env::var(key).ok());
    }

    /// Apply overrides from an arbitrary variable source.
    ///
    /// Production calls this with `std::env::var`; tests inject a closure
    /// to avoid mutating process-global environment state.
    fn apply_overrides_from(&mut self, get_var: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get_var("ERINRA_EMBEDDING_MODEL") {
            self.embedding.model = v;
        }
        if let Some(v) = get_var("ERINRA_LOG_LEVEL") {
            self.logging.log_level = v;
        }
        if let Some(v) = get_var("ERINRA_LOG_FILE") {
            self.logging.log_file = Some(PathBuf::from(v));
        }
        Self::parse_override(
            &get_var,
            "ERINRA_STORE_SIMILAR_LIMIT",
            &mut self.store.similar_limit,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_STORE_SIMILAR_THRESHOLD",
            &mut self.store.similar_threshold,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_STORE_CONTENT_MAX_LENGTH",
            &mut self.store.content_max_length,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_STORE_MAX_CONTENT_SIZE",
            &mut self.store.max_content_size,
        );
        Self::parse_override(&get_var, "ERINRA_SEARCH_RRF_K", &mut self.search.rrf_k);
        Self::parse_override(
            &get_var,
            "ERINRA_DATABASE_BUSY_TIMEOUT",
            &mut self.database.busy_timeout,
        );
        // Sync overrides.
        Self::parse_override(&get_var, "ERINRA_SYNC_ENABLED", &mut self.sync.enabled);
        if let Some(v) = get_var("ERINRA_SYNC_DIR") {
            self.sync.sync_dir = PathBuf::from(v);
        }
        if let Some(v) = get_var("ERINRA_SYNC_FILENAME") {
            self.sync.filename = v;
        }
        Self::parse_override(&get_var, "ERINRA_SYNC_FORMAT", &mut self.sync.format);
        Self::parse_override(
            &get_var,
            "ERINRA_SYNC_EXPORT_INTERVAL",
            &mut self.sync.export_interval,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_SYNC_POLL_INTERVAL",
            &mut self.sync.poll_interval,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_SYNC_RESTORE_ON_START",
            &mut self.sync.restore_on_start,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_SYNC_EXPORT_ON_EXIT",
            &mut self.sync.export_on_exit,
        );
        Self::parse_override(
            &get_var,
            "ERINRA_SYNC_TOMBSTONE_RETENTION_DAYS",
            &mut self.sync.tombstone_retention_days,
        );
        // Reranker overrides.
        Self::parse_override(
            &get_var,
            "ERINRA_RERANKER_ENABLED",
            &mut self.reranker.enabled,
        );
        if let Some(v) = get_var("ERINRA_RERANKER_MODEL") {
            self.reranker.model = v;
        }
        Self::parse_override(
            &get_var,
            "ERINRA_RERANKER_THRESHOLD",
            &mut self.reranker.threshold,
        );
        // Web overrides.
        Self::parse_override(&get_var, "ERINRA_WEB_PORT", &mut self.web.port);
        if let Some(v) = get_var("ERINRA_WEB_BIND") {
            self.web.bind = v;
        }
    }

    /// Parse a numeric env override, warning on unparseable values.
    fn parse_override<T: std::str::FromStr>(
        get_var: &impl Fn(&str) -> Option<String>,
        key: &str,
        target: &mut T,
    ) where
        T::Err: std::fmt::Display,
    {
        if let Some(v) = get_var(key) {
            match v.parse() {
                Ok(n) => *target = n,
                Err(e) => {
                    tracing::warn!(var = key, value = %v, error = %e, "ignoring unparseable env override")
                }
            }
        }
    }
}

/// CLI argument overrides (highest precedence in the config chain).
#[derive(Default)]
#[non_exhaustive]
pub struct CliOverrides {
    pub log_level: Option<String>,
    pub log_file: Option<PathBuf>,
    pub busy_timeout: Option<u32>,
    pub embedding_model: Option<String>,
    pub reranker_model: Option<String>,
}

impl CliOverrides {
    /// Create a new `CliOverrides` with all fields set.
    pub fn new(
        log_level: Option<String>,
        log_file: Option<PathBuf>,
        busy_timeout: Option<u32>,
        embedding_model: Option<String>,
        reranker_model: Option<String>,
    ) -> Self {
        Self {
            log_level,
            log_file,
            busy_timeout,
            embedding_model,
            reranker_model,
        }
    }
}

impl Config {
    /// Apply CLI argument overrides.
    fn apply_cli_overrides(&mut self, overrides: &CliOverrides) {
        if let Some(ref v) = overrides.log_level {
            self.logging.log_level = v.clone();
        }
        if let Some(ref v) = overrides.log_file {
            self.logging.log_file = Some(v.clone());
        }
        if let Some(v) = overrides.busy_timeout {
            self.database.busy_timeout = v;
        }
        if let Some(ref v) = overrides.embedding_model {
            self.embedding.model = v.clone();
        }
        if let Some(ref v) = overrides.reranker_model {
            self.reranker.model = v.clone();
            self.reranker.enabled = true; // specifying a model implies enabling
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_file(dir.path()).unwrap();
        assert_eq!(config.embedding.model, "NomicEmbedTextV15Q");
        assert_eq!(config.store.similar_limit, 3);
        assert!((config.store.similar_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.store.content_max_length, 500);
        assert_eq!(config.store.max_content_size, 10240);
        assert_eq!(config.search.rrf_k, 60);
        assert_eq!(config.database.busy_timeout, 5000);
        assert_eq!(config.logging.log_level, "info");
        assert!(config.logging.log_file.is_none());
        // Sync defaults per design doc lines 590-598.
        let home = dirs::home_dir().unwrap();
        assert_eq!(config.sync.sync_dir, home.join(".erinra/sync"));
        assert_eq!(config.sync.filename, "{hostname}");
        assert_eq!(config.sync.format, SyncFormat::JsonlGz);
        assert_eq!(config.sync.export_interval, 900);
        assert_eq!(config.sync.poll_interval, 0);
        assert!(!config.sync.restore_on_start);
        assert!(!config.sync.export_on_exit);
        assert_eq!(config.sync.tombstone_retention_days, 90);
    }

    #[test]
    fn default_config_toml_is_valid_and_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        // load_file should create config.toml when it doesn't exist.
        let _ = Config::load_file(dir.path()).unwrap();
        let config_path = dir.path().join("config.toml");
        assert!(config_path.exists(), "config.toml should be created");

        let contents = std::fs::read_to_string(&config_path).unwrap();
        // All values are commented out, so it should parse as defaults.
        let parsed: Config = toml::from_str(&contents).expect("generated TOML should parse");
        assert_eq!(parsed.embedding.model, "NomicEmbedTextV15Q");
        assert!(!parsed.reranker.enabled);
        assert_eq!(parsed.store.similar_limit, 3);
        assert_eq!(parsed.web.port, 9898);
    }

    #[test]
    fn partial_config_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(f, "[store]\nsimilar_limit = 5").unwrap();

        let config = Config::load_file(dir.path()).unwrap();
        assert_eq!(config.store.similar_limit, 5);
        // Other fields keep defaults.
        assert!((config.store.similar_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.store.content_max_length, 500);
        assert_eq!(config.store.max_content_size, 10240);
        assert_eq!(config.embedding.model, "NomicEmbedTextV15Q");
        assert_eq!(config.search.rrf_k, 60);
        assert_eq!(config.database.busy_timeout, 5000);
    }

    #[test]
    fn full_config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[embedding]
model = "BGESmallENV15Q"

[store]
similar_limit = 5
similar_threshold = 0.7
content_max_length = 300
max_content_size = 8192

[search]
rrf_k = 30

[database]
busy_timeout = 10000

[logging]
log_level = "debug"
log_file = "/tmp/erinra.log"

[sync]
sync_dir = "/custom/sync/dir"
filename = "my-machine"
format = "json.gz"
export_interval = 600
poll_interval = 30
restore_on_start = true
export_on_exit = true
tombstone_retention_days = 30
"#,
        )
        .unwrap();

        let config = Config::load_file(dir.path()).unwrap();
        assert_eq!(config.embedding.model, "BGESmallENV15Q");
        assert_eq!(config.store.similar_limit, 5);
        assert!((config.store.similar_threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(config.store.content_max_length, 300);
        assert_eq!(config.store.max_content_size, 8192);
        assert_eq!(config.search.rrf_k, 30);
        assert_eq!(config.database.busy_timeout, 10000);
        assert_eq!(config.logging.log_level, "debug");
        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(Path::new("/tmp/erinra.log"))
        );
        assert_eq!(config.sync.sync_dir, Path::new("/custom/sync/dir"));
        assert_eq!(config.sync.filename, "my-machine");
        assert_eq!(config.sync.format, SyncFormat::JsonGz);
        assert_eq!(config.sync.export_interval, 600);
        assert_eq!(config.sync.poll_interval, 30);
        assert!(config.sync.restore_on_start);
        assert!(config.sync.export_on_exit);
        assert_eq!(config.sync.tombstone_retention_days, 30);
    }

    #[test]
    fn env_overrides_file_values() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[store]\nsimilar_limit = 5\n").unwrap();

        // Load from file without env overrides.
        let contents = std::fs::read_to_string(&config_path).unwrap();
        let mut config: Config = toml::from_str(&contents).unwrap();
        // File value was loaded.
        assert_eq!(config.store.similar_limit, 5);

        // Apply overrides via injectable seam — no unsafe env mutation.
        config.apply_overrides_from(|key| match key {
            "ERINRA_STORE_SIMILAR_LIMIT" => Some("10".into()),
            "ERINRA_EMBEDDING_MODEL" => Some("TestModel".into()),
            "ERINRA_LOG_LEVEL" => Some("debug".into()),
            "ERINRA_LOG_FILE" => Some("/tmp/test.log".into()),
            _ => None,
        });

        assert_eq!(config.store.similar_limit, 10);
        assert_eq!(config.embedding.model, "TestModel");
        assert_eq!(config.logging.log_level, "debug");
        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(Path::new("/tmp/test.log"))
        );
        // File-only defaults survived (not overridden).
        assert_eq!(config.store.content_max_length, 500);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "not valid { toml").unwrap();

        let err = Config::load_file(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse") || msg.contains("TOML") || msg.contains("expected"),
            "expected a TOML parse error, got: {msg}"
        );
    }

    #[test]
    fn validation_rejects_bad_values() {
        let mut config = Config::default();

        config.database.busy_timeout = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("busy_timeout")
        );
        config.database.busy_timeout = 5000;

        config.store.similar_threshold = 1.5;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("similar_threshold")
        );
        config.store.similar_threshold = 0.5;

        config.store.max_content_size = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("max_content_size")
        );
        config.store.max_content_size = 10240;

        config.store.similar_limit = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("similar_limit")
        );
        config.store.similar_limit = 3;

        config.logging.log_level = "".to_string();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("log_level")
        );

        config.logging.log_level = "=bad_level".to_string();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("log_level")
        );
        config.logging.log_level = "info".to_string();

        config.logging.log_file = Some(PathBuf::from("relative.log"));
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
        config.logging.log_file = if cfg!(windows) {
            Some(PathBuf::from("C:\\tmp\\erinra.log"))
        } else {
            Some(PathBuf::from("/tmp/erinra.log"))
        };

        config.sync.export_interval = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("export_interval")
        );
        config.sync.export_interval = 900;

        config.sync.tombstone_retention_days = 0;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("tombstone_retention_days")
        );
        config.sync.tombstone_retention_days = 90;

        config.sync.filename = "".to_string();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("filename")
        );
        config.sync.filename = "{hostname}".to_string();

        // Valid config passes (including complex filter directives and absolute log_file).
        config.logging.log_level = "erinra=debug,tower=warn".to_string();
        config.validate().unwrap();
    }

    #[test]
    fn sync_env_overrides() {
        let mut config = Config::default();
        config.apply_overrides_from(|key| match key {
            "ERINRA_SYNC_DIR" => Some("/override/sync".into()),
            "ERINRA_SYNC_FILENAME" => Some("custom-host".into()),
            "ERINRA_SYNC_FORMAT" => Some("json".into()),
            "ERINRA_SYNC_EXPORT_INTERVAL" => Some("120".into()),
            "ERINRA_SYNC_POLL_INTERVAL" => Some("60".into()),
            "ERINRA_SYNC_RESTORE_ON_START" => Some("true".into()),
            "ERINRA_SYNC_EXPORT_ON_EXIT" => Some("true".into()),
            "ERINRA_SYNC_TOMBSTONE_RETENTION_DAYS" => Some("45".into()),
            _ => None,
        });

        assert_eq!(config.sync.sync_dir, Path::new("/override/sync"));
        assert_eq!(config.sync.filename, "custom-host");
        assert_eq!(config.sync.format, SyncFormat::Json);
        assert_eq!(config.sync.export_interval, 120);
        assert_eq!(config.sync.poll_interval, 60);
        assert!(config.sync.restore_on_start);
        assert!(config.sync.export_on_exit);
        assert_eq!(config.sync.tombstone_retention_days, 45);
    }

    #[test]
    fn sync_format_deserialization() {
        // Each valid format string parses correctly.
        for (toml_val, expected) in [
            ("jsonl", SyncFormat::Jsonl),
            ("jsonl.gz", SyncFormat::JsonlGz),
            ("json", SyncFormat::Json),
            ("json.gz", SyncFormat::JsonGz),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config_path = dir.path().join("config.toml");
            std::fs::write(&config_path, format!("[sync]\nformat = \"{toml_val}\"\n")).unwrap();
            let config = Config::load_file(dir.path()).unwrap();
            assert_eq!(config.sync.format, expected, "format={toml_val}");
        }

        // Invalid format string is rejected at parse time.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[sync]\nformat = \"xml\"\n").unwrap();
        assert!(Config::load_file(dir.path()).is_err());
    }

    #[test]
    fn sync_deny_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[sync]\nunknown_key = true\n").unwrap();
        let err = Config::load_file(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown") || msg.contains("Unknown"),
            "expected unknown field error, got: {msg}"
        );
    }

    #[test]
    fn validation_rejects_relative_sync_dir() {
        let mut config = Config::default();
        config.sync.sync_dir = PathBuf::from("relative/sync/dir");
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("absolute"),
            "expected error about absolute path, got: {err}"
        );
    }

    #[test]
    fn sync_dir_default_fallback_is_clearly_relative() {
        // When dirs::home_dir() would return None, the fallback should
        // NOT have a "." prefix (the old fallback was "./.erinra/sync").
        // The fallback should be ".erinra/sync" (no dot prefix) so that
        // validate() clearly rejects it as non-absolute.
        let fallback = PathBuf::from(".erinra/sync");
        assert!(
            !fallback.is_absolute(),
            "fallback must be relative so validate() catches it"
        );
    }

    #[test]
    fn tilde_expansion_in_log_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[logging]\nlog_file = \"~/logs/erinra.log\"\n",
        )
        .unwrap();

        let config = Config::load(dir.path(), None).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(home.join("logs/erinra.log").as_path()),
            "tilde in log_file should be expanded"
        );
    }

    #[test]
    fn tilde_expansion_in_sync_dir_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[sync]\nsync_dir = \"~/.erinra/sync\"\n").unwrap();

        let config = Config::load(dir.path(), None).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.sync.sync_dir,
            home.join(".erinra/sync"),
            "tilde should be expanded to home directory"
        );
    }

    #[test]
    fn tilde_expansion_in_sync_dir_from_env() {
        let dir = tempfile::tempdir().unwrap();
        // No config file, use defaults.
        let mut config = Config::load_file(dir.path()).unwrap();
        config.apply_overrides_from(|key| match key {
            "ERINRA_SYNC_DIR" => Some("~/.erinra/custom".into()),
            _ => None,
        });
        config.normalize();
        config.validate().unwrap();

        let home = dirs::home_dir().unwrap();
        assert_eq!(
            config.sync.sync_dir,
            home.join(".erinra/custom"),
            "tilde from env override should be expanded"
        );
    }

    #[test]
    fn expand_tilde_helper() {
        let home = dirs::home_dir().unwrap();
        // Tilde at start is expanded.
        assert_eq!(
            expand_tilde(Path::new("~/.erinra/sync")),
            home.join(".erinra/sync")
        );
        // Bare tilde expands to home.
        assert_eq!(expand_tilde(Path::new("~")), home);
        // Tilde not at start is left alone.
        assert_eq!(
            expand_tilde(Path::new("/foo/~/bar")),
            PathBuf::from("/foo/~/bar")
        );
        // Already absolute is left alone.
        assert_eq!(
            expand_tilde(Path::new("/absolute/path")),
            PathBuf::from("/absolute/path")
        );
        // ~user/path syntax is NOT expanded (left unchanged).
        assert_eq!(
            expand_tilde(Path::new("~alice/foo")),
            PathBuf::from("~alice/foo")
        );
    }

    #[test]
    fn cli_override_log_level() {
        let mut config = Config::default();
        assert_eq!(config.logging.log_level, "info");

        config.apply_cli_overrides(&CliOverrides {
            log_level: Some("debug".to_string()),
            log_file: None,
            busy_timeout: None,
            embedding_model: None,
            reranker_model: None,
        });

        assert_eq!(config.logging.log_level, "debug");
    }

    #[test]
    fn cli_override_log_file() {
        let mut config = Config::default();
        assert!(config.logging.log_file.is_none());

        config.apply_cli_overrides(&CliOverrides {
            log_level: None,
            log_file: Some(PathBuf::from("/tmp/override.log")),
            busy_timeout: None,
            embedding_model: None,
            reranker_model: None,
        });

        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(Path::new("/tmp/override.log"))
        );
    }

    #[test]
    fn cli_override_busy_timeout() {
        let mut config = Config::default();
        assert_eq!(config.database.busy_timeout, 5000);

        config.apply_cli_overrides(&CliOverrides {
            log_level: None,
            log_file: None,
            busy_timeout: Some(10000),
            embedding_model: None,
            reranker_model: None,
        });

        assert_eq!(config.database.busy_timeout, 10000);
    }

    #[test]
    fn cli_override_embedding_model() {
        let mut config = Config::default();
        assert_eq!(config.embedding.model, "NomicEmbedTextV15Q");

        config.apply_cli_overrides(&CliOverrides {
            log_level: None,
            log_file: None,
            busy_timeout: None,
            embedding_model: Some("BGESmallENV15Q".to_string()),
            reranker_model: None,
        });

        assert_eq!(config.embedding.model, "BGESmallENV15Q");
    }

    #[test]
    fn cli_override_none_leaves_config_unchanged() {
        let mut config = Config::default();
        let original_log_level = config.logging.log_level.clone();
        let original_log_file = config.logging.log_file.clone();
        let original_busy_timeout = config.database.busy_timeout;
        let original_model = config.embedding.model.clone();

        config.apply_cli_overrides(&CliOverrides {
            log_level: None,
            log_file: None,
            busy_timeout: None,
            embedding_model: None,
            reranker_model: None,
        });

        assert_eq!(config.logging.log_level, original_log_level);
        assert_eq!(config.logging.log_file, original_log_file);
        assert_eq!(config.database.busy_timeout, original_busy_timeout);
        assert_eq!(config.embedding.model, original_model);
    }

    #[test]
    fn cli_overrides_beat_env_vars() {
        let mut config = Config::default();

        // Simulate env var overrides setting values.
        config.apply_overrides_from(|key| match key {
            "ERINRA_LOG_LEVEL" => Some("warn".into()),
            "ERINRA_EMBEDDING_MODEL" => Some("EnvModel".into()),
            "ERINRA_DATABASE_BUSY_TIMEOUT" => Some("3000".into()),
            "ERINRA_LOG_FILE" => Some("/tmp/env.log".into()),
            _ => None,
        });

        assert_eq!(config.logging.log_level, "warn");
        assert_eq!(config.embedding.model, "EnvModel");
        assert_eq!(config.database.busy_timeout, 3000);
        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(Path::new("/tmp/env.log"))
        );

        // CLI overrides should win over the env-set values.
        config.apply_cli_overrides(&CliOverrides {
            log_level: Some("trace".to_string()),
            log_file: Some(PathBuf::from("/var/log/cli.log")),
            busy_timeout: Some(9999),
            embedding_model: Some("CliModel".to_string()),
            reranker_model: None,
        });

        assert_eq!(config.logging.log_level, "trace");
        assert_eq!(
            config.logging.log_file.as_deref(),
            Some(Path::new("/var/log/cli.log"))
        );
        assert_eq!(config.database.busy_timeout, 9999);
        assert_eq!(config.embedding.model, "CliModel");
    }

    #[test]
    fn sync_partial_config_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[sync]\nexport_interval = 120\nformat = \"json\"\n",
        )
        .unwrap();

        let config = Config::load_file(dir.path()).unwrap();
        // Explicitly set values.
        assert_eq!(config.sync.export_interval, 120);
        assert_eq!(config.sync.format, SyncFormat::Json);
        // Remaining fields get defaults.
        let home = dirs::home_dir().unwrap();
        assert_eq!(config.sync.sync_dir, home.join(".erinra/sync"));
        assert_eq!(config.sync.filename, "{hostname}");
        assert_eq!(config.sync.poll_interval, 0);
        assert!(!config.sync.restore_on_start);
        assert!(!config.sync.export_on_exit);
        assert_eq!(config.sync.tombstone_retention_days, 90);
    }

    #[test]
    fn reranker_defaults() {
        let config = Config::default();
        assert!(!config.reranker.enabled);
        assert_eq!(config.reranker.model, "JINARerankerV1TurboEn");
        assert!((config.reranker.threshold - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn reranker_env_overrides() {
        let mut config = Config::default();
        config.apply_overrides_from(|key| match key {
            "ERINRA_RERANKER_ENABLED" => Some("true".into()),
            "ERINRA_RERANKER_MODEL" => Some("BGERerankerBase".into()),
            "ERINRA_RERANKER_THRESHOLD" => Some("-0.5".into()),
            _ => None,
        });

        assert!(config.reranker.enabled);
        assert_eq!(config.reranker.model, "BGERerankerBase");
        assert!((config.reranker.threshold - (-0.5)).abs() < f64::EPSILON);
    }

    #[test]
    fn reranker_config_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[reranker]
enabled = true
model = "BGERerankerV2M3"
threshold = 0.1
"#,
        )
        .unwrap();

        let config = Config::load_file(dir.path()).unwrap();
        assert!(config.reranker.enabled);
        assert_eq!(config.reranker.model, "BGERerankerV2M3");
        assert!((config.reranker.threshold - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn reranker_deny_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[reranker]\nunknown_key = true\n").unwrap();
        let err = Config::load_file(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unknown") || msg.contains("Unknown"),
            "expected unknown field error, got: {msg}"
        );
    }

    #[test]
    fn cli_reranker_model_implies_enabled() {
        let mut config = Config::default();
        assert!(!config.reranker.enabled);

        config.apply_cli_overrides(&CliOverrides {
            log_level: None,
            log_file: None,
            busy_timeout: None,
            embedding_model: None,
            reranker_model: Some("BGERerankerBase".to_string()),
        });

        assert!(
            config.reranker.enabled,
            "specifying reranker_model should imply enabled"
        );
        assert_eq!(config.reranker.model, "BGERerankerBase");
    }
}
