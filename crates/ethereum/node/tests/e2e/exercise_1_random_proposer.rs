//! A random node receives Alice's second transaction and proposes block 2.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use rand::Rng;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{
    setup_engine_with_connection, transaction::TransactionTestContext, wallet::Wallet,
    NodeHelperType,
};
use reth_node_ethereum::EthereumNode;
use reth_rpc_api::EthApiServer;
use revm::primitives::ONE_ETHER;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn random_node_proposes_second_transaction() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
            .cancun_activated()
            .prague_activated()
            .build(),
    );
    let chain_id = chain_spec.chain().into();

    let mut wallets = Wallet::new(1).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);

    // Spawn ten independent nodes. We establish the devp2p topology ourselves.
    let (mut nodes, _) = setup_engine_with_connection::<EthereumNode>(
        10,
        chain_spec,
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
        false,
    )
    .await?;

    // Node A is the hub. Every other node has one direct session with A.
    {
        let (node_a, spokes) = nodes.split_at_mut(1);
        for spoke in spokes {
            spoke.connect(&mut node_a[0]).await;
        }
    }

    // Alice's nonce-0 transaction warms up node A and becomes block 1.
    let seed_tx = TransactionTestContext::transfer_tx_bytes(chain_id, alice.clone()).await;
    nodes[0].rpc.inject_tx(seed_tx).await?;
    let warmup_payload = nodes[0].advance_block().await?;
    assert_eq!(warmup_payload.block().number, 1);
    let warmup_hash = warmup_payload.block().hash();
    let warmup_timestamp = nodes[0].payload.timestamp;

    // Payload propagation is explicit and separate from transaction gossip.
    for node in nodes.iter().skip(1) {
        node.submit_payload(warmup_payload.clone()).await?;
        node.sync_to(warmup_hash).await?;
    }

    // Any node, including A, may be selected as both RPC ingress and block-2 proposer.
    let proposer_index = rand::rng().random_range(0..nodes.len());
    let mut proposer: NodeHelperType<EthereumNode> = nodes.remove(proposer_index);

    // sync_to() does not advance the helper's local payload-attribute timestamp.
    proposer.payload.timestamp = warmup_timestamp;

    let mut tx = TxEip1559 {
        chain_id,
        nonce: 1,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature = alice.sign_transaction_sync(&mut tx)?;
    let envelope = TxEnvelope::Eip1559(tx.into_signed(signature));
    let tx_hash = *envelope.tx_hash();

    proposer.rpc.inject_tx(envelope.encoded_2718().into()).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        proposer.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?.is_some(),
        "the proposer should retain the transaction in its pool"
    );
    for (peer_position, node) in nodes.iter().enumerate() {
        assert!(
            node.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?.is_some(),
            "remaining peer {peer_position} should receive the transaction through gossip"
        );
    }

    // Calling advance_block makes the selected node act as the proposer for block 2.
    let payload = proposer.advance_block().await?;
    let block = payload.block();
    assert_eq!(block.number, 2);
    let included_tx = block.body().transactions().next().expect("block 2 should contain a tx");
    assert_eq!(*included_tx.tx_hash(), tx_hash);
    let block_hash = block.hash();

    for node in &nodes {
        node.submit_payload(payload.clone()).await?;
        node.sync_to(block_hash).await?;
    }

    assert_eq!(proposer.rpc.inner.eth_api().block_number()?, 2);
    assert_eq!(proposer.rpc.inner.eth_api().transaction_count(alice.address(), None).await?, 2);
    for node in &nodes {
        assert_eq!(node.rpc.inner.eth_api().block_number()?, 2);
        assert_eq!(node.rpc.inner.eth_api().transaction_count(alice.address(), None).await?, 2);
    }

    println!("node {proposer_index} proposed block 2 and all ten nodes accepted it");
    Ok(())
}
