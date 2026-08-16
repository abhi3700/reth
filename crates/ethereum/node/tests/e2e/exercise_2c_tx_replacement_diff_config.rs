//! Exercise 2c:
//! Run two connected Reth nodes with different transaction replacement policies.
//!
//! Node A uses a 1% price bump.
//! Node B uses the default 10% price bump.
//!
//! The same 1% replacement transaction is therefore:
//!
//! - accepted by Node A
//! - rejected by Node B
//!
//! This demonstrates that mempool contents are local node policy and do not
//! necessarily have to be identical across connected Ethereum nodes.
//!
//! ```
//! cargo test --package reth-node-ethereum --test e2e -- exercise_2c_tx_replacement_diff_config::exercise_2c_tx_replacement_diff_config --exact --nocapture --include-ignored
//! ```

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use reth_chainspec::{ChainSpecBuilder, EthChainSpec};
use reth_e2e_test_utils::{
    setup_engine, transaction::TransactionTestContext, wallet::Wallet, E2ETestSetupBuilder,
};
use reth_node_ethereum::EthereumNode;
use reth_transaction_pool::TransactionPool;
use revm::primitives::ONE_ETHER;
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn exercise_2c_tx_replacement_diff_config() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = Arc::new(
        ChainSpecBuilder::mainnet()
            .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
            .cancun_activated()
            .prague_activated()
            .build(),
    );

    let chain_id = chain_spec.chain_id();

    // --------------------------------------------------
    // 1. Start Node A with a custom 1% replacement bump
    // --------------------------------------------------

    let (mut nodes, _) = E2ETestSetupBuilder::<EthereumNode, _>::new(
        1,
        chain_spec.clone(),
        crate::utils::eth_payload_attributes,
    )
    .with_node_config_modifier(|mut config| {
        // Lower the replacement threshold from the default
        // value of approximately 10% to 1%.
        config.txpool.price_bump = 1;
        config
    })
    .build()
    .await?;

    let mut node_a = nodes.remove(0);

    // Verify Node A's effective pool configuration.
    assert_eq!(node_a.inner.pool().config().price_bumps.default_price_bump, 1);

    // --------------------------------------------------
    // 2. Start Node B with the default 10% policy
    // --------------------------------------------------

    let (mut nodes, _) = setup_engine::<EthereumNode>(
        1,
        chain_spec.clone(),
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
    )
    .await?;

    let mut node_b = nodes.remove(0);

    // Verify Node B still uses the default replacement threshold.
    assert_eq!(node_b.inner.pool().config().price_bumps.default_price_bump, 10);

    // --------------------------------------------------
    // 3. Connect Node A and Node B
    // --------------------------------------------------

    node_a.connect(&mut node_b).await;

    let mut wallets = Wallet::new(1).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);

    // --------------------------------------------------
    // 4. Warm up the chain
    // --------------------------------------------------
    //
    // The seed transaction consumes Alice's nonce 0.
    //
    // Therefore the replacement experiment below will use
    // Alice's next nonce: 1.
    // --------------------------------------------------

    node_a.advance_block().await?;

    let seed_tx = TransactionTestContext::transfer_tx_bytes(chain_id, alice.clone()).await;

    node_a.rpc.inject_tx(seed_tx).await?;

    let warmup_payload = node_a.advance_block().await?;

    // Give Node B the same canonical chain state.
    node_b.submit_payload(warmup_payload.clone()).await?;
    node_b.sync_to(warmup_payload.block().hash()).await?;

    // --------------------------------------------------
    // 5. Alice creates the original pending transaction
    // --------------------------------------------------

    let original_fee = 1_000_000_000u128; // 1 gwei

    let mut tx1 = TxEip1559 {
        chain_id,
        nonce: 1,
        gas_limit: 21_000,
        max_fee_per_gas: original_fee,
        max_priority_fee_per_gas: original_fee,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };

    let signature1 = alice.sign_transaction_sync(&mut tx1)?;
    let envelope1 = TxEnvelope::Eip1559(tx1.into_signed(signature1).into());
    let tx_hash1 = *envelope1.tx_hash();

    // Submit tx1 only to Node A.
    node_a.rpc.inject_tx(envelope1.encoded_2718().into()).await?;

    // Give transaction gossip time to propagate tx1 to Node B.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // --------------------------------------------------
    // 6. Both nodes should now contain tx1
    // --------------------------------------------------

    let alice_txs_on_a = node_a.inner.pool().get_pending_transactions_by_sender(alice.address());

    assert_eq!(alice_txs_on_a.len(), 1);
    assert_eq!(alice_txs_on_a[0].hash(), &tx_hash1);

    let alice_txs_on_b = node_b.inner.pool().get_pending_transactions_by_sender(alice.address());

    assert_eq!(alice_txs_on_b.len(), 1);
    assert_eq!(alice_txs_on_b[0].hash(), &tx_hash1);

    // At this point:
    //
    // Node A:
    //   Alice nonce 1 -> tx1
    //
    // Node B:
    //   Alice nonce 1 -> tx1

    // --------------------------------------------------
    // 7. Create a replacement with only a 1% fee bump
    // --------------------------------------------------

    let replacement_fee = original_fee * 101 / 100;

    assert_eq!(replacement_fee, 1_010_000_000);

    let mut tx2 = TxEip1559 {
        chain_id,

        // Same sender + same nonce makes tx2 a replacement
        // candidate for tx1.
        nonce: 1,

        gas_limit: 21_000,

        // Only 1% more expensive than tx1.
        max_fee_per_gas: replacement_fee,
        max_priority_fee_per_gas: replacement_fee,

        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),

        ..Default::default()
    };

    let signature2 = alice.sign_transaction_sync(&mut tx2)?;
    let envelope2 = TxEnvelope::Eip1559(tx2.into_signed(signature2).into());
    let tx_hash2 = *envelope2.tx_hash();

    assert_ne!(tx_hash1, tx_hash2);

    // --------------------------------------------------
    // 8. Submit the replacement to Node B
    // --------------------------------------------------
    //
    // Node B requires approximately a 10% bump:
    //
    // original    = 1.00 gwei
    // replacement = 1.01 gwei
    // required    ≈ 1.10 gwei
    //
    // Therefore Node B rejects tx2.
    // --------------------------------------------------

    let err = node_b.rpc.inject_tx(envelope2.encoded_2718().into()).await.unwrap_err();

    assert!(err.to_string().contains("replacement transaction underpriced"));

    // Node B must still contain the original transaction.
    let alice_txs_on_b = node_b.inner.pool().get_pending_transactions_by_sender(alice.address());

    assert_eq!(alice_txs_on_b.len(), 1);
    assert_eq!(alice_txs_on_b[0].hash(), &tx_hash1);

    // --------------------------------------------------
    // 9. Submit the EXACT SAME replacement to Node A
    // --------------------------------------------------
    //
    // Node A only requires a 1% bump.
    //
    // Therefore tx2 replaces tx1.
    // --------------------------------------------------

    node_a.rpc.inject_tx(envelope2.encoded_2718().into()).await?;

    let alice_txs_on_a = node_a.inner.pool().get_pending_transactions_by_sender(alice.address());

    assert_eq!(alice_txs_on_a.len(), 1);
    assert_eq!(alice_txs_on_a[0].hash(), &tx_hash2);

    // --------------------------------------------------
    // 10. Allow Node A to gossip tx2 to Node B
    // --------------------------------------------------
    //
    // Node B may receive tx2 from Node A, but transaction
    // gossip does NOT bypass Node B's local txpool policy.
    //
    // Node B should independently reject tx2 as an
    // underpriced replacement and retain tx1.
    // --------------------------------------------------

    tokio::time::sleep(Duration::from_millis(200)).await;

    let alice_txs_on_b = node_b.inner.pool().get_pending_transactions_by_sender(alice.address());

    assert_eq!(alice_txs_on_b.len(), 1);

    // Node B still has the original transaction.
    assert_eq!(alice_txs_on_b[0].hash(), &tx_hash1);

    // Node B did not replace it with tx2.
    assert_ne!(alice_txs_on_b[0].hash(), &tx_hash2);

    println!(
        "✅ Connected nodes have different mempool contents\n\
         Node A (1% bump):  {tx_hash2}\n\
         Node B (10% bump): {tx_hash1}"
    );

    Ok(())
}
