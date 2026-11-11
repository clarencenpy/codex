use clap::Parser;
use clap::Subcommand;
use codex_app_server_protocol::ConfigWarningNotification;
use codex_cc_bridge::config::Config;
use codex_cc_bridge::config::default_config_path;
use codex_cc_bridge::engine::ClaudeLauncher;
use codex_cc_bridge::engine::engine_version;
use codex_cc_bridge::server::Bridge;
use codex_cc_bridge::transport::listen;
use codex_cc_bridge::transport::parse_listen;
use std::path::PathBuf;
use std::sync::Arc;

/// Speaks the Codex app-server protocol to a Codex client while Claude Code
/// does the work.
#[derive(Parser)]
#[command(name = "cc-codex-bridge", version)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Serve the Codex app-server protocol.
    AppServer {
        /// stdio:// (default), ws://127.0.0.1:PORT, or unix:///path/to.sock
        #[arg(long, default_value = "stdio://")]
        listen: String,
        /// Bridge config file.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout carries the protocol on stdio, so logs go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let CliCommand::AppServer {
        listen: listen_spec,
        config,
    } = Cli::parse().command;
    let listen_on = parse_listen(&listen_spec)?;
    let config = Config::load(&config.unwrap_or_else(default_config_path))?;

    let mut warnings = Vec::new();
    match engine_version(&config.engine_command).await {
        None => warnings.push(ConfigWarningNotification {
            summary: format!(
                "Could not run `{} --version`.",
                config.engine_command.join(" ")
            ),
            details: Some("Check engineCommand in the bridge config.".to_string()),
            path: None,
            range: None,
        }),
        Some(version) if version != config.tested_claude_version => {
            warnings.push(ConfigWarningNotification {
                summary: format!(
                    "Claude Code {version} is installed; the bridge was tested with {}.",
                    config.tested_claude_version
                ),
                details: Some("Its stream-json messages may differ.".to_string()),
                path: None,
                range: None,
            });
        }
        Some(_) => {}
    }
    for warning in &warnings {
        tracing::warn!("{}", warning.summary);
    }

    let launcher = Arc::new(ClaudeLauncher {
        command: config.engine_command.clone(),
    });
    let bridge = Bridge::new(config, launcher, warnings)?;
    listen(bridge, listen_on).await
}
