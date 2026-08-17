//! Exercise 3a: Nonce-gap queueing behavior in the transaction pool.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{setup_engine, wallet::Wallet};
use reth_node_ethereum::EthereumNode;
use reth_rpc_api::EthApiServer;
use reth_transaction_pool::TransactionPool;
use std::sync::Arc;

fn build_tx(chain_id: u64, alice: &PrivateKeySigner, nonce: u64) -> TxEnvelope {
    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(Address::random()),
        value: U256::from(100),
        ..Default::default()
    };

    let sig = alice.sign_transaction_sync(&mut tx).unwrap();
    TxEnvelope::Eip1559(tx.into_signed(sig))
}

#[tokio::test(flavor = "multi_thread")]
async fn exercise_3a_pool_nonce_gap() -> eyre::Result<()> {
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

    let (mut nodes, _) = setup_engine::<EthereumNode>(
        1,
        chain_spec,
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
    )
    .await?;

    let mut node = nodes.pop().unwrap();

    let wallet = Wallet::new(1).with_chain_id(chain_id);
    let mut wallets = wallet.wallet_gen();
    let alice = wallets.remove(0);
    let alice_addr = alice.address();

    // 1. Submit nonces 0, 2, and 3. Nonce 1 is intentionally missing.
    let tx0 = build_tx(chain_id, &alice, 0);
    let tx2 = build_tx(chain_id, &alice, 2);
    let tx3 = build_tx(chain_id, &alice, 3);

    println!("Added txs - 0, 2, 3 to pool");
    node.rpc.inject_tx(tx0.encoded_2718().into()).await?;
    node.rpc.inject_tx(tx2.encoded_2718().into()).await?;
    node.rpc.inject_tx(tx3.encoded_2718().into()).await?;

    // 2. Only nonce 0 is executable.
    //
    //    nonce 0 -> pending
    //    nonce 1 -> missing
    //    nonce 2 -> queued
    //    nonce 3 -> queued
    let pending_nonce = node.rpc.inner.eth_api().transaction_count(alice_addr, None).await?;
    assert_eq!(pending_nonce, 0, "Only nonce 0 is executable while nonce 1 is missing");

    let pending = node.inner.pool().get_pending_transactions_by_sender(alice_addr);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].hash(), tx0.hash());
    println!("Pending: tx0");
    let queued = node.inner.pool().get_queued_transactions_by_sender(alice_addr);
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].hash(), tx2.hash());
    assert_eq!(queued[1].hash(), tx3.hash());
    println!("Queue: tx2, tx3");

    println!("Since, tx0 is in pending, so only tx0 can be mined or added to block.");

    // 3. Mine nonce 0.
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().number, 1);

    let pending_nonce = node.rpc.inner.eth_api().transaction_count(alice_addr, None).await?;
    assert_eq!(pending_nonce, 1, "Only nonce 1 is executable after nonce-0");

    println!("Alice's canonical nonce is now 1, but nonces 2 and 3 remain blocked.");

    // 4. Submit the missing nonce 1 transaction.
    let tx1 = build_tx(chain_id, &alice, 1);

    node.rpc.inject_tx(tx1.encoded_2718().into()).await?;

    println!("Now, tx1 is added to pool.");
    let pending = node.inner.pool().get_pending_transactions_by_sender(alice_addr);
    assert_eq!(pending.len(), 3);
    assert_eq!(pending[0].hash(), tx1.hash());
    assert_eq!(pending[1].hash(), tx2.hash());
    assert_eq!(pending[2].hash(), tx3.hash());
    println!("Pending: tx1, tx2, tx3");
    let queued = node.inner.pool().get_queued_transactions_by_sender(alice_addr);
    assert_eq!(queued.len(), 0);
    println!("Queue:");
    println!("So, tx2, tx3 are automatically moved from \'queue\' to \'pending\' sub-pool.");

    //  Mine another block. It should contain all three newly executable transactions.
    // Now you can mine all 3 txs: tx1, tx2, tx3 into a block automatically. When tx1 added to
    // pool, tx2, tx3 automatically moves from 'queue' to 'pending' sub-pool. Hence, 3 txs ready to
    // be included in next block.
    let payload = node.advance_block().await?;
    let block = payload.block();
    assert_eq!(block.number, 2);
    assert_eq!(payload.block().transaction_count(), 3);

    let mined_hashes: Vec<_> = block.body().transactions().map(|tx| *tx.tx_hash()).collect();
    assert!(mined_hashes.contains(tx1.hash()), "Block #2 must contain tx1");
    assert!(mined_hashes.contains(tx2.hash()), "Block #2 must contain tx2");
    assert!(mined_hashes.contains(tx3.hash()), "Block #2 must contain tx3");

    println!("✅ Nonce-gap queueing verified: nonce 1 unlocked queued nonces 2 and 3");

    // 5. Filling nonce 1 unlocks the contiguous sequence 1 -> 2 -> 3.
    let pending_nonce_after = node.rpc.inner.eth_api().transaction_count(alice_addr, None).await?;

    assert_eq!(
        pending_nonce_after, 4,
        "Nonces 1, 2, and 3 should become executable after nonce 1 arrives"
    );

    let pending_txs = node.inner.pool.get_pending_transactions_by_sender(alice_addr);
    assert_eq!(pending_txs.len(), 0, "No pending txs should be present in pool");

    Ok(())
}
