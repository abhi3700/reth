//! E2E demo: launch two interconnected test reth nodes, have **Alice** send **1 ETH** to **Bob**,
//! watch the tx enter Alice's tx pool, get gossiped to Bob's tx pool over P2P, then get mined
//! into a block by the payload builder.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_network::TxSignerSync;
use alloy_primitives::{TxKind, U256};
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::{
    setup_engine_with_connection, transaction::TransactionTestContext, wallet::Wallet,
};
use reth_node_ethereum::EthereumNode;
use reth_rpc_api::EthApiServer;
use std::{sync::Arc, time::Duration};

const ONE_ETH_WEI: u128 = 1_000_000_000_000_000_000; // 1 ETH = 10^18 wei

#[tokio::test(flavor = "multi_thread")]
async fn alice_sends_1_eth_to_bob_via_tx_pool_and_p2p() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // 1. Chain spec from the test genesis (funds the hardhat accounts)
    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
            .cancun_activated()
            .prague_activated()
            .build(),
    );
    let chain_id = chain_spec.chain().into();

    // 2. Alice = funded account #0 (0xf39F...), Bob = funded account #1 (0x7099...)
    let mut wallets = Wallet::new(2).with_chain_id(chain_id).wallet_gen();
    let alice = wallets.remove(0);
    let bob = wallets.remove(0);

    // 3. Launch TWO Engine-API test nodes, NOT auto-connected (we wire P2P manually below)
    let (mut nodes, _) = setup_engine_with_connection::<EthereumNode>(
        2,
        chain_spec.clone(),
        false, // drive payloads explicitly via Engine API
        Default::default(),
        crate::utils::eth_payload_attributes,
        false, // don't auto-connect
    )
    .await?;
    let (mut node_a, mut node_b) = (nodes.remove(0), nodes.remove(0));

    // 4. Establish the devp2p session between node A and node B
    node_a.connect(&mut node_b).await;

    // 5. Warm-up: mine block #1 on A, submit it to B, and sync B to it.
    //    This prevents both nodes from considering themselves "unsynced",
    //    a prerequisite for tx pool accept/gossip (same pattern as `test_tx_propagation`).
    let seed_tx = TransactionTestContext::transfer_tx_bytes(chain_id, alice.clone()).await;
    node_a.rpc.inject_tx(seed_tx).await?;
    let warmup_payload = node_a.advance_block().await?;
    node_b.submit_payload(warmup_payload.clone()).await?;
    node_b.sync_to(warmup_payload.block().hash()).await?;

    // 6. Alice signs an EIP-1559 tx: send 1 ETH to Bob (nonce = 1, warm-up used nonce 0)
    let mut tx = TxEip1559 {
        chain_id,
        nonce: 1,
        gas_limit: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: TxKind::Call(bob.address()),
        value: U256::from(ONE_ETH_WEI),
        ..Default::default()
    };
    let signature = alice.sign_transaction_sync(&mut tx).unwrap();
    let envelope = TxEnvelope::Eip1559(tx.into_signed(signature));
    let tx_hash = *envelope.tx_hash();
    println!("Alice -> Bob signed tx: {tx_hash}");

    // 7. Inject the raw tx via `eth_sendRawTransaction` -> node A's tx pool
    let before_bob =
        node_a.rpc.inner.eth_api().balance(bob.address(), Default::default()).await?;
    node_a.rpc.inject_tx(envelope.encoded_2718().into()).await?;
    println!("tx {tx_hash} accepted into node A's tx pool");

    // 8. P2P gossip: node A broadcasts the tx; node B validates & inserts it into its pool
    tokio::time::sleep(Duration::from_millis(200)).await;

    let tx_on_b = node_b.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?;
    assert!(tx_on_b.is_some(), "node B should have the tx in its pool via P2P gossip");
    println!("tx {tx_hash} propagated to node B's tx pool over P2P");
    let tx_on_a = node_a.rpc.inner.eth_api().transaction_by_hash(tx_hash).await?;
    assert!(tx_on_a.is_some(), "node A should still have the tx in its pool");

    // 9. Node A's payload builder drains the tx pool and mines Alice's tx into block #2
    let payload = node_a.advance_block().await?;
    let block = payload.block();
    assert_eq!(block.number, 2, "warm-up block was #1, this is #2");
    let tx_in_block = block.body().transactions().next().expect("block has a tx");
    assert_eq!(*tx_in_block.tx_hash(), tx_hash, "mined block contains Alice's tx");
    println!("tx {tx_hash} mined in block #{}", block.number);

    // 10. Verify balances: Bob is up exactly 1 ETH; Alice paid 1 ETH + gas
    let after_bob = node_a.rpc.inner.eth_api().balance(bob.address(), Default::default()).await?;
    let after_alice =
        node_a.rpc.inner.eth_api().balance(alice.address(), Default::default()).await?;
    assert_eq!(after_bob - before_bob, U256::from(ONE_ETH_WEI), "Bob received exactly 1 ETH");
    println!("Bob balance delta: +1 ETH (now {after_bob})");
    println!("Alice balance after paying 1 ETH + gas: {after_alice}");

    Ok(())
}