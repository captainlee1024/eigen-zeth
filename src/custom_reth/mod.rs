use reth_beacon_consensus::BeaconConsensus;
use reth_node_builder::{
    components::{ComponentsBuilder, PayloadServiceBuilder},
    node::NodeTypes,
    BuilderContext, FullNodeTypes, Node, NodeBuilder, PayloadBuilderConfig,
};
use reth_primitives::revm_primitives::{BlockEnv, CfgEnvWithHandlerCfg};
use reth_primitives::{Address, ChainSpec, Header, Withdrawals, B256};
use std::sync::Arc;

use reth_basic_payload_builder::{
    BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig, BuildArguments, BuildOutcome,
    PayloadBuilder, PayloadConfig,
};
use reth_db::init_db;
use reth_node_api::{
    validate_version_specific_fields, AttributesValidationError, EngineApiMessageVersion,
    EngineTypes, PayloadAttributes, PayloadBuilderAttributes, PayloadOrAttributes,
};
use reth_provider::{
    providers::{BlockchainProvider, ProviderFactory},
    CanonStateSubscriptions, StateProviderFactory,
};
use reth_tasks::TaskManager;
use reth_transaction_pool::TransactionPool;

use reth_node_ethereum::{
    node::{EthereumNetworkBuilder, EthereumPoolBuilder},
    EthEvmConfig,
};
use reth_payload_builder::{
    error::PayloadBuilderError, EthBuiltPayload, EthPayloadBuilderAttributes, PayloadBuilderHandle,
    PayloadBuilderService,
};
use reth_rpc_types::{
    engine::{
        ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3,
        PayloadAttributes as EthPayloadAttributes, PayloadId,
    },
    withdrawal::Withdrawal,
    ExecutionPayloadV1,
};
use reth_tracing::{RethTracer, Tracer};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use thiserror::Error;

use crate::custom_reth::eigen::EigenRpcExt;
use crate::custom_reth::eigen::EigenRpcExtApiServer;
use crate::db::Database as RollupDatabase;
use anyhow::{anyhow, Result};
use jsonrpsee::tracing;
use reth_blockchain_tree::{
    BlockchainTree, BlockchainTreeConfig, ShareableBlockchainTree, TreeExternals,
};
use reth_db::mdbx::DatabaseArguments;
use reth_interfaces::consensus::Consensus;
use reth_node_core::args::{DevArgs, RpcServerArgs};
use reth_node_core::dirs::{DataDirPath, MaybePlatformPath};
use reth_node_core::node_config::NodeConfig;
use reth_revm::EvmProcessorFactory;

pub(crate) mod eigen;

/// A custom payload attributes type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomPayloadAttributes {
    /// An inner payload type
    #[serde(flatten)]
    pub inner: EthPayloadAttributes,
    /// A custom field
    pub custom: u64,
}

/// Custom error type used in payload attributes validation
#[derive(Debug, Error)]
pub enum CustomError {
    #[error("Custom field is not zero")]
    CustomFieldIsNotZero,
}

impl PayloadAttributes for CustomPayloadAttributes {
    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        self.inner.withdrawals()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn ensure_well_formed_attributes(
        &self,
        chain_spec: &ChainSpec,
        version: EngineApiMessageVersion,
    ) -> Result<(), AttributesValidationError> {
        validate_version_specific_fields(chain_spec, version, self.into())?;

        // custom validation logic - ensure that the custom field is not zero
        if self.custom == 0 {
            return Err(AttributesValidationError::invalid_params(
                CustomError::CustomFieldIsNotZero,
            ));
        }

        Ok(())
    }
}

/// New type around the payload builder attributes type
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomPayloadBuilderAttributes(EthPayloadBuilderAttributes);

impl PayloadBuilderAttributes for CustomPayloadBuilderAttributes {
    type RpcPayloadAttributes = CustomPayloadAttributes;
    type Error = Infallible;

    fn try_new(parent: B256, attributes: CustomPayloadAttributes) -> Result<Self, Infallible> {
        Ok(Self(EthPayloadBuilderAttributes::new(
            parent,
            attributes.inner,
        )))
    }

    fn payload_id(&self) -> PayloadId {
        self.0.id
    }

    fn parent(&self) -> B256 {
        self.0.parent
    }

    fn timestamp(&self) -> u64 {
        self.0.timestamp
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.0.parent_beacon_block_root
    }

    fn suggested_fee_recipient(&self) -> Address {
        self.0.suggested_fee_recipient
    }

    fn prev_randao(&self) -> B256 {
        self.0.prev_randao
    }

    fn withdrawals(&self) -> &Withdrawals {
        &self.0.withdrawals
    }

    fn cfg_and_block_env(
        &self,
        chain_spec: &ChainSpec,
        parent: &Header,
    ) -> (CfgEnvWithHandlerCfg, BlockEnv) {
        self.0.cfg_and_block_env(chain_spec, parent)
    }
}

/// Custom engine types - uses a custom payload attributes RPC type, but uses the default
/// payload builder attributes type.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[non_exhaustive]
pub struct CustomEngineTypes;

impl EngineTypes for CustomEngineTypes {
    type PayloadAttributes = CustomPayloadAttributes;
    type PayloadBuilderAttributes = CustomPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;
    type ExecutionPayloadV1 = ExecutionPayloadV1;
    type ExecutionPayloadV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadV3 = ExecutionPayloadEnvelopeV3;

    fn validate_version_specific_fields(
        chain_spec: &ChainSpec,
        version: EngineApiMessageVersion,
        payload_or_attrs: PayloadOrAttributes<'_, CustomPayloadAttributes>,
    ) -> Result<(), AttributesValidationError> {
        validate_version_specific_fields(chain_spec, version, payload_or_attrs)
    }
}

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
struct MyCustomNode;

/// Configure the node types
impl NodeTypes for MyCustomNode {
    type Primitives = ();
    // use the custom engine types
    type Engine = CustomEngineTypes;
    // use the default ethereum EVM config
    type Evm = EthEvmConfig;

    fn evm_config(&self) -> Self::Evm {
        Self::Evm::default()
    }
}

/// Implement the Node trait for the custom node
///
/// This provides a preset configuration for the node
impl<N> Node<N> for MyCustomNode
where
    N: FullNodeTypes<Engine = CustomEngineTypes>,
{
    type PoolBuilder = EthereumPoolBuilder;
    type NetworkBuilder = EthereumNetworkBuilder;
    type PayloadBuilder = CustomPayloadServiceBuilder;

    fn components(
        self,
    ) -> ComponentsBuilder<N, Self::PoolBuilder, Self::PayloadBuilder, Self::NetworkBuilder> {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(EthereumPoolBuilder::default())
            .payload(CustomPayloadServiceBuilder::default())
            .network(EthereumNetworkBuilder::default())
    }
}

/// A custom payload service builder that supports the custom engine types
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct CustomPayloadServiceBuilder;

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for CustomPayloadServiceBuilder
where
    Node: FullNodeTypes<Engine = CustomEngineTypes>,
    Pool: TransactionPool + Unpin + 'static,
{
    async fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<Node::Engine>> {
        let payload_builder = CustomPayloadBuilder::default();
        let conf = ctx.payload_builder_config();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval())
            .deadline(conf.deadline())
            .max_payload_tasks(conf.max_payload_tasks())
            .extradata(conf.extradata_rlp_bytes())
            .max_gas_limit(conf.max_gas_limit());

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            ctx.chain_spec(),
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor()
            .spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

/// The type responsible for building custom payloads
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct CustomPayloadBuilder;

impl<Pool, Client> PayloadBuilder<Pool, Client> for CustomPayloadBuilder
where
    Client: StateProviderFactory,
    Pool: TransactionPool,
{
    type Attributes = CustomPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Pool, Client, Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let BuildArguments {
            client,
            pool,
            cached_reads,
            config,
            cancel,
            best_payload,
        } = args;
        let PayloadConfig {
            initialized_block_env,
            initialized_cfg,
            parent_block,
            extra_data,
            attributes,
            chain_spec,
        } = config;

        // This reuses the default EthereumPayloadBuilder to build the payload
        // but any custom logic can be implemented here
        reth_ethereum_payload_builder::EthereumPayloadBuilder::default().try_build(BuildArguments {
            client,
            pool,
            cached_reads,
            config: PayloadConfig {
                initialized_block_env,
                initialized_cfg,
                parent_block,
                extra_data,
                attributes: attributes.0,
                chain_spec,
            },
            cancel,
            best_payload,
        })
    }

    fn build_empty_payload(
        client: &Client,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        let PayloadConfig {
            initialized_block_env,
            initialized_cfg,
            parent_block,
            extra_data,
            attributes,
            chain_spec,
        } = config;
        <reth_ethereum_payload_builder::EthereumPayloadBuilder  as PayloadBuilder<Pool,Client>>  ::build_empty_payload(
            client,
            PayloadConfig { initialized_block_env, initialized_cfg, parent_block, extra_data, attributes: attributes.0, chain_spec }
        )
    }
}

pub async fn launch_custom_node(
    mut stop_rx: tokio::sync::mpsc::Receiver<()>,
    reth_started_signal_channel: tokio::sync::mpsc::Sender<()>,
    rollup_db: Arc<Box<dyn RollupDatabase>>,
    spec: Arc<ChainSpec>,
    rpc_args: RpcServerArgs,
    data_dir: MaybePlatformPath<DataDirPath>,
    dev_args: DevArgs,
) -> Result<()> {
    let _guard = RethTracer::new().init().map_err(|e| anyhow!(e))?;

    let tasks = TaskManager::current();

    let data_dir = data_dir.unwrap_or_chain_default(Default::default());
    let db_path = data_dir.db_path();

    let db_arguments = DatabaseArguments::default();

    tracing::info!(target: "reth::cli", path = ?db_path, "Opening database");
    let database = Arc::new(
        init_db(db_path.clone(), db_arguments)
            .map_err(|e| anyhow!(e))?
            .with_metrics(),
    );

    // create node config
    let node_config = NodeConfig::test()
        .with_rpc(rpc_args)
        .with_chain(spec.clone())
        .with_dev(dev_args);

    let factory =
        ProviderFactory::new(database.clone(), spec.clone(), data_dir.static_files_path())?;

    let consensus: Arc<dyn Consensus> = Arc::new(BeaconConsensus::new(Arc::clone(&spec)));

    let evm_config = EthEvmConfig::default();

    // Configure blockchain tree
    let tree_externals = TreeExternals::new(
        factory.clone(),
        Arc::clone(&consensus),
        EvmProcessorFactory::new(spec.clone(), evm_config),
    );

    let tree = BlockchainTree::new(tree_externals, BlockchainTreeConfig::default(), None)?;
    let blockchain_tree = ShareableBlockchainTree::new(tree);

    let provider = BlockchainProvider::new(factory, blockchain_tree.clone())?;

    let handle = NodeBuilder::new(node_config)
        .with_database(database)
        .with_launch_context(tasks.executor(), data_dir)
        .node(MyCustomNode::default())
        .extend_rpc_modules(move |ctx| {
            // create EigenRpcExt Instance
            let custom_rpc = EigenRpcExt {
                provider: provider.clone(),
                rollup_db: rollup_db.clone(),
            };

            // add EigenRpcExt to RPC modules
            ctx.modules.merge_configured(custom_rpc.into_rpc())?;

            log::info!("EigenRpcExt extension enabled");

            Ok(())
        })
        .on_node_started(move |_ctx| {
            log::info!("[OnNodeStartedHook] Node started");
            // layer2 node started, send the signal to the L2Watcher
            reth_started_signal_channel.try_send(()).unwrap();
            Ok(())
        })
        .launch()
        .await
        .unwrap();

    tokio::select! {
        _ = stop_rx.recv() => {
            log::info!("Node stopped by signal");
        }
        r = handle.node_exit_future => {
            if let Err(e) = r {
                log::error!("Node stopped with error: {:?}", e);
            } else {
                log::info!("Node stopped");
            }
        }
    }

    Ok(())
}
