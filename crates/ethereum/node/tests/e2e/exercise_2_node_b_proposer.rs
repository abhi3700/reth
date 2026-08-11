//! Node A proposes the warm-up block, then node B receives the next tx and proposes block 2.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{TxKind, U256};
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
async fn node_b_proposes_second_transaction() -> eyre::Result<()> {
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

    let mut wallets = Wallet::new(2).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);
    let bob = wallets.remove(0);

    let (mut nodes, _) = setup_engine_with_connection::<EthereumNode>(
        2,
        chain_spec,
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
        false,
    )
    .await?;
    let mut node_a: NodeHelperType<EthereumNode> = nodes.remove(0);
    let mut node_b: NodeHelperType<EthereumNode> = nodes.remove(0);
    node_a.connect(&mut node_b).await;

    // A receives Alice's nonce-0 transaction and proposes block 1.
    let seed_tx = TransactionTestContext::transfer_tx_bytes(chain_id, alice.clone()).await;
    node_a.rpc.inject_tx(seed_tx).await?;
    let warmup_payload = node_a.advance_block().await?;
    assert_eq!(warmup_payload.block().number, 1);

    node_b.submit_payload(warmup_payload.clone()).await?;
    node_b.sync_to(warmup_payload.block().hash()).await?;

    // B now has A's canonical head, but its payload helper still has its original timestamp.
    node_b.payload.timestamp = node_a.payload.timestamp;

    let bob_balance_before =
        node_a.rpc.inner.eth_api().balance(bob.address(), Default::default()).await?;

    // Alice signs nonce 1, while node B is the RPC ingress node.
    let mut tx = TxEip1559 {
        chain_id,
        nonce: 1,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(bob.address()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature = alice.sign_transaction_sync(&mut tx)?;
    let envelope = TxEnvelope::Eip1559(tx.into_signed(signature));
    let tx_hash = *envelope.tx_hash();

    node_b.rpc.inject_tx(envelope.encoded_2718().into()).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        node_b.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?.is_some(),
        "node B should contain the locally submitted transaction"
    );
    assert!(
        node_a.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?.is_some(),
        "node A should receive the transaction through gossip from B"
    );

    // B's local Engine API flow builds and canonicalizes block 2.
    let payload = node_b.advance_block().await?;
    let block = payload.block();
    assert_eq!(block.number, 2);
    let included_tx = block.body().transactions().next().expect("block 2 should contain a tx");
    assert_eq!(*included_tx.tx_hash(), tx_hash);

    // A receives B's completed payload; this is separate from tx gossip.
    node_a.submit_payload(payload.clone()).await?;
    node_a.sync_to(block.hash()).await?;

    assert_eq!(node_a.rpc.inner.eth_api().block_number()?, 2);
    assert_eq!(node_b.rpc.inner.eth_api().block_number()?, 2);
    assert_eq!(node_a.rpc.inner.eth_api().transaction_count(alice.address(), None).await?, 2);
    assert_eq!(node_b.rpc.inner.eth_api().transaction_count(alice.address(), None).await?, 2);

    let bob_balance_on_a =
        node_a.rpc.inner.eth_api().balance(bob.address(), Default::default()).await?;
    let bob_balance_on_b =
        node_b.rpc.inner.eth_api().balance(bob.address(), Default::default()).await?;
    assert_eq!(bob_balance_on_a - bob_balance_before, U256::from(ONE_ETHER));
    assert_eq!(bob_balance_on_b, bob_balance_on_a);

    Ok(())
}
