use crate::eventloop::{Eventloop, PHOENIX_TOPIC};
use crate::messages::InitGateway;
use anyhow::{Context, Result};
use backoff::ExponentialBackoffBuilder;
use clap::Parser;
use connlib_shared::{get_user_agent, keypair, Callbacks, Cidrv4, Cidrv6, LoginUrl, StaticSecret};
use firezone_cli_utils::{setup_global_subscriber, CommonArgs};
use firezone_tunnel::{GatewayTunnel, Sockets};
use futures::{future, TryFutureExt};
use ip_network::{Ipv4Network, Ipv6Network};
use secrecy::{Secret, SecretString};
use std::collections::HashSet;
use std::convert::Infallible;
use std::path::Path;
use std::pin::pin;
use tokio::io::AsyncWriteExt;
use tokio::signal::ctrl_c;
use tracing_subscriber::layer;
use uuid::Uuid;

mod eventloop;
mod messages;

const ID_PATH: &str = "/var/lib/firezone/gateway_id";
const PEERS_IPV4: &str = "100.64.0.0/11";
const PEERS_IPV6: &str = "fd00:2021:1111::/107";

#[tokio::main]
async fn main() {
    // Enforce errors only being printed on a single line using the technique recommended in the anyhow docs:
    // https://docs.rs/anyhow/latest/anyhow/struct.Error.html#display-representations
    //
    // By default, `anyhow` prints a stacktrace when it exits.
    // That looks like a "crash" but we "just" exit with a fatal error.
    if let Err(e) = try_main().await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

async fn try_main() -> Result<()> {
    let cli = Cli::parse();
    setup_global_subscriber(layer::Identity::new());

    let firezone_id = get_firezone_id(cli.firezone_id).await
        .context("Couldn't read FIREZONE_ID or write it to disk: Please provide it through the env variable or provide rw access to /var/lib/firezone/")?;

    let (private_key, public_key) = keypair();
    let login = LoginUrl::gateway(
        cli.common.api_url,
        &SecretString::new(cli.common.token),
        firezone_id,
        cli.common.firezone_name,
        public_key.to_bytes(),
    )?;

    let task = tokio::spawn(run(login, private_key)).err_into();

    let ctrl_c = pin!(ctrl_c().map_err(anyhow::Error::new));

    tokio::spawn(http_health_check::serve(
        cli.health_check.health_check_addr,
        || true,
    ));

    match future::try_select(task, ctrl_c)
        .await
        .map_err(|e| e.factor_first().0)?
    {
        future::Either::Left((res, _)) => {
            res?;
        }
        future::Either::Right(_) => {}
    };

    Ok(())
}

async fn get_firezone_id(env_id: Option<String>) -> Result<String> {
    if let Some(id) = env_id {
        if !id.is_empty() {
            return Ok(id);
        }
    }

    if let Ok(id) = tokio::fs::read_to_string(ID_PATH).await {
        if !id.is_empty() {
            return Ok(id);
        }
    }

    let id_path = Path::new(ID_PATH);
    tokio::fs::create_dir_all(id_path.parent().unwrap()).await?;
    let mut id_file = tokio::fs::File::create(id_path).await?;
    let id = Uuid::new_v4().to_string();
    id_file.write_all(id.as_bytes()).await?;
    Ok(id)
}

async fn run(login: LoginUrl, private_key: StaticSecret) -> Result<Infallible> {
    let mut tunnel = GatewayTunnel::new(private_key, Sockets::new(), CallbackHandler)?;

    let (portal, init) = phoenix_channel::init::<_, InitGateway, _, _>(
        Secret::new(login),
        get_user_agent(None),
        PHOENIX_TOPIC,
        (),
        ExponentialBackoffBuilder::default()
            .with_max_elapsed_time(None)
            .build(),
    )
    .await??;

    tunnel
        .set_interface(&init.interface)
        .context("Failed to set interface")?;
    let mut interface = connlib_shared::interface::InterfaceManager::default();
    interface
        .on_set_interface_config(init.interface.ipv4, init.interface.ipv6)
        .await?;
    interface
        .on_update_routes(
            vec![Cidrv4::from(PEERS_IPV4.parse::<Ipv4Network>().unwrap())],
            vec![Cidrv6::from(PEERS_IPV6.parse::<Ipv6Network>().unwrap())],
        )
        .await?;
    tunnel.update_relays(HashSet::default(), init.relays);

    let mut eventloop = Eventloop::new(tunnel, portal);

    future::poll_fn(|cx| eventloop.poll(cx))
        .await
        .context("Eventloop failed")?;

    unreachable!()
}

#[derive(Clone)]
struct CallbackHandler;

impl Callbacks for CallbackHandler {}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    #[command(flatten)]
    health_check: http_health_check::HealthCheckArgs,

    /// Identifier generated by the portal to identify and display the device.
    #[arg(short = 'i', long, env = "FIREZONE_ID")]
    pub firezone_id: Option<String>,
}
