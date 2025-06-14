//! Optimism Node types config.

use crate::{
    args::RollupArgs,
    engine::OpEngineValidator,
    txpool::{OpTransactionPool, OpTransactionValidator},
    OpEngineApiBuilder, OpEngineTypes,
};
use op_alloy_consensus::{interop::SafetyLevel, OpPooledTransaction};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use reth_chainspec::{ChainSpecProvider, EthChainSpec, Hardforks};
use reth_evm::{ConfigureEvm, EvmFactory, EvmFactoryFor};
use reth_network::{
    primitives::NetPrimitivesFor, NetworkConfig, NetworkHandle, NetworkManager, NetworkPrimitives,
    PeersInfo,
};
use reth_node_api::{
    AddOnsContext, FullNodeComponents, KeyHasherTy, NodeAddOns, NodePrimitives, PayloadTypes,
    PrimitivesTy, TxTy,
};
use reth_node_builder::{
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ConsensusBuilder, ExecutorBuilder,
        NetworkBuilder, PayloadBuilderBuilder, PoolBuilder, PoolBuilderConfigOverrides,
    },
    node::{FullNodeTypes, NodeTypes},
    rpc::{
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, RethRpcAddOns, RpcAddOns,
        RpcHandle,
    },
    BuilderContext, DebugNode, Node, NodeAdapter, NodeComponentsBuilder,
};
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_consensus::OpBeaconConsensus;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes, OpRethReceiptBuilder};
use reth_optimism_forks::OpHardforks;
use reth_optimism_payload_builder::{
    builder::OpPayloadTransactions,
    config::{OpBuilderConfig, OpDAConfig},
    OpBuiltPayload, OpPayloadBuilderAttributes, OpPayloadPrimitives,
};
use reth_optimism_primitives::{DepositReceipt, OpPrimitives, OpReceipt, OpTransactionSigned};
use reth_optimism_rpc::{
    eth::{ext::OpEthExtApi, OpEthApiBuilder},
    miner::{MinerApiExtServer, OpMinerExtApi},
    witness::{DebugExecutionWitnessApiServer, OpDebugWitnessApi},
    OpEthApi, OpEthApiError, SequencerClient,
};
use reth_optimism_txpool::{
    supervisor::{SupervisorClient, DEFAULT_SUPERVISOR_URL},
    OpPooledTx,
};
use reth_provider::{providers::ProviderFactoryBuilder, CanonStateSubscriptions, EthStorage};
use reth_rpc_api::DebugApiServer;
use reth_rpc_eth_api::{ext::L2EthApiExtServer, FullEthApiServer};
use reth_rpc_eth_types::error::FromEvmError;
use reth_rpc_server_types::RethRpcModule;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, CoinbaseTipOrdering, EthPoolTransaction, PoolTransaction,
    TransactionPool, TransactionValidationTaskExecutor,
};
use reth_trie_db::MerklePatriciaTrie;
use revm::context::TxEnv;
use std::{marker::PhantomData, sync::Arc};

/// Marker trait for Optimism node types with standard engine, chain spec, and primitives.
pub trait OpNodeTypes:
    NodeTypes<Payload = OpEngineTypes, ChainSpec = OpChainSpec, Primitives = OpPrimitives>
{
}
/// Blanket impl for all node types that conform to the Optimism spec.
impl<N> OpNodeTypes for N where
    N: NodeTypes<Payload = OpEngineTypes, ChainSpec = OpChainSpec, Primitives = OpPrimitives>
{
}
/// Storage implementation for Optimism.
pub type OpStorage = EthStorage<OpTransactionSigned>;

/// Type configuration for a regular Optimism node.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpNode {
    /// Additional Optimism args
    pub args: RollupArgs,
    /// Data availability configuration for the OP builder.
    ///
    /// Used to throttle the size of the data availability payloads (configured by the batcher via
    /// the `miner_` api).
    ///
    /// By default no throttling is applied.
    pub da_config: OpDAConfig,
}

impl OpNode {
    /// Creates a new instance of the Optimism node type.
    pub fn new(args: RollupArgs) -> Self {
        Self { args, da_config: OpDAConfig::default() }
    }

    /// Configure the data availability configuration for the OP builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = da_config;
        self
    }

    /// Returns the components for the given [`RollupArgs`].
    pub fn components<Node>(
        &self,
    ) -> ComponentsBuilder<
        Node,
        OpPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types: OpNodeTypes>,
    {
        let RollupArgs { disable_txpool_gossip, compute_pending_block, discovery_v4, .. } =
            self.args;
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(
                OpPoolBuilder::default()
                    .with_enable_tx_conditional(self.args.enable_tx_conditional)
                    .with_supervisor(
                        self.args.supervisor_http.clone(),
                        self.args.supervisor_safety_level,
                    ),
            )
            .executor(OpExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                OpPayloadBuilder::new(compute_pending_block).with_da_config(self.da_config.clone()),
            ))
            .network(OpNetworkBuilder::new(disable_txpool_gossip, !discovery_v4))
            .consensus(OpConsensusBuilder::default())
    }

    /// Instantiates the [`ProviderFactoryBuilder`] for an opstack node.
    ///
    /// # Open a Providerfactory in read-only mode from a datadir
    ///
    /// See also: [`ProviderFactoryBuilder`] and
    /// [`ReadOnlyConfig`](reth_provider::providers::ReadOnlyConfig).
    ///
    /// ```no_run
    /// use reth_optimism_chainspec::BASE_MAINNET;
    /// use reth_optimism_node::OpNode;
    ///
    /// let factory =
    ///     OpNode::provider_factory_builder().open_read_only(BASE_MAINNET.clone(), "datadir").unwrap();
    /// ```
    ///
    /// # Open a Providerfactory manually with all required components
    ///
    /// ```no_run
    /// use reth_db::open_db_read_only;
    /// use reth_optimism_chainspec::OpChainSpecBuilder;
    /// use reth_optimism_node::OpNode;
    /// use reth_provider::providers::StaticFileProvider;
    /// use std::sync::Arc;
    ///
    /// let factory = OpNode::provider_factory_builder()
    ///     .db(Arc::new(open_db_read_only("db", Default::default()).unwrap()))
    ///     .chainspec(OpChainSpecBuilder::base_mainnet().build().into())
    ///     .static_file(StaticFileProvider::read_only("db/static_files", false).unwrap())
    ///     .build_provider_factory();
    /// ```
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }
}

impl<N> Node<N> for OpNode
where
    N: FullNodeTypes<
        Types: NodeTypes<
            Payload = OpEngineTypes,
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
            Storage = OpStorage,
        >,
    >,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        OpPoolBuilder,
        BasicPayloadServiceBuilder<OpPayloadBuilder>,
        OpNetworkBuilder,
        OpExecutorBuilder,
        OpConsensusBuilder,
    >;

    type AddOns = OpAddOns<
        NodeAdapter<N, <Self::ComponentsBuilder as NodeComponentsBuilder<N>>::Components>,
        OpEthApiBuilder,
    >;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components(self)
    }

    fn add_ons(&self) -> Self::AddOns {
        Self::AddOns::builder()
            .with_sequencer(self.args.sequencer.clone())
            .with_sequencer_headers(self.args.sequencer_headers.clone())
            .with_da_config(self.da_config.clone())
            .with_enable_tx_conditional(self.args.enable_tx_conditional)
            .build()
    }
}

impl<N> DebugNode<N> for OpNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types_eth::Block<op_alloy_consensus::OpTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        rpc_block.into_consensus()
    }
}

impl NodeTypes for OpNode {
    type Primitives = OpPrimitives;
    type ChainSpec = OpChainSpec;
    type StateCommitment = MerklePatriciaTrie;
    type Storage = OpStorage;
    type Payload = OpEngineTypes;
}

/// Add-ons w.r.t. optimism.
#[derive(Debug)]
pub struct OpAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N>,
    EV = OpEngineValidatorBuilder,
    EB = OpEngineApiBuilder<OpEngineValidatorBuilder>,
> {
    /// Rpc add-ons responsible for launching the RPC servers and instantiating the RPC handlers
    /// and eth-api.
    pub rpc_add_ons: RpcAddOns<N, EthB, EV, EB>,
    /// Data availability configuration for the OP builder.
    pub da_config: OpDAConfig,
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    pub sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    pub sequencer_headers: Vec<String>,
    /// Enable transaction conditionals.
    enable_tx_conditional: bool,
}

impl<N, NetworkT> Default for OpAddOns<N, OpEthApiBuilder<NetworkT>>
where
    N: FullNodeComponents<Types: NodeTypes>,
    OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
{
    fn default() -> Self {
        Self::builder().build()
    }
}

impl<N, NetworkT> OpAddOns<N, OpEthApiBuilder<NetworkT>>
where
    N: FullNodeComponents<Types: NodeTypes>,
    OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
{
    /// Build a [`OpAddOns`] using [`OpAddOnsBuilder`].
    pub fn builder() -> OpAddOnsBuilder<NetworkT> {
        OpAddOnsBuilder::default()
    }
}

impl<N, NetworkT> NodeAddOns<N> for OpAddOns<N, OpEthApiBuilder<NetworkT>>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
            Storage = OpStorage,
            Payload = OpEngineTypes,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    >,
    OpEthApiError: FromEvmError<N::Evm>,
    <N::Pool as TransactionPool>::Transaction: OpPooledTx,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = op_revm::OpTransaction<TxEnv>>,
    OpEthApi<N, NetworkT>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
    NetworkT: op_alloy_network::Network + Unpin,
{
    type Handle = RpcHandle<N, OpEthApi<N, NetworkT>>;

    async fn launch_add_ons(
        self,
        ctx: reth_node_api::AddOnsContext<'_, N>,
    ) -> eyre::Result<Self::Handle> {
        let Self {
            rpc_add_ons,
            da_config,
            sequencer_url,
            sequencer_headers,
            enable_tx_conditional,
        } = self;

        let builder = reth_optimism_payload_builder::OpPayloadBuilder::new(
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
            ctx.node.evm_config().clone(),
        );
        // install additional OP specific rpc methods
        let debug_ext = OpDebugWitnessApi::new(
            ctx.node.provider().clone(),
            Box::new(ctx.node.task_executor().clone()),
            builder,
        );
        let miner_ext = OpMinerExtApi::new(da_config);

        let sequencer_client = if let Some(url) = sequencer_url {
            Some(SequencerClient::new_with_headers(url, sequencer_headers).await?)
        } else {
            None
        };

        let tx_conditional_ext: OpEthExtApi<N::Pool, N::Provider> = OpEthExtApi::new(
            sequencer_client,
            ctx.node.pool().clone(),
            ctx.node.provider().clone(),
        );

        rpc_add_ons
            .launch_add_ons_with(ctx, move |modules, auth_modules, registry| {
                debug!(target: "reth::cli", "Installing debug payload witness rpc endpoint");
                modules.merge_if_module_configured(RethRpcModule::Debug, debug_ext.into_rpc())?;

                // extend the miner namespace if configured in the regular http server
                modules.merge_if_module_configured(
                    RethRpcModule::Miner,
                    miner_ext.clone().into_rpc(),
                )?;

                // install the miner extension in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Miner) {
                    debug!(target: "reth::cli", "Installing miner DA rpc endpoint");
                    auth_modules.merge_auth_methods(miner_ext.into_rpc())?;
                }

                // install the debug namespace in the authenticated if configured
                if modules.module_config().contains_any(&RethRpcModule::Debug) {
                    debug!(target: "reth::cli", "Installing debug rpc endpoint");
                    auth_modules.merge_auth_methods(registry.debug_api().into_rpc())?;
                }

                if enable_tx_conditional {
                    // extend the eth namespace if configured in the regular http server
                    modules.merge_if_module_configured(
                        RethRpcModule::Eth,
                        tx_conditional_ext.into_rpc(),
                    )?;
                }

                Ok(())
            })
            .await
    }
}

impl<N, NetworkT> RethRpcAddOns<N> for OpAddOns<N, OpEthApiBuilder<NetworkT>>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
            Storage = OpStorage,
            Payload = OpEngineTypes,
        >,
        Evm: ConfigureEvm<NextBlockEnvCtx = OpNextBlockEnvAttributes>,
    >,
    OpEthApiError: FromEvmError<N::Evm>,
    <<N as FullNodeComponents>::Pool as TransactionPool>::Transaction: OpPooledTx,
    EvmFactoryFor<N::Evm>: EvmFactory<Tx = op_revm::OpTransaction<TxEnv>>,
    OpEthApi<N, NetworkT>: FullEthApiServer<Provider = N::Provider, Pool = N::Pool>,
    NetworkT: op_alloy_network::Network + Unpin,
{
    type EthApi = OpEthApi<N, NetworkT>;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.rpc_add_ons.hooks_mut()
    }
}

impl<N, NetworkT> EngineValidatorAddOn<N> for OpAddOns<N, OpEthApiBuilder<NetworkT>>
where
    N: FullNodeComponents<
        Types: NodeTypes<
            ChainSpec = OpChainSpec,
            Primitives = OpPrimitives,
            Payload = OpEngineTypes,
        >,
    >,
    OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
{
    type Validator = OpEngineValidator<
        N::Provider,
        <<N::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx,
        <N::Types as NodeTypes>::ChainSpec,
    >;

    async fn engine_validator(&self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::Validator> {
        OpEngineValidatorBuilder::default().build(ctx).await
    }
}

/// A regular optimism evm and executor builder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OpAddOnsBuilder<NetworkT> {
    /// Sequencer client, configured to forward submitted transactions to sequencer of given OP
    /// network.
    sequencer_url: Option<String>,
    /// Headers to use for the sequencer client requests.
    sequencer_headers: Vec<String>,
    /// Data availability configuration for the OP builder.
    da_config: Option<OpDAConfig>,
    /// Enable transaction conditionals.
    enable_tx_conditional: bool,
    /// Marker for network types.
    _nt: PhantomData<NetworkT>,
}

impl<NetworkT> Default for OpAddOnsBuilder<NetworkT> {
    fn default() -> Self {
        Self {
            sequencer_url: None,
            sequencer_headers: Vec::new(),
            da_config: None,
            enable_tx_conditional: false,
            _nt: PhantomData,
        }
    }
}

impl<NetworkT> OpAddOnsBuilder<NetworkT> {
    /// With a [`SequencerClient`].
    pub fn with_sequencer(mut self, sequencer_client: Option<String>) -> Self {
        self.sequencer_url = sequencer_client;
        self
    }

    /// With headers to use for the sequencer client requests.
    pub fn with_sequencer_headers(mut self, sequencer_headers: Vec<String>) -> Self {
        self.sequencer_headers = sequencer_headers;
        self
    }

    /// Configure the data availability configuration for the OP builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = Some(da_config);
        self
    }

    /// Configure if transaction conditional should be enabled.
    pub const fn with_enable_tx_conditional(mut self, enable_tx_conditional: bool) -> Self {
        self.enable_tx_conditional = enable_tx_conditional;
        self
    }
}

impl<NetworkT> OpAddOnsBuilder<NetworkT> {
    /// Builds an instance of [`OpAddOns`].
    pub fn build<N>(self) -> OpAddOns<N, OpEthApiBuilder<NetworkT>>
    where
        N: FullNodeComponents<Types: NodeTypes>,
        OpEthApiBuilder<NetworkT>: EthApiBuilder<N>,
    {
        let Self { sequencer_url, sequencer_headers, da_config, enable_tx_conditional, .. } = self;

        OpAddOns {
            rpc_add_ons: RpcAddOns::new(
                OpEthApiBuilder::default()
                    .with_sequencer(sequencer_url.clone())
                    .with_sequencer_headers(sequencer_headers.clone()),
                OpEngineValidatorBuilder::default(),
                OpEngineApiBuilder::default(),
            ),
            da_config: da_config.unwrap_or_default(),
            sequencer_url,
            sequencer_headers,
            enable_tx_conditional,
        }
    }
}

/// A regular optimism evm and executor builder.
#[derive(Debug, Copy)]
#[non_exhaustive]
pub struct OpExecutorBuilder<ChainSpec = OpChainSpec, Primitives = OpPrimitives> {
    /// Marker for chain spec type.
    _cs: PhantomData<ChainSpec>,
    /// Marker for primitives type.
    _p: PhantomData<Primitives>,
}

impl<ChainSpec, Primitives> Clone for OpExecutorBuilder<ChainSpec, Primitives> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<ChainSpec, Primitives> Default for OpExecutorBuilder<ChainSpec, Primitives> {
    fn default() -> Self {
        Self { _cs: PhantomData, _p: PhantomData }
    }
}

impl<Node, ChainSpec, Primitives> ExecutorBuilder<Node> for OpExecutorBuilder<ChainSpec, Primitives>
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec = ChainSpec, Primitives = Primitives>>,
    ChainSpec: EthChainSpec + OpHardforks,
    Primitives: NodePrimitives,
    OpEvmConfig<ChainSpec, Primitives>: ConfigureEvm<Primitives = Primitives> + 'static,
{
    type EVM = OpEvmConfig<ChainSpec, Primitives>;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = OpEvmConfig::new(ctx.chain_spec(), OpRethReceiptBuilder::default());

        Ok(evm_config)
    }
}

/// A basic optimism transaction pool.
///
/// This contains various settings that can be configured and take precedence over the node's
/// config.
#[derive(Debug)]
pub struct OpPoolBuilder<T = crate::txpool::OpPooledTransaction> {
    /// Enforced overrides that are applied to the pool config.
    pub pool_config_overrides: PoolBuilderConfigOverrides,
    /// Enable transaction conditionals.
    pub enable_tx_conditional: bool,
    /// Supervisor client url
    pub supervisor_http: String,
    /// Supervisor safety level
    pub supervisor_safety_level: SafetyLevel,
    /// Marker for the pooled transaction type.
    _pd: core::marker::PhantomData<T>,
}

impl<T> Default for OpPoolBuilder<T> {
    fn default() -> Self {
        Self {
            pool_config_overrides: Default::default(),
            enable_tx_conditional: false,
            supervisor_http: DEFAULT_SUPERVISOR_URL.to_string(),
            supervisor_safety_level: SafetyLevel::CrossUnsafe,
            _pd: Default::default(),
        }
    }
}

impl<T> Clone for OpPoolBuilder<T> {
    fn clone(&self) -> Self {
        Self {
            pool_config_overrides: self.pool_config_overrides.clone(),
            enable_tx_conditional: self.enable_tx_conditional,
            supervisor_http: self.supervisor_http.clone(),
            supervisor_safety_level: self.supervisor_safety_level,
            _pd: core::marker::PhantomData,
        }
    }
}

impl<T> OpPoolBuilder<T> {
    /// Sets the `enable_tx_conditional` flag on the pool builder.
    pub const fn with_enable_tx_conditional(mut self, enable_tx_conditional: bool) -> Self {
        self.enable_tx_conditional = enable_tx_conditional;
        self
    }

    /// Sets the [`PoolBuilderConfigOverrides`] on the pool builder.
    pub fn with_pool_config_overrides(
        mut self,
        pool_config_overrides: PoolBuilderConfigOverrides,
    ) -> Self {
        self.pool_config_overrides = pool_config_overrides;
        self
    }

    /// Sets the supervisor client
    pub fn with_supervisor(
        mut self,
        supervisor_client: String,
        supervisor_safety_level: SafetyLevel,
    ) -> Self {
        self.supervisor_http = supervisor_client;
        self.supervisor_safety_level = supervisor_safety_level;
        self
    }
}

impl<Node, T> PoolBuilder<Node> for OpPoolBuilder<T>
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: OpHardforks>>,
    T: EthPoolTransaction<Consensus = TxTy<Node::Types>> + OpPooledTx,
{
    type Pool = OpTransactionPool<Node::Provider, DiskFileBlobStore, T>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let Self { pool_config_overrides, .. } = self;
        let data_dir = ctx.config().datadir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore(), Default::default())?;
        // supervisor used for interop
        if ctx.chain_spec().is_interop_active_at_timestamp(ctx.head().timestamp) &&
            self.supervisor_http == DEFAULT_SUPERVISOR_URL
        {
            info!(target: "reth::cli",
                url=%DEFAULT_SUPERVISOR_URL,
                "Default supervisor url is used, consider changing --rollup.supervisor-http."
            );
        }
        let supervisor_client = SupervisorClient::builder(self.supervisor_http.clone())
            .minimum_safety(self.supervisor_safety_level)
            .build()
            .await;

        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .no_eip4844()
            .with_head_timestamp(ctx.head().timestamp)
            .kzg_settings(ctx.kzg_settings()?)
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_additional_tasks(
                pool_config_overrides
                    .additional_validation_tasks
                    .unwrap_or_else(|| ctx.config().txpool.additional_validation_tasks),
            )
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone())
            .map(|validator| {
                OpTransactionValidator::new(validator)
                    // In --dev mode we can't require gas fees because we're unable to decode
                    // the L1 block info
                    .require_l1_data_gas_fee(!ctx.config().dev.dev)
                    .with_supervisor(supervisor_client.clone())
            });

        let transaction_pool = reth_transaction_pool::Pool::new(
            validator,
            CoinbaseTipOrdering::default(),
            blob_store,
            pool_config_overrides.apply(ctx.pool_config()),
        );
        info!(target: "reth::cli", "Transaction pool initialized");

        // spawn txpool maintenance tasks
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            if !ctx.config().txpool.disable_transactions_backup {
                // Use configured backup path or default to data dir
                let transactions_path = ctx
                    .config()
                    .txpool
                    .transactions_backup_path
                    .clone()
                    .unwrap_or_else(|| data_dir.txpool_transactions());

                let transactions_backup_config =
                    reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

                ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
                    "local transactions backup task",
                    |shutdown| {
                        reth_transaction_pool::maintain::backup_local_transactions_task(
                            shutdown,
                            pool.clone(),
                            transactions_backup_config,
                        )
                    },
                );
            }

            // spawn the main maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool.clone(),
                    chain_events,
                    ctx.task_executor().clone(),
                    reth_transaction_pool::maintain::MaintainPoolConfig {
                        max_tx_lifetime: pool.config().max_queued_lifetime,
                        no_local_exemptions: transaction_pool
                            .config()
                            .local_transactions_config
                            .no_exemptions,
                        ..Default::default()
                    },
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");

            // spawn the Op txpool maintenance task
            let chain_events = ctx.provider().canonical_state_stream();
            ctx.task_executor().spawn_critical(
                "Op txpool interop maintenance task",
                reth_optimism_txpool::maintain::maintain_transaction_pool_interop_future(
                    pool.clone(),
                    chain_events,
                    supervisor_client,
                ),
            );
            debug!(target: "reth::cli", "Spawned Op interop txpool maintenance task");

            if self.enable_tx_conditional {
                // spawn the Op txpool maintenance task
                let chain_events = ctx.provider().canonical_state_stream();
                ctx.task_executor().spawn_critical(
                    "Op txpool conditional maintenance task",
                    reth_optimism_txpool::maintain::maintain_transaction_pool_conditional_future(
                        pool,
                        chain_events,
                    ),
                );
                debug!(target: "reth::cli", "Spawned Op conditional txpool maintenance task");
            }
        }

        Ok(transaction_pool)
    }
}

/// A basic optimism payload service builder
#[derive(Debug, Default, Clone)]
pub struct OpPayloadBuilder<Txs = ()> {
    /// By default the pending block equals the latest block
    /// to save resources and not leak txs from the tx-pool,
    /// this flag enables computing of the pending block
    /// from the tx-pool instead.
    ///
    /// If `compute_pending_block` is not enabled, the payload builder
    /// will use the payload attributes from the latest block. Note
    /// that this flag is not yet functional.
    pub compute_pending_block: bool,
    /// The type responsible for yielding the best transactions for the payload if mempool
    /// transactions are allowed.
    pub best_transactions: Txs,
    /// This data availability configuration specifies constraints for the payload builder
    /// when assembling payloads
    pub da_config: OpDAConfig,
}

impl OpPayloadBuilder {
    /// Create a new instance with the given `compute_pending_block` flag and data availability
    /// config.
    pub fn new(compute_pending_block: bool) -> Self {
        Self { compute_pending_block, best_transactions: (), da_config: OpDAConfig::default() }
    }

    /// Configure the data availability configuration for the OP payload builder.
    pub fn with_da_config(mut self, da_config: OpDAConfig) -> Self {
        self.da_config = da_config;
        self
    }
}

impl<Txs> OpPayloadBuilder<Txs> {
    /// Configures the type responsible for yielding the transactions that should be included in the
    /// payload.
    pub fn with_transactions<T>(self, best_transactions: T) -> OpPayloadBuilder<T> {
        let Self { compute_pending_block, da_config, .. } = self;
        OpPayloadBuilder { compute_pending_block, best_transactions, da_config }
    }
}

impl<Node, Pool, Txs, Evm> PayloadBuilderBuilder<Node, Pool, Evm> for OpPayloadBuilder<Txs>
where
    Node: FullNodeTypes<
        Provider: ChainSpecProvider<ChainSpec: OpHardforks>,
        Types: NodeTypes<
            Primitives: OpPayloadPrimitives,
            Payload: PayloadTypes<
                BuiltPayload = OpBuiltPayload<PrimitivesTy<Node::Types>>,
                PayloadAttributes = OpPayloadAttributes,
                PayloadBuilderAttributes = OpPayloadBuilderAttributes<TxTy<Node::Types>>,
            >,
        >,
    >,
    Evm: ConfigureEvm<
            Primitives = PrimitivesTy<Node::Types>,
            NextBlockEnvCtx = OpNextBlockEnvAttributes,
        > + 'static,
    Pool: TransactionPool<Transaction: OpPooledTx<Consensus = TxTy<Node::Types>>> + Unpin + 'static,
    Txs: OpPayloadTransactions<Pool::Transaction>,
{
    type PayloadBuilder =
        reth_optimism_payload_builder::OpPayloadBuilder<Pool, Node::Provider, Evm, Txs>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
        evm_config: Evm,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let payload_builder = reth_optimism_payload_builder::OpPayloadBuilder::with_builder_config(
            pool,
            ctx.provider().clone(),
            evm_config,
            OpBuilderConfig { da_config: self.da_config.clone() },
        )
        .with_transactions(self.best_transactions.clone())
        .set_compute_pending_block(self.compute_pending_block);
        Ok(payload_builder)
    }
}

/// A basic optimism network builder.
#[derive(Debug, Default)]
pub struct OpNetworkBuilder<NetworkP = OpNetworkPrimitives, PooledTx = OpPooledTransaction> {
    /// Disable transaction pool gossip
    pub disable_txpool_gossip: bool,
    /// Disable discovery v4
    pub disable_discovery_v4: bool,
    /// Marker for the network primitives type
    _np: PhantomData<NetworkP>,
    /// Marker for the pooled transaction type
    _pt: PhantomData<PooledTx>,
}

impl<NetworkP, PooledTx> Clone for OpNetworkBuilder<NetworkP, PooledTx> {
    fn clone(&self) -> Self {
        Self::new(self.disable_txpool_gossip, self.disable_discovery_v4)
    }
}

impl<NetworkP, PooledTx> OpNetworkBuilder<NetworkP, PooledTx> {
    /// Creates a new `OpNetworkBuilder`.
    pub const fn new(disable_txpool_gossip: bool, disable_discovery_v4: bool) -> Self {
        Self { disable_txpool_gossip, disable_discovery_v4, _np: PhantomData, _pt: PhantomData }
    }
}

impl<NetworkP: NetworkPrimitives, PooledTx> OpNetworkBuilder<NetworkP, PooledTx> {
    /// Returns the [`NetworkConfig`] that contains the settings to launch the p2p network.
    ///
    /// This applies the configured [`OpNetworkBuilder`] settings.
    pub fn network_config<Node>(
        &self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<NetworkConfig<<Node as FullNodeTypes>::Provider, NetworkP>>
    where
        Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    {
        let Self { disable_txpool_gossip, disable_discovery_v4, .. } = self.clone();
        let args = &ctx.config().network;
        let network_builder = ctx
            .network_config_builder()?
            // apply discovery settings
            .apply(|mut builder| {
                let rlpx_socket = (args.addr, args.port).into();
                if disable_discovery_v4 || args.discovery.disable_discovery {
                    builder = builder.disable_discv4_discovery();
                }
                if !args.discovery.disable_discovery {
                    builder = builder.discovery_v5(
                        args.discovery.discovery_v5_builder(
                            rlpx_socket,
                            ctx.config()
                                .network
                                .resolved_bootnodes()
                                .or_else(|| ctx.chain_spec().bootnodes())
                                .unwrap_or_default(),
                        ),
                    );
                }

                builder
            });

        let mut network_config = ctx.build_network_config(network_builder);

        // When `sequencer_endpoint` is configured, the node will forward all transactions to a
        // Sequencer node for execution and inclusion on L1, and disable its own txpool
        // gossip to prevent other parties in the network from learning about them.
        network_config.tx_gossip_disabled = disable_txpool_gossip;

        Ok(network_config)
    }
}

impl<Node, Pool, NetworkP, PooledTx> NetworkBuilder<Node, Pool>
    for OpNetworkBuilder<NetworkP, PooledTx>
where
    Node: FullNodeTypes<Types: NodeTypes<ChainSpec: Hardforks>>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<Consensus = TxTy<Node::Types>, Pooled = PooledTx>,
        > + Unpin
        + 'static,
    NetworkP: NetworkPrimitives<
            PooledTransaction = PooledTx,
            BroadcastedTransaction = <<Node::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx
        >
        + NetPrimitivesFor<PrimitivesTy<Node::Types>>,
    PooledTx: Send,
{
    type Network = NetworkHandle<NetworkP>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network_config = self.network_config(ctx)?;
        let network = NetworkManager::builder(network_config).await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");

        Ok(handle)
    }
}

/// A basic optimism consensus builder.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for OpConsensusBuilder
where
    Node: FullNodeTypes<
        Types: NodeTypes<
            ChainSpec: OpHardforks,
            Primitives: NodePrimitives<Receipt: DepositReceipt>,
        >,
    >,
{
    type Consensus = Arc<OpBeaconConsensus<<Node::Types as NodeTypes>::ChainSpec>>;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(Arc::new(OpBeaconConsensus::new(ctx.chain_spec())))
    }
}

/// Builder for [`OpEngineValidator`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OpEngineValidatorBuilder;

impl<Node, Types> EngineValidatorBuilder<Node> for OpEngineValidatorBuilder
where
    Types: NodeTypes<ChainSpec = OpChainSpec, Primitives = OpPrimitives, Payload = OpEngineTypes>,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = OpEngineValidator<
        Node::Provider,
        <<Node::Types as NodeTypes>::Primitives as NodePrimitives>::SignedTx,
        <Node::Types as NodeTypes>::ChainSpec,
    >;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(OpEngineValidator::new::<KeyHasherTy<Types>>(
            ctx.config.chain.clone(),
            ctx.node.provider().clone(),
        ))
    }
}

/// Network primitive types used by Optimism networks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct OpNetworkPrimitives;

impl NetworkPrimitives for OpNetworkPrimitives {
    type BlockHeader = alloy_consensus::Header;
    type BlockBody = alloy_consensus::BlockBody<OpTransactionSigned>;
    type Block = alloy_consensus::Block<OpTransactionSigned>;
    type BroadcastedTransaction = OpTransactionSigned;
    type PooledTransaction = OpPooledTransaction;
    type Receipt = OpReceipt;
}
