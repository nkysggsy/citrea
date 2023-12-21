use std::net::SocketAddr;

use chainway_sequencer::ChainwaySequencer;
use const_rollup_config::TEST_PRIVATE_KEY;
use demo_stf::genesis_config::GenesisPaths;
use sov_demo_rollup::MockDemoRollup;
use sov_mock_da::{MockAddress, MockDaConfig, MockDaService};
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::default_signature::private_key::DefaultPrivateKey;
use sov_modules_rollup_blueprint::{RollupAndStorage, RollupBlueprint};
use sov_stf_runner::{
    RollupConfig, RollupProverConfig, RpcConfig, RunnerConfig, SequencerClientRpcConfig,
    StorageConfig,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMode {
    FullNode(SocketAddr),
    SequencerNode,
}

pub async fn start_rollup(
    rpc_reporting_channel: oneshot::Sender<SocketAddr>,
    genesis_paths: GenesisPaths,
    rollup_prover_config: RollupProverConfig,
    node_mode: NodeMode,
) {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp_path = temp_dir.path();

    let rollup_config = RollupConfig {
        storage: StorageConfig {
            path: temp_path.to_path_buf(),
        },
        runner: RunnerConfig {
            start_height: 0,
            rpc_config: RpcConfig {
                bind_host: "127.0.0.1".into(),
                bind_port: 0,
            },
        },
        da: MockDaConfig {
            sender_address: MockAddress::from([0; 32]),
        },
        sequencer_client: match node_mode {
            NodeMode::FullNode(socket_addr) => Some(SequencerClientRpcConfig {
                start_height: 1,
                url: format!("http://localhost:{}", socket_addr.port()),
            }),
            NodeMode::SequencerNode => None,
        },
    };

    let mock_demo_rollup = MockDemoRollup {};
    let RollupAndStorage { rollup, storage } = mock_demo_rollup
        .create_new_rollup(&genesis_paths, rollup_config, rollup_prover_config)
        .await
        .unwrap();

    match node_mode {
        NodeMode::FullNode(_) => {
            rollup
                .run_and_report_rpc_port(Some(rpc_reporting_channel))
                .await
                .unwrap();
        }
        NodeMode::SequencerNode => {
            let da_service = MockDaService::new(MockAddress::new([0u8; 32]));

            let mut sequencer: ChainwaySequencer<DefaultContext, MockDaService, _> =
                ChainwaySequencer::new(
                    rollup,
                    da_service,
                    DefaultPrivateKey::from_hex(TEST_PRIVATE_KEY).unwrap(),
                    storage,
                );
            sequencer
                .start_rpc_server(Some(rpc_reporting_channel))
                .await
                .unwrap();
            sequencer.run().await.unwrap();
        }
    }

    // Close the tempdir explicitly to ensure that rustc doesn't see that it's unused and drop it unexpectedly
    temp_dir.close().unwrap();
}
