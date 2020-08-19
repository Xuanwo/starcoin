// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0
use actix::{clock::delay_for, prelude::*};
use anyhow::Result;
use network_rpc_core::server::NetworkRpcServer;
use starcoin_account_api::AccountAsyncService;
use starcoin_account_service::AccountServiceActor;
use starcoin_block_relayer::BlockRelayer;
use starcoin_bus::{Bus, BusActor};
use starcoin_chain::{ChainActor, ChainActorRef};
use starcoin_chain_notify::ChainNotifyHandlerActor;
use starcoin_config::NodeConfig;
use starcoin_dev::playground::PlaygroudService;
use starcoin_genesis::Genesis;
use starcoin_logger::prelude::*;
use starcoin_logger::LoggerHandle;
use starcoin_miner::MinerActor;
use starcoin_miner::MinerClientActor;
use starcoin_network::{NetworkActor, NetworkAsyncService, RawRpcRequestMessage};
use starcoin_network_rpc_api::{
    gen_client::{get_rpc_info, NetworkRpcClient},
    RemoteChainStateReader,
};
use starcoin_rpc_server::module::PubSubService;
use starcoin_rpc_server::RpcActor;
use starcoin_state_service::ChainStateActor;
use starcoin_storage::block_info::BlockInfoStore;
use starcoin_storage::{BlockStore, Storage};
use starcoin_sync::SyncActor;
use starcoin_sync_api::StartSyncTxnEvent;
use starcoin_txpool::{TxPool, TxPoolService};
use starcoin_types::account_config::association_address;
use starcoin_types::peer_info::{PeerInfo, RpcInfo};
use starcoin_types::system_events::SystemStarted;
use std::sync::Arc;
use std::time::Duration;

//TODO rework field and order.
pub struct NodeStartHandle {
    pub config: Arc<NodeConfig>,
    pub bus: Addr<BusActor>,
    pub storage: Arc<Storage>,
    pub chain_arbiter: Arbiter,
    pub chain_actor: ChainActorRef,
    pub miner_actor: Addr<MinerActor<TxPoolService, ChainActorRef, Storage>>,
    pub sync_actor: Addr<SyncActor>,
    pub rpc_actor: Addr<RpcActor>,
    pub miner_client: Option<Addr<MinerClientActor>>,
    pub chain_notifier: Addr<ChainNotifyHandlerActor>,
    pub network: NetworkAsyncService,
    pub network_rpc_server: Addr<NetworkRpcServer>,
    pub block_relayer: Addr<BlockRelayer<TxPoolService>>,
    pub txpool: TxPool,
}

pub async fn start(
    config: Arc<NodeConfig>,
    logger_handle: Option<Arc<LoggerHandle>>,
) -> Result<NodeStartHandle> {
    let bus = BusActor::launch();

    let (storage, startup_info, genesis_hash) = Genesis::init_storage(config.as_ref())?;

    info!("Start chain with startup info: {}", startup_info);

    let account_service = AccountServiceActor::launch(config.clone(), bus.clone())?;

    //Init default account
    let default_account = match account_service.clone().get_default_account().await? {
        Some(account) => account,
        None => {
            //TODO only in dev mod ?
            let wallet_account = account_service
                .clone()
                .create_account("".to_string())
                .await?;
            info!("Create default account: {}", wallet_account.address);
            wallet_account
        }
    };

    //Only dev network association_key_pair contains private_key.
    if let (Some(association_private_key), _) = &config.net().get_config().association_key_pair {
        let association_account = account_service
            .clone()
            .get_account(association_address())
            .await?;
        if association_account.is_none() {
            account_service
                .clone()
                .import_account(
                    association_address(),
                    association_private_key.to_bytes().to_vec(),
                    "".to_string(),
                )
                .await?;
            info!("Import association account to wallet.");
        }
    }

    let head_block_hash = *startup_info.get_master();

    let txpool = TxPool::start(
        config.clone(),
        storage.clone(),
        head_block_hash,
        bus.clone(),
    );
    let txpool_service = txpool.get_service();

    let head_block = match storage.get_block(head_block_hash)? {
        Some(block) => block,
        None => panic!("can't get block by hash {}", head_block_hash),
    };
    let head_block_info = match storage.get_block_info(head_block_hash)? {
        Some(block_info) => block_info,
        None => panic!("can't get block info by hash {}", head_block_hash),
    };
    let peer_id = config
        .network
        .self_peer_id
        .clone()
        .expect("Self peer_id must has been set.");
    let mut rpc_proto_info = Vec::new();
    let chain_rpc_proto_info = get_rpc_info();
    rpc_proto_info.push((
        chain_rpc_proto_info.0.into(),
        RpcInfo::new(chain_rpc_proto_info.1),
    ));
    let self_info = PeerInfo::new_with_proto(
        peer_id.clone(),
        head_block_info.get_total_difficulty(),
        head_block.header().clone(),
        rpc_proto_info,
    );
    let network_config = config.clone();
    let network_bus = bus.clone();
    let (network, rpc_rx) = Arbiter::new()
        .exec(move || -> (NetworkAsyncService, futures::channel::mpsc::UnboundedReceiver<RawRpcRequestMessage>){
            NetworkActor::launch(
                network_config,
                network_bus,
                genesis_hash,
                self_info,
            )
        })
        .await?;

    let head_block = storage
        .get_block(*startup_info.get_master())?
        .expect("Head block must exist.");
    let block_relayer = BlockRelayer::new(bus.clone(), txpool.get_service(), network.clone())?;
    let chain_state_service = ChainStateActor::launch(
        bus.clone(),
        storage.clone(),
        Some(head_block.header().state_root()),
    )?;

    let chain_config = config.clone();
    let chain_storage = storage.clone();
    let chain_bus = bus.clone();
    let chain_txpool_service = txpool_service.clone();

    let remote_state_reader = Some(RemoteChainStateReader::new(NetworkRpcClient::new(
        network.clone(),
    )));
    let chain_arbiter = Arbiter::new();
    let chain = chain_arbiter
        .exec(move || -> Result<ChainActorRef> {
            ChainActor::launch(
                chain_config,
                startup_info,
                chain_storage,
                chain_bus,
                chain_txpool_service,
                remote_state_reader,
            )
        })
        .await??;

    // running in background
    let chain_notify_handler = {
        let bus = bus.clone();
        let storage = storage.clone();
        Actor::start_in_arbiter(&Arbiter::new(), |_ctx| {
            ChainNotifyHandlerActor::new(bus, storage)
        })
    };

    // network rpc server
    let network_rpc_server = starcoin_network_rpc::start_network_rpc_server(
        rpc_rx,
        chain.clone(),
        storage.clone(),
        chain_state_service.clone(),
        txpool_service.clone(),
    )?;

    info!("Self peer_id is: {}", peer_id.to_base58());
    info!(
        "Self address is: {}",
        config
            .network
            .self_address
            .as_ref()
            .expect("Self connect address must has been set.")
    );
    let peer_id = Arc::new(peer_id);
    let sync_config = config.clone();
    let sync_bus = bus.clone();
    let sync_chain = chain.clone();
    let sync_txpool = txpool_service.clone();
    let sync_network = network.clone();
    let sync_storage = storage.clone();
    let sync = Arbiter::new()
        .exec(move || -> Result<Addr<SyncActor>> {
            SyncActor::launch(
                sync_config,
                sync_bus,
                peer_id,
                sync_chain,
                sync_txpool,
                sync_network,
                sync_storage,
            )
        })
        .await??;

    delay_for(Duration::from_secs(1)).await;

    let miner_config = config.clone();
    let miner_bus = bus.clone();
    let miner_storage = storage.clone();
    let miner_txpool = txpool.get_service();
    let miner_chain = chain.clone();
    let miner = MinerActor::<TxPoolService, ChainActorRef, Storage>::launch(
        miner_config,
        miner_bus,
        miner_storage,
        miner_txpool,
        miner_chain,
        default_account,
    )?;
    let miner_client_config = config.miner.client_config.clone();
    let consensus_strategy = config.net().consensus();
    let miner_client = if config.miner.enable_miner_client {
        Some(
            Arbiter::new()
                .exec(move || {
                    MinerClientActor::new(miner_client_config, consensus_strategy).start()
                })
                .await?,
        )
    } else {
        None
    };

    let (json_rpc, _io_handler) = RpcActor::launch(
        config.clone(),
        txpool_service.clone(),
        chain.clone(),
        account_service,
        chain_state_service,
        Some(PlaygroudService::new(storage.clone())),
        Some(PubSubService::new(bus.clone(), txpool_service)),
        Some(network.clone()),
        logger_handle,
    )?;
    bus.clone().broadcast(StartSyncTxnEvent).await.unwrap();
    bus.clone().broadcast(SystemStarted).await?;

    Ok(NodeStartHandle {
        config,
        storage,
        bus,
        chain_arbiter,
        chain_actor: chain,
        miner_actor: miner,
        sync_actor: sync,
        rpc_actor: json_rpc,
        miner_client,
        chain_notifier: chain_notify_handler,
        network,
        network_rpc_server,
        block_relayer,
        txpool,
    })
}
