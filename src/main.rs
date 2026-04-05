mod cli;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use erinra::config;

#[derive(Parser)]
#[command(
    name = "erinra",
    version,
    about = "Memory MCP server for LLM coding assistants"
)]
struct Cli {
    /// Data directory (database, config, models, sync) [default: ~/.erinra]
    #[arg(long, env = "ERINRA_DATA_DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start MCP server (stdio transport)
    Serve {
        /// Override log level (e.g. "debug", "info", "erinra=debug")
        #[arg(long)]
        log_level: Option<String>,
        /// Override log file path
        #[arg(long)]
        log_file: Option<PathBuf>,
        /// Override SQLite busy timeout in milliseconds
        #[arg(long)]
        busy_timeout: Option<u32>,
        /// Override embedding model name
        #[arg(long)]
        embedding_model: Option<String>,
        /// Override reranker model name (also enables reranking)
        #[arg(long)]
        reranker_model: Option<String>,
        /// Also start the web dashboard (background daemon)
        #[arg(long)]
        web: bool,
        /// Override web server port (requires --web)
        #[arg(long, requires = "web")]
        port: Option<u16>,
        /// Override web server bind address (requires --web)
        #[arg(long, requires = "web")]
        bind: Option<String>,
    },

    /// Export memories to JSONL file
    Export {
        /// Output file path
        output: PathBuf,
        /// Compress output with gzip
        #[arg(long)]
        gzip: bool,
        /// Only export memories created/updated after this timestamp
        #[arg(long)]
        since: Option<String>,
    },

    /// Import memories from JSONL file
    Import {
        /// Input file path
        input: PathBuf,
    },

    /// Run export + import sync cycle
    Sync {
        /// Run even if sync is not enabled in config
        #[arg(long)]
        force: bool,
    },

    /// Regenerate all embeddings
    Reembed {
        /// Override the embedding model
        #[arg(long)]
        model: Option<String>,
    },

    /// Print database stats
    Status,

    /// List available embedding models
    Models,

    /// Print third-party license notices
    Licenses,

    /// Internal: run the web dashboard daemon (not user-facing)
    #[command(name = "_daemon", hide = true)]
    InternalDaemon {
        #[arg(long)]
        port: u16,
        #[arg(long)]
        bind: String,
    },

    /// Open the web dashboard
    Dash {
        /// Override web server port
        #[arg(long)]
        port: Option<u16>,
        /// Override web server bind address
        #[arg(long)]
        bind: Option<String>,
        /// Don't open the browser automatically
        #[arg(long)]
        no_open: bool,
        /// Open the browser and exit immediately (requires a running daemon)
        #[arg(long)]
        open_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.map(Ok).unwrap_or_else(|| {
        dirs::home_dir()
            .map(|mut d| {
                d.push(".erinra");
                d
            })
            .ok_or_else(|| {
                anyhow::anyhow!("could not determine home directory; set ERINRA_DATA_DIR")
            })
    })?;

    match cli.command {
        Command::Serve {
            log_level,
            log_file,
            busy_timeout,
            embedding_model,
            reranker_model,
            web,
            port,
            bind,
        } => {
            cli::serve(
                &data_dir,
                log_level,
                log_file,
                busy_timeout,
                embedding_model,
                reranker_model,
                web,
                port,
                bind,
            )
            .await
        }
        cmd => {
            let config = if data_dir.exists() {
                config::Config::load(&data_dir, None).context("failed to load configuration")?
            } else {
                config::Config::default()
            };
            cli::init_tracing(&config.logging)?;

            match cmd {
                Command::Export {
                    output,
                    gzip,
                    since,
                } => cli::export(&data_dir, &config, &output, gzip, since),
                Command::Import { input } => cli::import(&data_dir, &config, &input).await,
                Command::Sync { force } => cli::run_sync(&data_dir, &config, force).await,
                Command::Reembed { model } => cli::reembed(&data_dir, &config, model).await,
                Command::Status => cli::status(&data_dir, &config),
                Command::Models => cli::models(),
                Command::Licenses => cli::licenses(),
                Command::Dash {
                    port,
                    bind,
                    no_open,
                    open_only,
                } => cli::dash(&data_dir, &config, port, bind, no_open, open_only).await,
                Command::InternalDaemon { port, bind } => {
                    cli::run_daemon(&data_dir, &config, port, &bind).await
                }
                Command::Serve { .. } => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn serve_web_with_port_and_bind() {
        let cli = Cli::try_parse_from([
            "erinra",
            "serve",
            "--web",
            "--port",
            "9090",
            "--bind",
            "127.0.0.1",
        ])
        .expect("serve --web --port --bind should parse");
        match cli.command {
            Command::Serve {
                web, port, bind, ..
            } => {
                assert!(web);
                assert_eq!(port, Some(9090));
                assert_eq!(bind, Some("127.0.0.1".to_string()));
            }
            _ => panic!("expected Serve variant"),
        }
    }

    #[test]
    fn serve_port_requires_web_flag() {
        let result = Cli::try_parse_from(["erinra", "serve", "--port", "9090"]);
        assert!(result.is_err(), "--port without --web should fail");
    }

    #[test]
    fn serve_defaults_web_false() {
        let cli = Cli::try_parse_from(["erinra", "serve"]).expect("bare serve should parse");
        match cli.command {
            Command::Serve {
                web, port, bind, ..
            } => {
                assert!(!web);
                assert_eq!(port, None);
                assert_eq!(bind, None);
            }
            _ => panic!("expected Serve variant"),
        }
    }

    #[test]
    fn serve_bind_requires_web_flag() {
        let result = Cli::try_parse_from(["erinra", "serve", "--bind", "0.0.0.0"]);
        assert!(result.is_err(), "--bind without --web should fail");
    }

    #[test]
    fn dash_with_port_bind_no_open() {
        let cli = Cli::try_parse_from([
            "erinra",
            "dash",
            "--port",
            "9090",
            "--bind",
            "127.0.0.1",
            "--no-open",
        ])
        .expect("dash --port --bind --no-open should parse");
        match cli.command {
            Command::Dash {
                port,
                bind,
                no_open,
                open_only: _,
            } => {
                assert_eq!(port, Some(9090));
                assert_eq!(bind, Some("127.0.0.1".to_string()));
                assert!(no_open);
            }
            _ => panic!("expected Dash variant"),
        }
    }

    #[test]
    fn hidden_daemon_subcommand_parses() {
        let cli =
            Cli::try_parse_from(["erinra", "_daemon", "--port", "9090", "--bind", "127.0.0.1"])
                .expect("_daemon subcommand should parse");
        match cli.command {
            Command::InternalDaemon { port, bind } => {
                assert_eq!(port, 9090);
                assert_eq!(bind, "127.0.0.1");
            }
            _ => panic!("expected InternalDaemon variant"),
        }
    }

    #[test]
    fn serve_with_reranker_model() {
        let cli = Cli::try_parse_from(["erinra", "serve", "--reranker-model", "BGERerankerBase"])
            .expect("serve --reranker-model should parse");
        match cli.command {
            Command::Serve { reranker_model, .. } => {
                assert_eq!(reranker_model, Some("BGERerankerBase".to_string()));
            }
            _ => panic!("expected Serve variant"),
        }
    }

    #[test]
    fn serve_defaults_no_reranker_model() {
        let cli = Cli::try_parse_from(["erinra", "serve"]).expect("bare serve should parse");
        match cli.command {
            Command::Serve { reranker_model, .. } => {
                assert_eq!(reranker_model, None);
            }
            _ => panic!("expected Serve variant"),
        }
    }
}
