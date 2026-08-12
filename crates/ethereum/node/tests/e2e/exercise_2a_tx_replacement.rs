//! Exercise 2a:
//! Observe Reth's default transaction replacement threshold.
//!
//! ```
//! cargo test --package reth-node-ethereum --test e2e -- exercise_2a_tx_replacement::exercise_2a_tx_replacement --exact --nocapture --include-ignored
//! ```

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use reth_chainspec::{ChainSpecBuilder, EthChainSpec};
use reth_e2e_test_utils::{setup_engine, wallet::Wallet};
use reth_node_ethereum::EthereumNode;
use reth_transaction_pool::TransactionPool;
use revm::primitives::ONE_ETHER;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn exercise_2a_tx_replacement() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let chain_spec = Arc::new(
        ChainSpecBuilder::mainnet()
            .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
            .cancun_activated()
            .prague_activated()
            .build(),
    );
    let chain_id = chain_spec.chain_id();
    let (mut nodes, _) = setup_engine::<EthereumNode>(
        1,
        chain_spec.clone(),
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
    )
    .await?;
    let node = nodes.remove(0);
    let initial_base_fee =
        node.inner.chain_spec().initial_base_fee().expect("Must have initial base fee") as u128;
    let mut wallets = Wallet::new(1).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);
    // --------------------------------------------------
    // 1. Original transaction
    // --------------------------------------------------
    let original_fee = initial_base_fee;
    let mut tx1 = TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: original_fee,
        max_priority_fee_per_gas: original_fee,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature1 = alice.sign_transaction_sync(&mut tx1)?;
    let envelope1 = TxEnvelope::Eip1559(tx1.into_signed(signature1));
    let tx_hash1 = *envelope1.tx_hash();
    node.rpc.inject_tx(envelope1.encoded_2718().into()).await?;
    // Alice should currently have exactly one pending tx.
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice.address());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash(), &tx_hash1);
    // --------------------------------------------------
    // 2. Attempt a tiny replacement
    // --------------------------------------------------
    let tiny_bump = original_fee + 1;
    let mut tx2 = TxEip1559 {
        chain_id,
        // Same sender + same nonce => replacement candidate.
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: tiny_bump,
        max_priority_fee_per_gas: tiny_bump,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature2 = alice.sign_transaction_sync(&mut tx2)?;
    let envelope2 = TxEnvelope::Eip1559(tx2.into_signed(signature2));
    let result = node.rpc.inject_tx(envelope2.encoded_2718().into()).await;
    assert_eq!(result.unwrap_err().to_string(), "replacement transaction underpriced");
    // The original transaction should still be present.
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice.address());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash(), &tx_hash1);
    // --------------------------------------------------
    // 3. Replacement satisfying the default threshold
    // --------------------------------------------------
    let replacement_fee = original_fee * 110 / 100;
    let mut tx3 = TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: replacement_fee,
        max_priority_fee_per_gas: replacement_fee,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature3 = alice.sign_transaction_sync(&mut tx3)?;
    let envelope3 = TxEnvelope::Eip1559(tx3.into_signed(signature3));
    let tx_hash3 = *envelope3.tx_hash();
    node.rpc.inject_tx(envelope3.encoded_2718().into()).await?;
    // Still only one tx for nonce 0,
    // but now it should be the replacement.
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice.address());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash(), &tx_hash3);
    println!("✅ Original {tx_hash1} replaced by {tx_hash3}");
    Ok(())
}
