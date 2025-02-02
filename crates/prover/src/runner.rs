use std::collections::VecDeque;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::bail;
use backoff::future::retry as retry_backoff;
use backoff::ExponentialBackoffBuilder;
use borsh::de::BorshDeserialize;
use borsh::BorshSerialize;
use jsonrpsee::core::client::Error as JsonrpseeError;
use jsonrpsee::RpcModule;
use rand::Rng;
use sequencer_client::SequencerClient;
use shared_backup_db::{PostgresConnector, ProofType};
use sov_db::ledger_db::{LedgerDB, SlotCommit};
use sov_db::schema::types::{BatchNumber, SlotNumber, StoredStateTransition};
use sov_modules_api::storage::HierarchicalStorageManager;
use sov_modules_api::{BlobReaderTrait, Context, SignedSoftConfirmationBatch, SlotData};
use sov_modules_stf_blueprint::StfBlueprintTrait;
use sov_rollup_interface::da::{BlockHeaderTrait, DaData, DaSpec, SequencerCommitment};
use sov_rollup_interface::rpc::SoftConfirmationStatus;
use sov_rollup_interface::services::da::DaService;
use sov_rollup_interface::stf::{SoftBatchReceipt, StateTransitionFunction};
use sov_rollup_interface::zk::{Proof, StateTransitionData, ZkvmHost};
use sov_stf_runner::{
    InitVariant, ProverConfig, ProverService, RollupPublicKeys, RpcConfig, RunnerConfig,
};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{debug, error, info, instrument, warn};

type StateRoot<ST, Vm, Da> = <ST as StateTransitionFunction<Vm, Da>>::StateRoot;

pub struct CitreaProver<C, Da, Sm, Vm, Stf, Ps>
where
    C: Context,
    Da: DaService,
    Sm: HierarchicalStorageManager<Da::Spec>,
    Vm: ZkvmHost,
    Stf: StateTransitionFunction<Vm, Da::Spec, Condition = <Da::Spec as DaSpec>::ValidityCondition>
        + StfBlueprintTrait<C, Da::Spec, Vm>,

    Ps: ProverService<Vm>,
{
    start_height: u64,
    da_service: Da,
    stf: Stf,
    storage_manager: Sm,
    /// made pub so that sequencer can clone it
    pub ledger_db: LedgerDB,
    state_root: StateRoot<Stf, Vm, Da::Spec>,
    rpc_config: RpcConfig,
    #[allow(dead_code)]
    prover_service: Option<Ps>,
    sequencer_client: SequencerClient,
    sequencer_pub_key: Vec<u8>,
    sequencer_da_pub_key: Vec<u8>,
    prover_da_pub_key: Vec<u8>,
    phantom: std::marker::PhantomData<C>,
    prover_config: Option<ProverConfig>,
    code_commitment: Vm::CodeCommitment,
}

impl<C, Da, Sm, Vm, Stf, Ps> CitreaProver<C, Da, Sm, Vm, Stf, Ps>
where
    C: Context,
    Da: DaService<Error = anyhow::Error> + Clone + Send + Sync + 'static,
    Sm: HierarchicalStorageManager<Da::Spec>,
    Vm: ZkvmHost,
    Stf: StateTransitionFunction<
            Vm,
            Da::Spec,
            Condition = <Da::Spec as DaSpec>::ValidityCondition,
            PreState = Sm::NativeStorage,
            ChangeSet = Sm::NativeChangeSet,
        > + StfBlueprintTrait<C, Da::Spec, Vm>,
    Ps: ProverService<Vm, StateRoot = Stf::StateRoot, Witness = Stf::Witness, DaService = Da>,
{
    /// Creates a new `StateTransitionRunner`.
    ///
    /// If a previous state root is provided, uses that as the starting point
    /// for execution. Otherwise, initializes the chain using the provided
    /// genesis config.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runner_config: RunnerConfig,
        public_keys: RollupPublicKeys,
        rpc_config: RpcConfig,
        da_service: Da,
        ledger_db: LedgerDB,
        stf: Stf,
        mut storage_manager: Sm,
        init_variant: InitVariant<Stf, Vm, Da::Spec>,
        prover_service: Option<Ps>,
        prover_config: Option<ProverConfig>,
        code_commitment: Vm::CodeCommitment,
    ) -> Result<Self, anyhow::Error> {
        let prev_state_root = match init_variant {
            InitVariant::Initialized(state_root) => {
                debug!("Chain is already initialized. Skipping initialization.");
                state_root
            }
            InitVariant::Genesis(params) => {
                info!("No history detected. Initializing chain...");
                let storage = storage_manager.create_storage_on_l2_height(0)?;
                let (genesis_root, initialized_storage) = stf.init_chain(storage, params);
                storage_manager.save_change_set_l2(0, initialized_storage)?;
                storage_manager.finalize_l2(0)?;
                info!(
                    "Chain initialization is done. Genesis root: 0x{}",
                    hex::encode(genesis_root.as_ref()),
                );
                genesis_root
            }
        };

        // Start the main rollup loop
        let item_numbers = ledger_db.get_next_items_numbers();
        let last_soft_batch_processed_before_shutdown = item_numbers.soft_batch_number;

        let start_height = last_soft_batch_processed_before_shutdown;

        Ok(Self {
            start_height,
            da_service,
            stf,
            storage_manager,
            ledger_db,
            state_root: prev_state_root,
            rpc_config,
            prover_service,
            sequencer_client: SequencerClient::new(runner_config.sequencer_client_url),
            sequencer_pub_key: public_keys.sequencer_public_key,
            sequencer_da_pub_key: public_keys.sequencer_da_pub_key,
            prover_da_pub_key: public_keys.prover_da_pub_key,
            phantom: std::marker::PhantomData,
            prover_config,
            code_commitment,
        })
    }

    /// Starts a RPC server with provided rpc methods.
    pub async fn start_rpc_server(
        &self,
        methods: RpcModule<()>,
        channel: Option<oneshot::Sender<SocketAddr>>,
    ) {
        let bind_host = match self.rpc_config.bind_host.parse() {
            Ok(bind_host) => bind_host,
            Err(e) => {
                error!("Failed to parse bind host: {}", e);
                return;
            }
        };
        let listen_address = SocketAddr::new(bind_host, self.rpc_config.bind_port);

        let max_connections = self.rpc_config.max_connections;

        let _handle = tokio::spawn(async move {
            let server = jsonrpsee::server::ServerBuilder::default()
                .max_connections(max_connections)
                .build([listen_address].as_ref())
                .await;

            match server {
                Ok(server) => {
                    let bound_address = match server.local_addr() {
                        Ok(address) => address,
                        Err(e) => {
                            error!("{}", e);
                            return;
                        }
                    };
                    if let Some(channel) = channel {
                        if let Err(e) = channel.send(bound_address) {
                            error!("Could not send bound_address {}: {}", bound_address, e);
                            return;
                        }
                    }
                    info!("Starting RPC server at {} ", &bound_address);

                    let _server_handle = server.start(methods);
                    futures::future::pending::<()>().await;
                }
                Err(e) => {
                    error!("Could not start RPC server: {}", e);
                }
            }
        });
    }

    /// Runs the prover process.
    #[instrument(level = "trace", skip_all, err)]
    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let skip_submission_until_l1 = std::env::var("SKIP_PROOF_SUBMISSION_UNTIL_L1")
            .map_or(0u64, |v| v.parse().unwrap_or(0));

        // Prover node should sync when a new sequencer commitment arrives
        // Check da block get and sync up to the latest block in the latest commitment
        let last_scanned_l1_height = self
            .ledger_db
            .get_prover_last_scanned_l1_height()
            .unwrap_or_else(|_| panic!("Failed to get last scanned l1 height from the ledger db"));

        let mut l1_height = match last_scanned_l1_height {
            Some(height) => height.0 + 1,
            None => get_initial_slot_height::<Da::Spec>(&self.sequencer_client).await,
        };

        let mut l2_height = self.start_height;

        let prover_config = self.prover_config.clone().unwrap();

        let pg_client = match prover_config.db_config {
            Some(db_config) => {
                info!("Connecting to postgres");
                Some(PostgresConnector::new(db_config.clone()).await)
            }
            None => None,
        };

        loop {
            let da_service = &self.da_service;

            let exponential_backoff = ExponentialBackoffBuilder::new()
                .with_initial_interval(Duration::from_secs(1))
                .with_max_elapsed_time(Some(Duration::from_secs(5 * 60)))
                .build();
            let last_finalized_height = retry_backoff(exponential_backoff.clone(), || async {
                da_service
                    .get_last_finalized_block_header()
                    .await
                    .map_err(backoff::Error::transient)
            })
            .await?
            .height();

            if l1_height > last_finalized_height {
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            let filtered_block = retry_backoff(exponential_backoff.clone(), || async {
                da_service
                    .get_block_at(l1_height)
                    .await
                    .map_err(backoff::Error::transient)
            })
            .await?;

            // map the height to the hash
            self.ledger_db
                .set_l1_height_of_l1_hash(filtered_block.header().hash().into(), l1_height)
                .unwrap();

            let mut sequencer_commitments = Vec::<SequencerCommitment>::new();
            let mut zk_proofs = Vec::<Proof>::new();

            self.da_service
                .extract_relevant_blobs(&filtered_block)
                .into_iter()
                .for_each(|mut tx| {
                    let data = DaData::try_from_slice(tx.full_data());

                    if tx.sender().as_ref() == self.sequencer_da_pub_key.as_slice() {
                        if let Ok(DaData::SequencerCommitment(seq_com)) = data {
                            sequencer_commitments.push(seq_com);
                        } else {
                            warn!(
                                "Found broken DA data in block 0x{}: {:?}",
                                hex::encode(filtered_block.hash()),
                                data
                            );
                        }
                    } else if tx.sender().as_ref() == self.prover_da_pub_key.as_slice() {
                        if let Ok(DaData::ZKProof(proof)) = data {
                            zk_proofs.push(proof);
                        } else {
                            warn!(
                                "Found broken DA data in block 0x{}: {:?}",
                                hex::encode(filtered_block.hash()),
                                data
                            );
                        }
                    } else {
                        warn!("Force transactions are not implemented yet");
                        // TODO: This is where force transactions will land - try to parse DA data force transaction
                    }
                });

            if !zk_proofs.is_empty() {
                warn!("ZK proofs are not empty");
                // TODO: Implement this
            }

            if sequencer_commitments.is_empty() {
                info!("No sequencer commitment found at height {}", l1_height,);

                self.ledger_db
                    .set_prover_last_scanned_l1_height(SlotNumber(l1_height))
                    .unwrap_or_else(|_| {
                        panic!(
                            "Failed to put prover last scanned l1 height in the ledger db {}",
                            l1_height
                        )
                    });

                l1_height += 1;
                continue;
            }

            info!(
                "Processing {} sequencer commitments at height {}",
                sequencer_commitments.len(),
                filtered_block.header().height(),
            );

            let initial_state_root = self.state_root.clone();

            let mut da_data = self.da_service.extract_relevant_blobs(&filtered_block);
            let da_block_header_of_commitments = filtered_block.header().clone();
            let (inclusion_proof, completeness_proof) = self
                .da_service
                .get_extraction_proof(&filtered_block, &da_data)
                .await;

            // if we don't do this, the zk circuit can't read the sequencer commitments
            da_data.iter_mut().for_each(|blob| {
                blob.full_data();
            });

            let mut soft_confirmations: VecDeque<Vec<SignedSoftConfirmationBatch>> =
                VecDeque::new();
            let mut state_transition_witnesses: VecDeque<Vec<Stf::Witness>> = VecDeque::new();
            let mut da_block_headers_of_soft_confirmations: VecDeque<
                Vec<<<Da as DaService>::Spec as DaSpec>::BlockHeader>,
            > = VecDeque::new();

            let mut traversed_l1_tuples = vec![];

            for sequencer_commitment in sequencer_commitments.clone().into_iter() {
                let mut sof_soft_confirmations_to_push = vec![];
                let mut state_transition_witnesses_to_push = vec![];
                let mut da_block_headers_to_push: Vec<
                    <<Da as DaService>::Spec as DaSpec>::BlockHeader,
                > = vec![];

                let start_l1_height = retry_backoff(exponential_backoff.clone(), || async {
                    da_service
                        .get_block_by_hash(sequencer_commitment.l1_start_block_hash)
                        .await
                        .map_err(backoff::Error::transient)
                })
                .await?
                .header()
                .height();

                let end_l1_height = retry_backoff(exponential_backoff.clone(), || async {
                    da_service
                        .get_block_by_hash(sequencer_commitment.l1_end_block_hash)
                        .await
                        .map_err(backoff::Error::transient)
                })
                .await?
                .header()
                .height();
                traversed_l1_tuples.push((start_l1_height, end_l1_height));

                // start fetching blocks from sequencer, when you see a soft batch with l1 height more than end_l1_height, stop
                // while getting the blocks to all the same ops as full node
                // after stopping call continue  and look for a new seq_commitment
                // change the item numbers only after the sync is done so not for every da block

                loop {
                    let inner_client = &self.sequencer_client;
                    let soft_batch =
                        match retry_backoff(exponential_backoff.clone(), || async move {
                            match inner_client.get_soft_batch::<Da::Spec>(l2_height).await {
                                Ok(Some(soft_batch)) => Ok(soft_batch),
                                Ok(None) => {
                                    debug!("Soft Batch: no batch at height {}", l2_height);

                                    // Return a Permanent error so that we exit the retry.
                                    Err(backoff::Error::Permanent(
                                        "No soft batch published".to_owned(),
                                    ))
                                }
                                Err(e) => match e.downcast_ref::<JsonrpseeError>() {
                                    Some(JsonrpseeError::Transport(e)) => {
                                        let error_msg = format!(
                                            "Soft Batch: connection error during RPC call: {:?}",
                                            e
                                        );
                                        error!(error_msg);
                                        Err(backoff::Error::Transient {
                                            err: error_msg,
                                            retry_after: None,
                                        })
                                    }
                                    _ => {
                                        let error_msg = format!(
                                            "Soft Batch: unknown error from RPC call: {:?}",
                                            e
                                        );
                                        error!(error_msg);
                                        Err(backoff::Error::Transient {
                                            err: error_msg,
                                            retry_after: None,
                                        })
                                    }
                                },
                            }
                        })
                        .await
                        {
                            Ok(soft_batch) => soft_batch,
                            Err(_) => {
                                break;
                            }
                        };

                    if soft_batch.da_slot_height > end_l1_height {
                        break;
                    }

                    info!(
                        "Running soft confirmation batch #{} with hash: 0x{} on DA block #{}",
                        l2_height,
                        hex::encode(soft_batch.hash),
                        soft_batch.da_slot_height
                    );

                    let mut signed_soft_confirmation: SignedSoftConfirmationBatch =
                        soft_batch.clone().into();

                    sof_soft_confirmations_to_push.push(signed_soft_confirmation.clone());

                    // The filtered block of soft batch, which is the block at the da_slot_height of soft batch
                    let filtered_block = retry_backoff(exponential_backoff.clone(), || async {
                        da_service
                            .get_block_at(soft_batch.da_slot_height)
                            .await
                            .map_err(backoff::Error::transient)
                    })
                    .await?;

                    if da_block_headers_to_push.is_empty()
                        || da_block_headers_to_push.last().unwrap().height()
                            != filtered_block.header().height()
                    {
                        da_block_headers_to_push.push(filtered_block.header().clone());
                    }

                    let mut data_to_commit = SlotCommit::new(filtered_block.clone());

                    let pre_state = self
                        .storage_manager
                        .create_storage_on_l2_height(l2_height)?;

                    let slot_result = self.stf.apply_soft_batch(
                        self.sequencer_pub_key.as_slice(),
                        // TODO(https://github.com/Sovereign-Labs/sovereign-sdk/issues/1247): incorrect pre-state root in case of re-org
                        &self.state_root,
                        pre_state,
                        Default::default(),
                        filtered_block.header(),
                        &filtered_block.validity_condition(),
                        &mut signed_soft_confirmation,
                    );

                    state_transition_witnesses_to_push.push(slot_result.witness);

                    for receipt in slot_result.batch_receipts {
                        data_to_commit.add_batch(receipt);
                    }

                    self.storage_manager
                        .save_change_set_l2(l2_height, slot_result.change_set)?;

                    let batch_receipt = data_to_commit.batch_receipts()[0].clone();

                    let next_state_root = slot_result.state_root;

                    // Check if post state root is the same as the one in the soft batch
                    if next_state_root.as_ref().to_vec() != soft_batch.post_state_root {
                        bail!("Post state root mismatch")
                    }

                    let soft_batch_receipt = SoftBatchReceipt::<_, _, Da::Spec> {
                        pre_state_root: self.state_root.as_ref().to_vec(),
                        post_state_root: next_state_root.as_ref().to_vec(),
                        phantom_data: PhantomData::<u64>,
                        batch_hash: batch_receipt.batch_hash,
                        da_slot_hash: filtered_block.header().hash(),
                        da_slot_height: filtered_block.header().height(),
                        da_slot_txs_commitment: filtered_block.header().txs_commitment(),
                        tx_receipts: batch_receipt.tx_receipts,
                        soft_confirmation_signature: soft_batch.soft_confirmation_signature,
                        pub_key: soft_batch.pub_key,
                        deposit_data: soft_batch.deposit_data.into_iter().map(|x| x.tx).collect(),
                        l1_fee_rate: soft_batch.l1_fee_rate,
                        timestamp: soft_batch.timestamp,
                    };

                    self.ledger_db.commit_soft_batch(soft_batch_receipt, true)?;
                    self.ledger_db.extend_l2_range_of_l1_slot(
                        SlotNumber(filtered_block.header().height()),
                        BatchNumber(l2_height),
                    )?;

                    self.state_root = next_state_root;

                    info!(
                        "New State Root after soft confirmation #{} is: {:?}",
                        l2_height, self.state_root
                    );

                    self.storage_manager.finalize_l2(l2_height)?;

                    l2_height += 1;
                }

                soft_confirmations.push_back(sof_soft_confirmations_to_push);
                state_transition_witnesses.push_back(state_transition_witnesses_to_push);
                da_block_headers_of_soft_confirmations.push_back(da_block_headers_to_push);
            }

            info!("Sending for proving");

            let hash = da_block_header_of_commitments.hash();

            let transition_data: StateTransitionData<Stf::StateRoot, Stf::Witness, Da::Spec> =
                StateTransitionData {
                    initial_state_root,
                    final_state_root: self.state_root.clone(),
                    da_data,
                    da_block_header_of_commitments,
                    inclusion_proof,
                    completeness_proof,
                    soft_confirmations,
                    state_transition_witnesses,
                    da_block_headers_of_soft_confirmations,

                    sequencer_public_key: self.sequencer_pub_key.clone(),
                    sequencer_da_public_key: self.sequencer_da_pub_key.clone(),
                };

            let should_prove: bool = {
                let mut rng = rand::thread_rng();
                // if proof_sampling_number is 0, then we always prove and submit
                // otherwise we submit and prove with a probability of 1/proof_sampling_number
                if prover_config.proof_sampling_number == 0 {
                    true
                } else {
                    rng.gen_range(0..prover_config.proof_sampling_number) == 0
                }
            };

            // Skip submission until l1 height
            if l1_height >= skip_submission_until_l1 && should_prove {
                let prover_service = self
                    .prover_service
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Prover service is not initialized"))?;

                prover_service.submit_witness(transition_data).await;

                prover_service.prove(hash.clone()).await?;

                let (tx_id, proof) = prover_service
                    .wait_for_proving_and_send_to_da(hash.clone(), &self.da_service)
                    .await?;

                let tx_id_u8 = tx_id.into();

                // l1_height => (tx_id, proof, transition_data)
                // save proof along with tx id to db, should be queriable by slot number or slot hash
                let transition_data: sov_modules_api::StateTransition<
                    <Da as DaService>::Spec,
                    Stf::StateRoot,
                > = Vm::extract_output(&proof).expect("Proof should be deserializable");

                match proof {
                    Proof::PublicInput(_) => {
                        warn!("Proof is public input, skipping");
                    }
                    Proof::Full(ref proof) => {
                        info!("Verifying proof!");
                        let transition_data_from_proof = Vm::verify_and_extract_output::<
                            <Da as DaService>::Spec,
                            Stf::StateRoot,
                        >(
                            &proof.clone(), &self.code_commitment
                        )
                        .expect("Proof should be verifiable");

                        info!(
                            "transition data from proof: {:?}",
                            transition_data_from_proof
                        );
                    }
                }

                info!("transition data: {:?}", transition_data);

                let stored_state_transition = StoredStateTransition {
                    initial_state_root: transition_data.initial_state_root.as_ref().to_vec(),
                    final_state_root: transition_data.final_state_root.as_ref().to_vec(),
                    state_diff: transition_data.state_diff,
                    da_slot_hash: transition_data.da_slot_hash.into(),
                    sequencer_public_key: transition_data.sequencer_public_key,
                    sequencer_da_public_key: transition_data.sequencer_da_public_key,
                    validity_condition: transition_data.validity_condition.try_to_vec().unwrap(),
                };

                match pg_client.as_ref() {
                    Some(Ok(pool)) => {
                        info!("Inserting proof data into postgres");
                        let (proof_data, proof_type) = match proof.clone() {
                            Proof::Full(full_proof) => (full_proof, ProofType::Full),
                            Proof::PublicInput(public_input) => {
                                (public_input, ProofType::PublicInput)
                            }
                        };
                        pool.insert_proof_data(
                            tx_id_u8.to_vec(),
                            proof_data,
                            stored_state_transition.clone().into(),
                            proof_type,
                        )
                        .await
                        .unwrap();
                    }
                    _ => {
                        warn!("No postgres client found");
                    }
                }

                self.ledger_db.put_proof_data(
                    l1_height,
                    tx_id_u8,
                    proof,
                    stored_state_transition,
                )?;
            } else {
                info!("Skipping proving for l1 height {}", l1_height);
            }

            for (sequencer_commitment, l1_heights) in
                sequencer_commitments.into_iter().zip(traversed_l1_tuples)
            {
                // Save commitments on prover ledger db
                self.ledger_db
                    .update_commitments_on_da_slot(l1_height, sequencer_commitment.clone())
                    .unwrap();

                for i in l1_heights.0..=l1_heights.1 {
                    self.ledger_db
                        .put_soft_confirmation_status(
                            SlotNumber(i),
                            SoftConfirmationStatus::Finalized,
                        )
                        .unwrap_or_else(|_| {
                            panic!(
                                "Failed to put soft confirmation status in the ledger db {}",
                                i
                            )
                        });
                }
            }

            self.ledger_db
                .set_prover_last_scanned_l1_height(SlotNumber(l1_height))?;
            l1_height += 1;
        }
    }
}

async fn get_initial_slot_height<Da: DaSpec>(client: &SequencerClient) -> u64 {
    loop {
        match client.get_soft_batch::<Da>(1).await {
            Ok(Some(batch)) => return batch.da_slot_height,
            _ => {
                // sleep 1
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        }
    }
}
