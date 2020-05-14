use crate::metadata::Metadata;
use crate::module::PubSubImpl;
use crate::module::PubSubService;
use anyhow::Result;
use jsonrpc_core::futures as futures01;
use jsonrpc_core::MetaIoHandler;
use jsonrpc_pubsub::Session;
// use starcoin_crypto::hash::HashValue;
use futures::compat::Stream01CompatExt;
use futures::StreamExt;
use starcoin_rpc_api::pubsub::StarcoinPubSub;
use starcoin_txpool_api::TxPoolAsyncService;
use starcoin_types::account_address;
use std::sync::Arc;
use txpool::test_helper::start_txpool;
// use txpool::TxPoolRef;
use starcoin_bus::{Bus, BusActor};
use starcoin_chain::test_helper as chain_test_helper;
use starcoin_config::NodeConfig;
use starcoin_consensus::dev::DevConsensus;
use starcoin_crypto::ed25519::Ed25519PrivateKey;
use starcoin_crypto::{Genesis, PrivateKey};
use starcoin_executor::executor::Executor;
use starcoin_executor::TransactionExecutor;
use starcoin_state_api::AccountStateReader;
use starcoin_traits::{ChainReader, ChainWriter, Consensus};
use starcoin_types::block::BlockDetail;
use starcoin_types::system_events::SystemEvents;
use starcoin_types::transaction::authenticator::AuthenticationKey;
use starcoin_wallet_api::WalletAccount;

#[actix_rt::test]
pub async fn test_subscribe_to_events() -> Result<()> {
    // prepare
    let config = Arc::new(NodeConfig::random_for_test());
    let (_collection, mut block_chain) =
        chain_test_helper::gen_blockchain_for_test::<DevConsensus>(config.clone())?;
    let miner_account = WalletAccount::random();

    let pri_key = Ed25519PrivateKey::genesis();
    let public_key = pri_key.public_key();
    let account_address = account_address::from_public_key(&public_key);
    let txn = {
        let auth_prefix = AuthenticationKey::ed25519(&public_key).prefix().to_vec();
        let txn = Executor::build_mint_txn(account_address, auth_prefix, 1, 10000);
        let txn = txn.as_signed_user_txn()?.clone();
        txn
    };
    let block_template = block_chain.create_block_template(
        *miner_account.address(),
        Some(miner_account.get_auth_key().prefix().to_vec()),
        None,
        vec![txn.clone()],
    )?;
    let new_block = DevConsensus::create_block(config.clone(), &block_chain, block_template)?;
    block_chain.apply(new_block.clone())?;

    let reader = AccountStateReader::new(block_chain.chain_state_reader());
    let balance = reader.get_balance(&account_address)?;
    assert_eq!(balance, Some(10000));

    // now block is applied, we can emit events.

    let service = PubSubService::new();
    let bus = BusActor::launch();
    service.start_chain_notify_handler(bus.clone(), block_chain.storage.clone());
    let pubsub = PubSubImpl::new(service);
    let pubsub = pubsub.to_delegate();

    let mut io = MetaIoHandler::default();
    io.extend_with(pubsub);

    let mut metadata = Metadata::default();
    let (sender, receiver) = futures01::sync::mpsc::channel(8);
    metadata.session = Some(Arc::new(Session::new(sender)));

    // Subscribe
    let request =
        r#"{"jsonrpc": "2.0", "method": "starcoin_subscribe", "params": ["events", {}], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","result":0,"id":1}"#;
    assert_eq!(
        io.handle_request_sync(request, metadata.clone()),
        Some(response.to_owned())
    );

    // Subscribe error
    let request =
        r#"{"jsonrpc": "2.0", "method": "starcoin_subscribe", "params": ["events"], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Couldn't parse parameters: events","data":"\"Expected a filter object.\""},"id":1}"#;
    assert_eq!(
        io.handle_request_sync(request, metadata.clone()),
        Some(response.to_owned())
    );

    // send block
    let block_detail = Arc::new(BlockDetail::new(new_block, 0.into()));
    bus.broadcast(SystemEvents::NewHeadBlock(block_detail))
        .await?;

    let mut receiver = receiver.compat();
    let res = receiver.next().await.transpose().unwrap();
    assert!(res.is_some());

    let res = res.unwrap();
    let notification = serde_json::from_str::<jsonrpc_core::Notification>(res.as_str()).unwrap();
    match notification.params {
        jsonrpc_core::Params::Map(s) => {
            let v = s.get("result").unwrap().get("blockNumber").unwrap();
            assert_eq!(v.as_u64(), Some(1));
        }
        p => {
            assert!(false, "subscribe return unexpected result, {:?}", &p);
        }
    }
    Ok(())
}

#[stest::test]
pub async fn test_subscribe_to_pending_transactions() -> Result<()> {
    // given
    let txpool = start_txpool();
    let service = PubSubService::new();

    service.start_transaction_subscription_handler(txpool.clone());
    let pubsub = PubSubImpl::new(service);
    let pubsub = pubsub.to_delegate();

    let mut io = MetaIoHandler::default();
    io.extend_with(pubsub);

    let mut metadata = Metadata::default();
    let (sender, receiver) = futures01::sync::mpsc::channel(8);
    metadata.session = Some(Arc::new(Session::new(sender)));

    // Fail if params are provided
    let request = r#"{"jsonrpc": "2.0", "method": "starcoin_subscribe", "params": ["newPendingTransactions", {}], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Couldn't parse parameters: newPendingTransactions","data":"\"Expected no parameters.\""},"id":1}"#;
    assert_eq!(
        io.handle_request_sync(request, metadata.clone()),
        Some(response.to_owned())
    );

    // Subscribe
    let request = r#"{"jsonrpc": "2.0", "method": "starcoin_subscribe", "params": ["newPendingTransactions"], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","result":0,"id":1}"#;
    assert_eq!(
        io.handle_request_sync(request, metadata.clone()),
        Some(response.to_owned())
    );
    // Send new transactions
    let txn = {
        let pri_key = Ed25519PrivateKey::genesis();
        let public_key = pri_key.public_key();
        let account_address = account_address::from_public_key(&public_key);
        let auth_prefix = AuthenticationKey::ed25519(&public_key).prefix().to_vec();
        let txn = Executor::build_mint_txn(account_address, auth_prefix, 1, 10000);
        let txn = txn.as_signed_user_txn()?.clone();
        txn
    };
    txpool.clone().add_txns(vec![txn]).await?;
    let mut receiver = receiver.compat();
    let res = receiver.next().await.transpose().unwrap();
    let response = r#"{"jsonrpc":"2.0","method":"starcoin_subscription","params":{"result":["ecd825d29cfa52299a10563146d2674409597b1c8ffed801a69de4bd8ad0e116"],"subscription":0}}"#;
    assert_eq!(res, Some(response.into()));
    // And unsubscribe
    let request = r#"{"jsonrpc": "2.0", "method": "starcoin_unsubscribe", "params": [0], "id": 1}"#;
    let response = r#"{"jsonrpc":"2.0","result":true,"id":1}"#;
    assert_eq!(
        io.handle_request_sync(request, metadata),
        Some(response.to_owned())
    );

    let res = receiver.next().await.transpose().unwrap();
    assert_eq!(res, None);
    Ok(())
}
