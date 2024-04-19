//! A library for the privileged tunnel process for a Linux Firezone Client
//!
//! This is built both standalone and as part of the GUI package. Building it
//! standalone is faster and skips all the GUI dependencies. We can use that build for
//! CLI use cases.
//!
//! Building it as a binary within the `gui-client` package allows the
//! Tauri deb bundler to pick it up easily.
//! Otherwise we would just make it a normal binary crate.

use std::path::PathBuf;

pub use imp::{default_token_path, run};

#[cfg(target_os = "linux")]
mod imp_linux;
#[cfg(target_os = "linux")]
use imp_linux as imp;

#[cfg(target_os = "windows")]
mod imp_windows {
    use clap::Parser;

    pub fn default_token_path() -> std::path::PathBuf {
        todo!()
    }

    pub fn run() -> anyhow::Result<()> {
        let cli = super::Cli::parse();
        let _cmd = cli.command();
        tracing::info!(git_version = crate::GIT_VERSION);
        // Clippy will complain that the `Result` type is pointless if we can't
        // possibly throw an error, because it doesn't see that the Linux impl does
        // throw errors
        anyhow::bail!("`headless-client` is not implemented for Windows yet");
    }
}
#[cfg(target_os = "windows")]
use imp_windows as imp;

/// Output of `git describe` at compile time
/// e.g. `1.0.0-pre.4-20-ged5437c88-modified` where:
///
/// * `1.0.0-pre.4` is the most recent ancestor tag
/// * `20` is the number of commits since then
/// * `g` doesn't mean anything
/// * `ed5437c88` is the Git commit hash
/// * `-modified` is present if the working dir has any changes from that commit number
pub const GIT_VERSION: &str = git_version::git_version!(
    args = ["--always", "--dirty=-modified", "--tags"],
    fallback = "unknown"
);

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    #[arg(
        short = 'u',
        long,
        hide = true,
        env = "FIREZONE_API_URL",
        default_value = "wss://api.firezone.dev"
    )]
    pub api_url: url::Url,

    /// Check the configuration and return 0 before connecting to the API
    ///
    /// Returns 1 if the configuration is wrong. Mostly non-destructive but may
    /// write a device ID to disk if one is not found.
    #[arg(long)]
    check: bool,

    /// A filesystem path where the token can be found

    // Apparently passing secrets through stdin is the most secure method, but
    // until anyone asks for it, env vars are okay and files on disk are slightly better.
    // (Since we run as root and the env var on a headless system is probably stored
    // on disk somewhere anyway.)
    #[arg(default_value_t = default_token_path().display().to_string(), env = "FIREZONE_TOKEN_PATH", long)]
    token_path: String,

    /// Identifier used by the portal to identify and display the device.

    // AKA `device_id` in the Windows and Linux GUI clients
    // Generated automatically if not provided
    #[arg(short = 'i', long, env = "FIREZONE_ID")]
    pub firezone_id: Option<String>,

    /// File logging directory. Should be a path that's writeable by the current user.
    #[arg(short, long, env = "LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Maximum length of time to retry connecting to the portal if we're having internet issues or
    /// it's down. Accepts human times. e.g. "5m" or "1h" or "30d".
    #[arg(short, long, env = "MAX_PARTITION_TIME")]
    max_partition_time: Option<humantime::Duration>,
}

impl Cli {
    fn command(&self) -> Cmd {
        // Needed for backwards compatibility with old Docker images
        self.command.unwrap_or(Cmd::Auto)
    }
}

#[derive(clap::Subcommand, Clone, Copy)]
enum Cmd {
    /// If there is a token on disk, run in standalone mode. Otherwise, run as an IPC service. This will be removed in a future version.
    #[command(hide = true)]
    Auto,
    /// Listen for IPC connections and act as a privileged tunnel process for a GUI client
    #[command(hide = true)]
    IpcService,
    /// Act as a CLI-only Client
    Standalone,
    /// Act as an IPC client for development
    #[command(hide = true)]
    StubIpcClient,
}
