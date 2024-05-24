//! A library for the privileged tunnel process for a Linux Firezone Client
//!
//! This is built both standalone and as part of the GUI package. Building it
//! standalone is faster and skips all the GUI dependencies. We can use that build for
//! CLI use cases.
//!
//! Building it as a binary within the `gui-client` package allows the
//! Tauri deb bundler to pick it up easily.
//! Otherwise we would just make it a normal binary crate.

use anyhow::{bail, Context, Result};
use clap::Parser;
use connlib_client_shared::{file_logger, keypair, Callbacks, LoginUrl, Session, Sockets};
use connlib_shared::callbacks;
use firezone_cli_utils::setup_global_subscriber;
use futures::{future, SinkExt, StreamExt};
use secrecy::SecretString;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    pin::pin,
};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::subscriber::set_global_default;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Layer as _, Registry};
use url::Url;

use platform::default_token_path;

pub mod known_dirs;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux as platform;

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "windows")]
pub use windows as platform;

/// Only used on Linux
pub const FIREZONE_GROUP: &str = "firezone-client";

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

const TOKEN_ENV_KEY: &str = "FIREZONE_TOKEN";

/// Command-line args for the headless Client
#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    // Needed to preserve CLI arg compatibility
    // TODO: Remove
    #[command(subcommand)]
    _command: Option<Cmd>,

    #[command(flatten)]
    common: CliCommon,

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

    /// Friendly name for this client to display in the UI.
    #[arg(long, env = "FIREZONE_NAME")]
    firezone_name: Option<String>,

    /// Identifier used by the portal to identify and display the device.

    // AKA `device_id` in the Windows and Linux GUI clients
    // Generated automatically if not provided
    #[arg(short = 'i', long, env = "FIREZONE_ID")]
    pub firezone_id: Option<String>,

    /// Token generated by the portal to authorize websocket connection.
    // systemd recommends against passing secrets through env vars:
    // <https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#Environment=>
    #[arg(env = TOKEN_ENV_KEY, hide = true)]
    token: Option<String>,

    /// A filesystem path where the token can be found

    // Apparently passing secrets through stdin is the most secure method, but
    // until anyone asks for it, env vars are okay and files on disk are slightly better.
    // (Since we run as root and the env var on a headless system is probably stored
    // on disk somewhere anyway.)
    #[arg(default_value = default_token_path().display().to_string(), env = "FIREZONE_TOKEN_PATH", long)]
    token_path: PathBuf,
}

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct CliIpcService {
    #[command(subcommand)]
    command: CmdIpc,

    #[command(flatten)]
    common: CliCommon,
}

#[derive(clap::Subcommand, Debug, PartialEq, Eq)]
enum CmdIpc {
    #[command(hide = true)]
    DebugIpcService,
    IpcService,
}

impl Default for CmdIpc {
    fn default() -> Self {
        Self::IpcService
    }
}

/// CLI args common to both the IPC service and the headless Client
#[derive(clap::Args)]
struct CliCommon {
    /// File logging directory. Should be a path that's writeable by the current user.
    #[arg(short, long, env = "LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Maximum length of time to retry connecting to the portal if we're having internet issues or
    /// it's down. Accepts human times. e.g. "5m" or "1h" or "30d".
    #[arg(short, long, env = "MAX_PARTITION_TIME")]
    max_partition_time: Option<humantime::Duration>,
}

#[derive(clap::Subcommand, Clone, Copy)]
enum Cmd {
    #[command(hide = true)]
    IpcService,
    Standalone,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum IpcClientMsg {
    Connect { api_url: String, token: String },
    Disconnect,
    Reconnect,
    SetDns(Vec<IpAddr>),
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum IpcServerMsg {
    Ok,
    OnDisconnect,
    OnSetInterfaceConfig {
        ipv4: Ipv4Addr,
        ipv6: Ipv6Addr,
        dns: Vec<IpAddr>,
    },
    OnUpdateResources(Vec<callbacks::ResourceDescription>),
}

pub fn run_only_headless_client() -> Result<()> {
    let mut cli = Cli::try_parse()?;

    // Modifying the environment of a running process is unsafe. If any other
    // thread is reading or writing the environment, something bad can happen.
    // So `run` must take over as early as possible during startup, and
    // take the token env var before any other threads spawn.

    let token_env_var = cli.token.take().map(SecretString::from);
    let cli = cli;

    // Docs indicate that `remove_var` should actually be marked unsafe
    // SAFETY: We haven't spawned any other threads, this code should be the first
    // thing to run after entering `main` and parsing CLI args.
    // So nobody else is reading the environment.
    #[allow(unused_unsafe)]
    unsafe {
        // This removes the token from the environment per <https://security.stackexchange.com/a/271285>. We run as root so it may not do anything besides defense-in-depth.
        std::env::remove_var(TOKEN_ENV_KEY);
    }
    assert!(std::env::var(TOKEN_ENV_KEY).is_err());

    let (layer, _handle) = cli
        .common
        .log_dir
        .as_deref()
        .map(file_logger::layer)
        .unzip();
    setup_global_subscriber(layer);

    tracing::info!(git_version = crate::GIT_VERSION);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let token = get_token(token_env_var, &cli.token_path)?.with_context(|| {
        format!(
            "Can't find the Firezone token in ${TOKEN_ENV_KEY} or in `{}`",
            cli.token_path.display()
        )
    })?;
    tracing::info!("Running in headless / standalone mode");
    let _guard = rt.enter();
    // TODO: Should this default to 30 days?
    let max_partition_time = cli.common.max_partition_time.map(|d| d.into());

    // AKA "Device ID", not the Firezone slug
    let firezone_id = match cli.firezone_id {
        Some(id) => id,
        None => connlib_shared::device_id::get().context("Could not get `firezone_id` from CLI, could not read it from disk, could not generate it and save it to disk")?.id,
    };

    let (private_key, public_key) = keypair();
    let login = LoginUrl::client(
        cli.api_url,
        &token,
        firezone_id,
        cli.firezone_name,
        public_key.to_bytes(),
    )?;

    if cli.check {
        tracing::info!("Check passed");
        return Ok(());
    }

    let (on_disconnect_tx, mut on_disconnect_rx) = mpsc::channel(1);
    let callback_handler = CallbackHandler { on_disconnect_tx };

    platform::setup_before_connlib()?;
    let session = Session::connect(
        login,
        Sockets::new(),
        private_key,
        None,
        callback_handler,
        max_partition_time,
        rt.handle().clone(),
    );
    // TODO: this should be added dynamically
    session.set_dns(platform::system_resolvers().unwrap_or_default());

    let mut signals = platform::Signals::new()?;

    let result = rt.block_on(async {
        loop {
            match future::select(pin!(signals.recv()), pin!(on_disconnect_rx.recv())).await {
                future::Either::Left((SignalKind::Hangup, _)) => {
                    tracing::info!("Caught Hangup signal");
                    session.reconnect();
                }
                future::Either::Left((SignalKind::Interrupt, _)) => {
                    tracing::info!("Caught Interrupt signal");
                    return Ok(());
                }
                future::Either::Right((None, _)) => {
                    return Err(anyhow::anyhow!("on_disconnect_rx unexpectedly ran empty"));
                }
                future::Either::Right((Some(error), _)) => return Err(anyhow::anyhow!(error)),
            }
        }
    });

    session.disconnect();

    result
}

/// Only called from the GUI Client's build of the IPC service
pub fn run_only_ipc_service() -> Result<()> {
    // Docs indicate that `remove_var` should actually be marked unsafe
    // SAFETY: We haven't spawned any other threads, this code should be the first
    // thing to run after entering `main` and parsing CLI args.
    // So nobody else is reading the environment.
    #[allow(unused_unsafe)]
    unsafe {
        // This removes the token from the environment per <https://security.stackexchange.com/a/271285>. We run as root so it may not do anything besides defense-in-depth.
        std::env::remove_var(TOKEN_ENV_KEY);
    }
    assert!(std::env::var(TOKEN_ENV_KEY).is_err());
    let cli = CliIpcService::try_parse()?;
    match cli.command {
        CmdIpc::DebugIpcService => run_debug_ipc_service(),
        CmdIpc::IpcService => platform::run_ipc_service(cli.common),
    }
}

pub(crate) fn run_debug_ipc_service() -> Result<()> {
    debug_command_setup()?;
    let rt = tokio::runtime::Runtime::new()?;
    let ipc_service = pin!(ipc_listen());
    let mut signals = platform::Signals::new()?;

    rt.block_on(async {
        // Couldn't get the loop to work here yet, so SIGHUP is not implemented
        match future::select(pin!(signals.recv()), ipc_service).await {
            future::Either::Left((SignalKind::Hangup, _)) => {
                bail!("Exiting, SIGHUP not implemented for the IPC service");
            }
            future::Either::Left((SignalKind::Interrupt, _)) => {
                tracing::info!("Caught Interrupt signal");
                return Ok(());
            }
            future::Either::Right((Ok(()), _)) => {
                bail!("Impossible, ipc_listen can't return Ok");
            }
            future::Either::Right((Err(error), _)) => {
                return Err(error).context("ipc_listen failed")
            }
        }
    })
}

#[derive(Clone)]
struct CallbackHandlerIpc {
    cb_tx: mpsc::Sender<IpcServerMsg>,
}

impl Callbacks for CallbackHandlerIpc {
    fn on_disconnect(&self, error: &connlib_client_shared::Error) {
        tracing::error!(?error, "Got `on_disconnect` from connlib");
        self.cb_tx
            .try_send(IpcServerMsg::OnDisconnect)
            .expect("should be able to send OnDisconnect");
    }

    fn on_set_interface_config(
        &self,
        ipv4: Ipv4Addr,
        ipv6: Ipv6Addr,
        dns: Vec<IpAddr>,
    ) -> Option<i32> {
        tracing::info!("TunnelReady (on_set_interface_config)");
        self.cb_tx
            .try_send(IpcServerMsg::OnSetInterfaceConfig { ipv4, ipv6, dns })
            .expect("Should be able to send TunnelReady");
        None
    }

    fn on_update_resources(&self, resources: Vec<callbacks::ResourceDescription>) {
        tracing::debug!(len = resources.len(), "New resource list");
        self.cb_tx
            .try_send(IpcServerMsg::OnUpdateResources(resources))
            .expect("Should be able to send OnUpdateResources");
    }
}

async fn ipc_listen() -> Result<()> {
    let mut server = platform::IpcServer::new().await?;
    loop {
        connlib_shared::deactivate_dns_control()?;
        let stream = server.next_client().await?;
        if let Err(error) = handle_ipc_client(stream).await {
            tracing::error!(?error, "Error while handling IPC client");
        }
    }
}

async fn handle_ipc_client(stream: platform::IpcStream) -> Result<()> {
    let (rx, tx) = tokio::io::split(stream);
    let mut rx = FramedRead::new(rx, LengthDelimitedCodec::new());
    let mut tx = FramedWrite::new(tx, LengthDelimitedCodec::new());
    let (cb_tx, mut cb_rx) = mpsc::channel(100);

    let send_task = tokio::spawn(async move {
        while let Some(msg) = cb_rx.recv().await {
            tx.send(serde_json::to_string(&msg)?.into()).await?;
        }
        Ok::<_, anyhow::Error>(())
    });

    let mut connlib = None;
    let callback_handler = CallbackHandlerIpc { cb_tx };
    while let Some(msg) = rx.next().await {
        let msg = msg?;
        let msg: IpcClientMsg = serde_json::from_slice(&msg)?;

        match msg {
            IpcClientMsg::Connect { api_url, token } => {
                let token = secrecy::SecretString::from(token);
                assert!(connlib.is_none());
                let device_id = connlib_shared::device_id::get()
                    .context("Failed to read / create device ID")?;
                let (private_key, public_key) = keypair();

                let login = LoginUrl::client(
                    Url::parse(&api_url)?,
                    &token,
                    device_id.id,
                    None,
                    public_key.to_bytes(),
                )?;

                connlib = Some(connlib_client_shared::Session::connect(
                    login,
                    Sockets::new(),
                    private_key,
                    None,
                    callback_handler.clone(),
                    Some(std::time::Duration::from_secs(60 * 60 * 24 * 30)),
                    tokio::runtime::Handle::try_current()?,
                ));
            }
            IpcClientMsg::Disconnect => {
                if let Some(connlib) = connlib.take() {
                    connlib.disconnect();
                }
            }
            IpcClientMsg::Reconnect => connlib.as_mut().context("No connlib session")?.reconnect(),
            IpcClientMsg::SetDns(v) => connlib.as_mut().context("No connlib session")?.set_dns(v),
        }
    }

    send_task.abort();

    Ok(())
}

#[allow(dead_code)]
enum SignalKind {
    /// SIGHUP
    ///
    /// Not caught on Windows
    Hangup,
    /// SIGINT
    Interrupt,
}

#[derive(Clone)]
struct CallbackHandler {
    /// Channel for an error message if connlib disconnects due to an error
    on_disconnect_tx: mpsc::Sender<String>,
}

impl Callbacks for CallbackHandler {
    fn on_disconnect(&self, error: &connlib_client_shared::Error) {
        // Convert the error to a String since we can't clone it
        self.on_disconnect_tx
            .try_send(error.to_string())
            .expect("should be able to tell the main thread that we disconnected");
    }

    fn on_update_resources(&self, resources: Vec<callbacks::ResourceDescription>) {
        // See easily with `export RUST_LOG=firezone_headless_client=debug`
        for resource in &resources {
            tracing::debug!(?resource);
        }
    }
}

/// Read the token from disk if it was not in the environment
///
/// # Returns
/// - `Ok(None)` if there is no token to be found
/// - `Ok(Some(_))` if we found the token
/// - `Err(_)` if we found the token on disk but failed to read it
fn get_token(
    token_env_var: Option<SecretString>,
    token_path: &Path,
) -> Result<Option<SecretString>> {
    // This is very simple but I don't want to write it twice
    if let Some(token) = token_env_var {
        return Ok(Some(token));
    }
    read_token_file(token_path)
}

/// Try to retrieve the token from disk
///
/// Sync because we do blocking file I/O
fn read_token_file(path: &Path) -> Result<Option<SecretString>> {
    if let Ok(token) = std::env::var(TOKEN_ENV_KEY) {
        std::env::remove_var(TOKEN_ENV_KEY);

        let token = SecretString::from(token);
        // Token was provided in env var
        tracing::info!(
            ?path,
            ?TOKEN_ENV_KEY,
            "Found token in env var, ignoring any token that may be on disk."
        );
        return Ok(Some(token));
    }

    if std::fs::metadata(path).is_err() {
        return Ok(None);
    }
    platform::check_token_permissions(path)?;

    let Ok(bytes) = std::fs::read(path) else {
        // We got the metadata a second ago, but can't read the file itself.
        // Pretty strange, would have to be a disk fault or TOCTOU.
        tracing::info!(?path, "Token file existed but now is unreadable");
        return Ok(None);
    };
    let token = String::from_utf8(bytes)?.trim().to_string();
    let token = SecretString::from(token);

    tracing::info!(?path, "Loaded token from disk");
    Ok(Some(token))
}

/// Sets up logging for stderr only, with INFO level by default
pub fn debug_command_setup() -> Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
        .from_env_lossy();
    let layer = fmt::layer().with_filter(filter);
    let subscriber = Registry::default().with(layer);
    set_global_default(subscriber)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, CliIpcService, CmdIpc};
    use clap::Parser;
    use std::path::PathBuf;
    use url::Url;

    // Can't remember how Clap works sometimes
    // Also these are examples
    #[test]
    fn cli() -> anyhow::Result<()> {
        let exe_name = "firezone-headless-client";

        let actual = Cli::parse_from([exe_name]);
        assert_eq!(actual.api_url, Url::parse("wss://api.firezone.dev")?);
        assert!(!actual.check);

        let actual = Cli::parse_from([exe_name, "--api-url", "wss://api.firez.one"]);
        assert_eq!(actual.api_url, Url::parse("wss://api.firez.one")?);

        let actual = Cli::parse_from([exe_name, "--check", "--log-dir", "bogus_log_dir"]);
        assert!(actual.check);
        assert_eq!(actual.common.log_dir, Some(PathBuf::from("bogus_log_dir")));

        let actual = CliIpcService::parse_from([
            exe_name,
            "--log-dir",
            "bogus_log_dir",
            "debug-ipc-service",
        ]);
        assert_eq!(actual.command, CmdIpc::DebugIpcService);
        assert_eq!(actual.common.log_dir, Some(PathBuf::from("bogus_log_dir")));

        let actual = CliIpcService::parse_from([exe_name, "ipc-service"]);
        assert_eq!(actual.command, CmdIpc::IpcService);

        Ok(())
    }
}
