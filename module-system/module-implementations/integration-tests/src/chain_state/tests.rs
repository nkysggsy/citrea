use sov_chain_state::{StateTransitionId, TransitionInProgress};
use sov_data_generators::value_setter_data::ValueSetterMessages;
use sov_data_generators::{has_tx_events, new_test_blob_from_batch, MessageGenerator};
use sov_mock_da::{MockBlock, MockBlockHeader, MockDaSpec, MockHash, MockValidityCond};
use sov_mock_zkvm::MockZkvm;
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::storage::StorageManager;
use sov_modules_api::{Spec, WorkingSet};
use sov_modules_stf_blueprint::kernels::basic::BasicKernel;
use sov_modules_stf_blueprint::{SequencerOutcome, StfBlueprint};
use sov_rollup_interface::stf::StateTransitionFunction;
use sov_state::storage_manager::ProverStorageManager;
use sov_state::Storage;

use crate::chain_state::helpers::{create_chain_state_genesis_config, TestRuntime};

type C = DefaultContext;

/// This test generates a new mock rollup having a simple value setter module
/// with an associated chain state, and checks that the height, the genesis hash
/// and the state transitions are correctly stored and updated.
#[test]
fn test_simple_value_setter_with_chain_state() {
    // Build an stf blueprint with the module configurations

    let tmpdir = tempfile::tempdir().unwrap();

    let storage_manager = ProverStorageManager::new(sov_state::config::Config {
        path: tmpdir.path().to_path_buf(),
    })
    .unwrap();

    let stf =
        StfBlueprint::<C, MockDaSpec, MockZkvm, TestRuntime<C, MockDaSpec>, BasicKernel<C>>::new();
    let test_runtime = TestRuntime::<C, MockDaSpec>::default();

    let value_setter_messages = ValueSetterMessages::default();
    let value_setter = value_setter_messages.create_raw_txs::<TestRuntime<C, MockDaSpec>>();

    let admin_pub_key = value_setter_messages.messages[0].admin.default_address();

    // Genesis
    let (init_root_hash, _) = stf.init_chain(
        storage_manager.get_native_storage(),
        create_chain_state_genesis_config(admin_pub_key),
    );

    const MOCK_SEQUENCER_DA_ADDRESS: [u8; 32] = [1_u8; 32];

    let blob = new_test_blob_from_batch(
        sov_modules_stf_blueprint::Batch { txs: value_setter },
        &MOCK_SEQUENCER_DA_ADDRESS,
        [2; 32],
    );

    let slot_data: MockBlock = MockBlock {
        header: MockBlockHeader {
            prev_hash: [0; 32].into(),
            hash: [10; 32].into(),
            height: 0,
        },
        validity_cond: MockValidityCond::default(),
        blobs: Default::default(),
    };

    // Computes the initial working set
    let mut working_set = WorkingSet::new(storage_manager.get_native_storage());

    // Check the slot height before apply slot
    let new_height_storage = test_runtime.chain_state.get_slot_height(&mut working_set);

    assert_eq!(new_height_storage, 0, "The initial height was not computed");

    let result = stf.apply_slot(
        &init_root_hash,
        storage_manager.get_native_storage(),
        Default::default(),
        &slot_data.header,
        &slot_data.validity_cond,
        &mut [blob.clone()],
    );

    assert_eq!(1, result.batch_receipts.len());
    let apply_blob_outcome = result.batch_receipts[0].clone();
    assert_eq!(
        SequencerOutcome::Rewarded(0),
        apply_blob_outcome.inner,
        "Sequencer execution should have succeeded but failed "
    );

    // Computes the new working set after slot application
    let mut working_set = WorkingSet::new(storage_manager.get_native_storage());
    let chain_state_ref = &test_runtime.chain_state;

    let new_root_hash = result.state_root;

    // Check that the root hash has been stored correctly
    let stored_root = chain_state_ref.get_genesis_hash(&mut working_set).unwrap();

    assert_eq!(stored_root, init_root_hash, "Root hashes don't match");

    // Check the slot height
    let new_height_storage = chain_state_ref.get_slot_height(&mut working_set);

    assert_eq!(new_height_storage, 1, "The new height did not update");

    // Check the tx in progress
    let new_tx_in_progress: TransitionInProgress<MockDaSpec> = chain_state_ref
        .get_in_progress_transition(&mut working_set)
        .unwrap();

    assert_eq!(
        new_tx_in_progress,
        TransitionInProgress::<MockDaSpec>::new(
            MockHash::from([10; 32]),
            MockValidityCond::default()
        ),
        "The new transition has not been correctly stored"
    );

    assert!(has_tx_events(&apply_blob_outcome),);

    // We apply a new transaction with the same values
    let new_slot_data: MockBlock = MockBlock {
        header: MockBlockHeader {
            prev_hash: [10; 32].into(),
            hash: [20; 32].into(),
            height: 1,
        },
        validity_cond: MockValidityCond::default(),
        blobs: Default::default(),
    };

    let result = stf.apply_slot(
        &result.state_root,
        storage_manager.get_native_storage(),
        Default::default(),
        &new_slot_data.header,
        &new_slot_data.validity_cond,
        &mut [blob],
    );

    assert_eq!(1, result.batch_receipts.len());
    let apply_blob_outcome = result.batch_receipts[0].clone();
    assert_eq!(
        SequencerOutcome::Rewarded(0),
        apply_blob_outcome.inner,
        "Sequencer execution should have succeeded but failed "
    );

    // Computes the new working set after slot application
    let mut working_set = WorkingSet::new(storage_manager.get_native_storage());
    let chain_state_ref = &test_runtime.chain_state;

    // Check that the root hash has been stored correctly
    let stored_root = chain_state_ref.get_genesis_hash(&mut working_set).unwrap();

    assert_eq!(stored_root, init_root_hash, "Root hashes don't match");

    // Check the slot height
    let new_height_storage = chain_state_ref.get_slot_height(&mut working_set);
    assert_eq!(new_height_storage, 2, "The new height did not update");

    // Check the tx in progress
    let new_tx_in_progress: TransitionInProgress<MockDaSpec> = chain_state_ref
        .get_in_progress_transition(&mut working_set)
        .unwrap();

    assert_eq!(
        new_tx_in_progress,
        TransitionInProgress::<MockDaSpec>::new([20; 32].into(), MockValidityCond::default()),
        "The new transition has not been correctly stored"
    );

    let last_tx_stored: StateTransitionId<
        MockDaSpec,
        <<DefaultContext as Spec>::Storage as Storage>::Root,
    > = chain_state_ref
        .get_historical_transitions(1, &mut working_set)
        .unwrap();

    assert_eq!(
        last_tx_stored,
        StateTransitionId::new([10; 32].into(), new_root_hash, MockValidityCond::default())
    );
}
