use anyhow::{Context, Result};
use clap::Parser;
use connlib_client_shared::{file_logger, keypair, Callbacks, LoginUrl, Session, Sockets};
use firezone_cli_utils::setup_global_subscriber;
use secrecy::SecretString;
use std::{future, path::PathBuf, task::Poll};
use tokio::sync::mpsc;

use firezone_headless_client::{imp, Cli, SignalKind, TOKEN_ENV_KEY};

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

    fn on_update_resources(&self, resources: Vec<connlib_client_shared::ResourceDescription>) {
        // See easily with `export RUST_LOG=firezone_headless_client=debug`
        tracing::debug!(len = resources.len(), "Printing the resource list one time");
        for resource in &resources {
            tracing::debug!(?resource);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();

    // Modifying the environment of a running process is unsafe. If any other
    // thread is reading or writing the environment, something bad can happen.
    // So `run` must take over as early as possible during startup, and
    // take the token env var before any other threads spawn.

    let token_env_var = cli.token.take().map(SecretString::from);
    let cli = cli;

    // Docs indicate that `remove_var` should actually be marked unsafe
    // SAFETY: We haven't spawned any other threads, this code should be the first
    // thing to run after entering `main`. So nobody else is reading the environment.
    #[allow(unused_unsafe)]
    unsafe {
        // This removes the token from the environment per <https://security.stackexchange.com/a/271285>. We run as root so it may not do anything besides defense-in-depth.
        std::env::remove_var(TOKEN_ENV_KEY);
    }
    assert!(std::env::var(TOKEN_ENV_KEY).is_err());

    let (layer, _handle) = cli.log_dir.as_deref().map(file_logger::layer).unzip();
    setup_global_subscriber(layer);

    tracing::info!(git_version = firezone_headless_client::GIT_VERSION);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let token = get_token(token_env_var, &cli)?.with_context(|| {
        format!(
            "Can't find the Firezone token in ${TOKEN_ENV_KEY} or in `{}`",
            cli.token_path
        )
    })?;

    tracing::info!("Running in standalone mode");
    let _guard = rt.enter();
    // TODO: Should this default to 30 days?
    let max_partition_time = cli.max_partition_time.map(|d| d.into());

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
        None,
        public_key.to_bytes(),
    )?;

    if cli.check {
        tracing::info!("Check passed");
        return Ok(());
    }

    let (on_disconnect_tx, mut on_disconnect_rx) = mpsc::channel(1);
    let callback_handler = CallbackHandler { on_disconnect_tx };

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
    session.set_dns(imp::system_resolvers().unwrap_or_default());

    let mut signals = imp::Signals::new()?;

    let result = rt.block_on(async {
        future::poll_fn(|cx| loop {
            match on_disconnect_rx.poll_recv(cx) {
                Poll::Ready(Some(error)) => return Poll::Ready(Err(anyhow::anyhow!(error))),
                Poll::Ready(None) => {
                    return Poll::Ready(Err(anyhow::anyhow!(
                        "on_disconnect_rx unexpectedly ran empty"
                    )))
                }
                Poll::Pending => {}
            }

            match signals.poll(cx) {
                Poll::Ready(SignalKind::Hangup) => {
                    session.reconnect();
                    continue;
                }
                Poll::Ready(SignalKind::Interrupt) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    });

    session.disconnect();

    result
}

/// Read the token from disk if it was not in the environment
///
/// # Returns
/// - `Ok(None)` if there is no token to be found
/// - `Ok(Some(_))` if we found the token
/// - `Err(_)` if we found the token on disk but failed to read it
fn get_token(token_env_var: Option<SecretString>, cli: &Cli) -> Result<Option<SecretString>> {
    // This is very simple but I don't want to write it twice
    if let Some(token) = token_env_var {
        return Ok(Some(token));
    }
    read_token_file(cli)
}

/// Try to retrieve the token from disk
///
/// Sync because we do blocking file I/O
fn read_token_file(cli: &Cli) -> Result<Option<SecretString>> {
    let path = PathBuf::from(&cli.token_path);

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

    if std::fs::metadata(&path).is_err() {
        return Ok(None);
    }
    imp::check_token_permissions(&path)?;

    let Ok(bytes) = std::fs::read(&path) else {
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
