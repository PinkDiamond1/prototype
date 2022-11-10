mod cli;

use {
  crate::cli::CliOptions,
  anoma_network as network,
  clap::Parser,
  network::Network,
  tracing::info,
  tracing_subscriber::FmtSubscriber,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing::subscriber::set_global_default(FmtSubscriber::new())?;

  let opts = CliOptions::parse();
  info!("startup options: {opts:?}");

  let mut network = Network::default();

  // This topic is used for listening on
  // partial transactions (intents) that could potentially
  // be solved by this instance of the solver and turned into
  // complete transactions.
  let _intents_topic = network
    .join(network::topic::Config {
      name: "/testnet-1/intents".into(),
      bootstrap: opts.peers(),
    })
    .await?;

  // This topic is used to publish full (solved) transactions
  // to validators. It also is used to listen on transactions 
  // published by other solvers to discard intents solved by
  // other solvers from the mempool.
  let _transactions_topic = network
    .join(network::topic::Config {
      name: "/testnet-1/transactions".into(),
      bootstrap: opts.peers(),
    })
    .await?;

  Ok(())
}