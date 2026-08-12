//! Exercise 2b:
//! Lower Reth's transaction replacement threshold to 1%.
//!
//! ```
//! cargo test --package reth-node-ethereum --test e2e -- exercise_2b_tx_replacement_custom_bump::exercise_2b_tx_replacement_custom_bump --exact --nocapture --include-ignored
//! ```

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use reth_chainspec::{ChainSpecBuilder, EthChainSpec};
use reth_e2e_test_utils::{wallet::Wallet, E2ETestSetupBuilder};
use reth_node_ethereum::EthereumNode;
use reth_transaction_pool::TransactionPool;
use revm::primitives::ONE_ETHER;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn exercise_2b_tx_replacement_custom_bump() -> eyre::Result<()> {
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
    // 1. Configure the node before startup
    // --------------------------------------------------
    let (mut nodes, _) = E2ETestSetupBuilder::<EthereumNode, _>::new(
        1,
        chain_spec.clone(),
        crate::utils::eth_payload_attributes,
    )
    .with_node_config_modifier(|mut config| {
        // Default is approximately 10%.
        //
        // Lower the replacement threshold to 1%.
        config.txpool.price_bump = 1;
        config
    })
    .build()
    .await?;
    let node = nodes.remove(0);
    // --------------------------------------------------
    // 2. Inspect the effective pool configuration
    // --------------------------------------------------
    let price_bumps = node.inner.pool().config().price_bumps;
    assert_eq!(price_bumps.default_price_bump, 1);
    let initial_base_fee =
        node.inner.chain_spec().initial_base_fee().expect("Must have initial base fee") as u128;
    let mut wallets = Wallet::new(1).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);
    // --------------------------------------------------
    // 3. Original transaction
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
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice.address());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash(), &tx_hash1);
    // --------------------------------------------------
    // 4. Create a replacement only ~1% more expensive
    // --------------------------------------------------
    let replacement_fee = original_fee * 101 / 100;
    let mut tx2 = TxEip1559 {
        chain_id,
        // Same nonce is what makes tx2 a replacement.
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: replacement_fee,
        max_priority_fee_per_gas: replacement_fee,
        to: TxKind::Call(Address::random()),
        value: U256::from(ONE_ETHER),
        ..Default::default()
    };
    let signature2 = alice.sign_transaction_sync(&mut tx2)?;
    let envelope2 = TxEnvelope::Eip1559(tx2.into_signed(signature2));
    let tx_hash2 = *envelope2.tx_hash();
    // --------------------------------------------------
    // 5. Submit replacement
    // --------------------------------------------------
    node.rpc.inject_tx(envelope2.encoded_2718().into()).await?;
    // If the pool were still using the default 10%
    // replacement policy, this call would have returned:
    //
    // "replacement transaction underpriced"
    // --------------------------------------------------
    // 6. Verify replacement
    // --------------------------------------------------
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice.address());
    assert_eq!(pending.len(), 1, "There should still be only one tx for nonce 0");
    assert_eq!(pending[0].hash(), &tx_hash2, "The replacement should now occupy nonce 0");
    assert_ne!(tx_hash1, tx_hash2);
    println!(
        "✅ Custom 1% threshold accepted replacement\n\
         original:    {tx_hash1}\n\
         replacement: {tx_hash2}"
    );
    Ok(())
}
