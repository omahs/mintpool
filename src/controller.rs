use crate::config::ChainInclusionMode;
use eyre::WrapErr;
use sqlx::SqlitePool;
use tokio::select;
use tokio::sync::{mpsc, oneshot};

use crate::p2p::NetworkState;
use crate::rules::{Evaluation, Results, RuleContext, RulesEngine};
use crate::storage::{PremintStorage, Reader, Writer};
use crate::types::{InclusionClaim, MintpoolNodeInfo, PremintTypes};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SwarmCommand {
    ConnectToPeer {
        address: String,
    },
    ReturnNetworkState {
        channel: oneshot::Sender<NetworkState>,
    },
    AnnounceSelf,
    Broadcast {
        message: PremintTypes,
    },
    ReturnNodeInfo {
        channel: oneshot::Sender<MintpoolNodeInfo>,
    },
    SendOnchainMintFound(InclusionClaim),
}

pub enum P2PEvent {
    NetworkState(NetworkState),
    PremintReceived(PremintTypes),
    MintSeenOnchain(InclusionClaim),
}

pub enum ControllerCommands {
    ConnectToPeer {
        address: String,
    },
    ReturnNetworkState {
        channel: oneshot::Sender<NetworkState>,
    },
    AnnounceSelf,
    Broadcast {
        message: PremintTypes,
        channel: oneshot::Sender<eyre::Result<()>>,
    },
    ReturnNodeInfo {
        channel: oneshot::Sender<MintpoolNodeInfo>,
    },
    Query(DBQuery),
    ResolveOnchainMint(InclusionClaim),
}

pub enum DBQuery {
    ListAll(oneshot::Sender<eyre::Result<Vec<PremintTypes>>>),
    Direct(oneshot::Sender<eyre::Result<SqlitePool>>),
}

pub struct Controller {
    swarm_command_sender: mpsc::Sender<SwarmCommand>,
    swarm_event_receiver: mpsc::Receiver<P2PEvent>,
    external_commands: mpsc::Receiver<ControllerCommands>,
    store: PremintStorage,
    rules: RulesEngine<PremintStorage>,
    trusted_peers: Vec<String>,
    inclusion_mode: ChainInclusionMode,
}

impl Controller {
    pub fn new(
        swarm_command_sender: mpsc::Sender<SwarmCommand>,
        swarm_event_receiver: mpsc::Receiver<P2PEvent>,
        external_commands: mpsc::Receiver<ControllerCommands>,
        store: PremintStorage,
        rules: RulesEngine<PremintStorage>,
    ) -> Self {
        Self {
            swarm_command_sender,
            swarm_event_receiver,
            external_commands,
            store,
            rules,
        }
    }

    pub async fn run_loop(&mut self) {
        loop {
            select! {
                Some(command) = self.external_commands.recv() => {
                    if let Err(err) = self.handle_command(command).await {
                        tracing::error!("Error handling command to controller: {:?}", err);
                    };
                }
                Some(event) = self.swarm_event_receiver.recv() => {
                    self.handle_event(event).await;
                }
            }
        }
    }

    pub async fn handle_event(&self, event: P2PEvent) {
        match event {
            P2PEvent::NetworkState(network_state) => {
                tracing::info!("Current network state: {:?}", network_state);
            }
            P2PEvent::PremintReceived(premint) => {
                tracing::debug!(premint = premint.to_json().ok(), "Received premint");

                // TODO: handle error? respond with error summary?
                let _ = self.validate_and_insert(premint).await;
            }
            P2PEvent::MintSeenOnchain(claim) => {
                // Check or trust
                self
            }
        }
    }

    async fn handle_command(&mut self, command: ControllerCommands) -> eyre::Result<()> {
        match command {
            ControllerCommands::ConnectToPeer { address } => {
                self.swarm_command_sender
                    .send(SwarmCommand::ConnectToPeer { address })
                    .await?;
            }
            ControllerCommands::ReturnNetworkState { channel } => {
                self.swarm_command_sender
                    .send(SwarmCommand::ReturnNetworkState { channel })
                    .await?;
            }
            ControllerCommands::AnnounceSelf => {
                self.swarm_command_sender
                    .send(SwarmCommand::AnnounceSelf)
                    .await?;
            }
            ControllerCommands::Broadcast { message, channel } => {
                match self.validate_and_insert(message.clone()).await {
                    Ok(_result) => {
                        if let Err(err) = self
                            .swarm_command_sender
                            .send(SwarmCommand::Broadcast { message })
                            .await
                        {
                            channel
                                .send(Err(eyre::eyre!("Error broadcasing premint: {:?}", err)))
                                .map_err(|err| {
                                    eyre::eyre!("error broadcasting via channel: {:?}", err)
                                })?;
                        } else {
                            channel.send(Ok(())).map_err(|err| {
                                eyre::eyre!("error broadcasting via channel: {:?}", err)
                            })?;
                        }
                    }
                    Err(err) => channel
                        .send(Err(err))
                        .map_err(|err| eyre::eyre!("error broadcasting via channel: {:?}", err))?,
                }
            }
            ControllerCommands::ReturnNodeInfo { channel } => {
                self.swarm_command_sender
                    .send(SwarmCommand::ReturnNodeInfo { channel })
                    .await?;
            }
            ControllerCommands::Query(query) => match query {
                DBQuery::ListAll(chan) => {
                    let res = self.store.list_all().await;
                    if let Err(_err) = chan.send(res) {
                        tracing::error!("Error sending list all response back to command sender");
                    }
                }
                DBQuery::Direct(chan) => {
                    if let Err(_err) = chan.send(Ok(self.store.db())) {
                        tracing::error!("Error sending db arc response back to command sender");
                    };
                }
            },
            ControllerCommands::ResolveOnchainMint(claim) => {
                tracing::debug!("Received command to resolve onchain mint, {:?}", claim);
                // This comes from trusted internal checks run by the running node, so safe to trust
                // likely want to add some checks here to ensure the claim is valid in future
                if let Err(err) = self.store.mark_seen_on_chain(claim.clone()).await {
                    tracing::error!(
                        error = err.to_string(),
                        "Error marking premint as seen on chain"
                    );
                } else {
                    tracing::debug!("Marked as seen onchain {:?}", claim.clone());
                }
            }
        }
        Ok(())
    }

    async fn validate_and_insert(&self, premint: PremintTypes) -> eyre::Result<Results> {
        let evaluation = self.rules.evaluate(&premint, self.store.clone()).await?;

        if evaluation.is_accept() {
            self.store
                .store(premint)
                .await
                .map(|_r| evaluation)
                .wrap_err("Failed to store premint")
        } else {
            tracing::info!("Premint failed validation: {:?}", premint);

            Err(evaluation).wrap_err("Premint failed validation")
        }
    }
}

#[derive(Clone)]
pub struct ControllerInterface {
    command_sender: mpsc::Sender<ControllerCommands>,
}

impl ControllerInterface {
    pub fn new(command_sender: mpsc::Sender<ControllerCommands>) -> Self {
        Self { command_sender }
    }

    pub async fn send_command(&self, command: ControllerCommands) -> eyre::Result<()> {
        self.command_sender.send(command).await?;
        Ok(())
    }

    pub async fn get_node_info(&self) -> eyre::Result<MintpoolNodeInfo> {
        let (snd, recv) = oneshot::channel();
        self.send_command(ControllerCommands::ReturnNodeInfo { channel: snd })
            .await?;
        Ok(recv.await?)
    }

    pub async fn get_network_state(&self) -> eyre::Result<NetworkState> {
        let (snd, recv) = oneshot::channel();
        self.send_command(ControllerCommands::ReturnNetworkState { channel: snd })
            .await?;
        Ok(recv.await?)
    }
}
