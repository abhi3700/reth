//! Exercise 4a:
//! Deploy a smart contract, execute a state-changing contract call,
//! and verify the resulting Ethereum state.
//!
//! This exercise demonstrates the complete contract lifecycle:
//!
//! - deploy contract bytecode
//! - execute the constructor
//! - obtain the contract address from the receipt
//! - verify runtime bytecode with `eth_getCode`
//! - send a state-changing contract call
//! - verify storage with `eth_getStorageAt`
//! - execute a read-only call with `eth_call`
//!
//! ```
//! cargo test --package reth-node-ethereum --test e2e -- exercise_4a_contract_lifecycle::exercise_4a_contract_lifecycle --exact --nocapture --include-ignored
//! ```

use alloy_network::TransactionBuilder;
use alloy_primitives::{Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{sol, SolCall};
use reth_chainspec::{ChainSpecBuilder, MAINNET};
use reth_e2e_test_utils::setup_engine;
use reth_node_ethereum::EthereumNode;
use revm::primitives::ONE_GWEI;
use std::sync::Arc;

sol! {
    #[sol(
        rpc,
        bytecode = "0x6080604052348015600e575f5ffd5b5060405161010a38038061010a833981016040819052602b916031565b5f556047565b5f602082840312156040575f5ffd5b5051919050565b60b7806100535f395ff3fe6080604052348015600e575f5ffd5b5060043610603a575f3560e01c806360fe47b114603e5780636d4ce63c14604f578063c2985578146064575b5f5ffd5b604d6049366004606b565b5f55565b005b5f545b60405190815260200160405180910390f35b60525f5481565b5f60208284031215607a575f5ffd5b503591905056fea264697066735822122040e30ee6d93bebf71509d5b17e11c509362ae2050b8556f774704784b8391d2e64736f6c634300081b0033"
    )]
    contract Storage {
        uint256 public stored;

        constructor(uint256 x) {
            stored = x;
        }

        function set(uint256 x) external {
            stored = x;
        }

        function get() public returns (uint256) {
            return stored;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn exercise_4a_contract_lifecycle() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let chain_spec = Arc::new(
        ChainSpecBuilder::default()
            .chain(MAINNET.chain)
            .genesis(serde_json::from_str(include_str!("../assets/genesis.json")).unwrap())
            .cancun_activated()
            .prague_activated()
            .build(),
    );

    let (mut nodes, wallet) = setup_engine::<EthereumNode>(
        1,
        chain_spec,
        false,
        Default::default(),
        crate::utils::eth_payload_attributes,
    )
    .await?;

    let mut node = nodes.pop().unwrap();

    let alice = wallet.inner;

    let provider = ProviderBuilder::new().wallet(alice.clone()).connect_http(node.rpc_url());

    // --------------------------------------------------
    // 1. Deploy Storage with stored = 42
    // --------------------------------------------------
    //
    // `deploy_builder` creates the contract-creation
    // transaction:
    //
    //     creation bytecode
    //            +
    //     ABI-encoded constructor argument: 42
    //
    // The transaction is sent to Reth but the contract
    // does not become part of canonical state until we
    // mine the transaction.
    // --------------------------------------------------

    let deploy_pending = Storage::deploy_builder(&provider, U256::from(42)).send().await?;

    // --------------------------------------------------
    // 2. Mine the deployment transaction
    // --------------------------------------------------

    node.advance_block().await?;

    let deploy_receipt = deploy_pending.get_receipt().await?;

    assert!(deploy_receipt.status(), "Contract deployment must succeed");

    // --------------------------------------------------
    // 3. Obtain the newly created contract address
    // --------------------------------------------------
    //
    // A successful contract-creation receipt contains the
    // address of the newly deployed contract.
    // --------------------------------------------------

    let contract = deploy_receipt.contract_address.expect("A contract must be deployed");

    println!("Deployed Storage at {contract}");

    // --------------------------------------------------
    // 4. Verify runtime bytecode with eth_getCode
    // --------------------------------------------------
    //
    // During deployment, the EVM executes the creation
    // bytecode and stores the returned runtime bytecode
    // under the new contract account.
    //
    // Therefore:
    //
    //     eth_getCode(contract)
    //
    // must return non-empty bytes.
    // --------------------------------------------------

    let code = provider.get_code_at(contract).await?;

    assert!(!code.is_empty(), "Contract runtime bytecode should exist");

    println!("Contract code is {} bytes", code.len());

    // --------------------------------------------------
    // 5. Verify constructor initialized slot 0 to 42
    // --------------------------------------------------
    //
    // The constructor executed:
    //
    //     stored = 42;
    //
    // `stored` is the first uint256 state variable and
    // therefore occupies storage slot 0.
    // --------------------------------------------------

    let deployed_contract = Storage::new(contract, provider.clone());

    let value = deployed_contract.get().call().await?;

    assert_eq!(value, U256::from(42), "Constructor should initialize stored to 42");

    let slot0_before = provider.get_storage_at(contract, U256::ZERO).await?;

    assert_eq!(slot0_before, U256::from(42), "Storage slot 0 should initially contain 42");

    // --------------------------------------------------
    // 6. Build calldata for set(99)
    // --------------------------------------------------
    //
    // ABI encoding produces:
    //
    //     function selector
    //            +
    //     encoded uint256(99)
    //
    // This becomes the transaction's input/calldata.
    // --------------------------------------------------

    let set_call = Storage::setCall { x: U256::from(99) };

    // --------------------------------------------------
    // 7. Send the state-changing contract transaction
    // --------------------------------------------------
    //
    // Unlike `eth_call`, set(99) modifies Ethereum state.
    //
    // Therefore it must be:
    //
    //     signed
    //       ↓
    //     submitted
    //       ↓
    //     txpool
    //       ↓
    //     included in a block
    //       ↓
    //     executed by the EVM
    // --------------------------------------------------

    let set_pending = provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(contract)
                .with_input(set_call.abi_encode())
                .with_gas_limit(100_000)
                .with_max_fee_per_gas(20_000_000_000)
                .with_max_priority_fee_per_gas(ONE_GWEI),
        )
        .await?;

    // --------------------------------------------------
    // 8. Mine the setter transaction
    // --------------------------------------------------

    node.advance_block().await?;

    let set_receipt = set_pending.get_receipt().await?;

    assert!(set_receipt.status(), "set(99) transaction must succeed");

    // --------------------------------------------------
    // 9. Verify get() now returns 99
    // --------------------------------------------------

    let value = deployed_contract.get().call().await?;

    assert_eq!(value, U256::from(99), "get() should return the updated value");

    // --------------------------------------------------
    // 10. Verify raw contract storage
    // --------------------------------------------------
    //
    // Solidity:
    //
    //     uint256 public stored;
    //
    // occupies slot 0.
    //
    // Therefore:
    //
    //     eth_getStorageAt(contract, 0)
    //
    // should now return 99.
    // --------------------------------------------------

    let slot0_after = provider.get_storage_at(contract, U256::ZERO).await?;

    assert_eq!(slot0_after, U256::from(99), "Storage slot 0 should contain 99");

    // --------------------------------------------------
    // 11. Perform get() manually using raw eth_call
    // --------------------------------------------------
    //
    // Until now Alloy's generated contract binding handled
    // calldata encoding and result decoding for us.
    //
    // Here we manually:
    //
    //     ABI encode get()
    //            ↓
    //         eth_call
    //            ↓
    //     receive raw bytes
    //            ↓
    //     ABI decode uint256
    //
    // This exposes what the generated binding is doing
    // underneath.
    // --------------------------------------------------

    let get_call = Storage::getCall {};

    let result: Bytes = provider
        .raw_request(
            "eth_call".into(),
            (
                TransactionRequest::default().with_to(contract).input(get_call.abi_encode().into()),
                "latest",
            ),
        )
        .await?;

    let decoded = Storage::getCall::abi_decode_returns(&result)?;

    assert_eq!(decoded, U256::from(99), "eth_call get() should return 99");

    println!(
        "✅ Contract lifecycle verified\n\
         Contract: {contract}\n\
         Initial value: 42\n\
         Updated value: 99\n\
         Storage slot 0: {slot0_after}"
    );

    Ok(())
}
