//! CLI subcommands used to test features / dependencies before integrating
//! them with the GUI, or to exercise features programmatically.

use anyhow::Result;

#[derive(clap::Subcommand)]
pub(crate) enum Cmd {
    SetAutostart(SetAutostartArgs),
}

#[derive(clap::Parser)]
pub(crate) struct SetAutostartArgs {
    #[clap(action=clap::ArgAction::Set)]
    enabled: bool,
}

pub fn run(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::SetAutostart(SetAutostartArgs { enabled }) => set_autostart(enabled),
    }
}

fn set_autostart(enabled: bool) -> Result<()> {
    firezone_headless_client::setup_stdout_logging()?;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(crate::client::gui::set_autostart(enabled))?;
    Ok(())
}
