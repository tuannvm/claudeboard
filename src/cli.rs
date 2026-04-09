use clap::Parser;

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser, Debug, Clone)]
#[command(name = "claudeboard")]
#[command(about = "TUI dashboard for monitoring Claude Code via tmux")]
pub struct Args {
    #[arg(long, default_value = "5", help = "Refresh interval in seconds")]
    pub refresh_interval: u64,

    #[arg(long, help = "tmux socket path")]
    pub tmux_socket: Option<String>,
}
