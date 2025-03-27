// This file is part of HydraDX-node.

// Copyright (C) 2020-2021  Intergalactic, Limited (GIB).
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.

#![allow(clippy::all)]

use hydradx_runtime::{
	opaque::{Block, Hash},
	RuntimeApi,
};
use std::{sync::Arc, time::Duration};

use cumulus_client_cli::CollatorOptions;
use cumulus_client_collator::service::CollatorService;
use cumulus_client_service::{prepare_node_config, start_collator, start_full_node, TFullClient, TParachainBlockImport};
use cumulus_primitives_core::{relay_chain::CollatorPair, ParaId};
use cumulus_relay_chain_interface::{OverseerHandle, RelayChainInterface};

use fc_db::kv::Backend as FrontierBackend;
use fc_rpc_core::types::{FeeHistoryCache, FilterPool};
use sc_client_api::Backend;
use sc_consensus::ImportQueue;
use sc_executor::{HeapAllocStrategy, WasmExecutor, DEFAULT_HEAP_ALLOC_STRATEGY};
use sc_network::{NetworkBlock, Event};
use sc_service::{Configuration, PartialComponents, TFullBackend, TFullClient, TaskManager};
use sc_telemetry::{Telemetry, TelemetryHandle, TelemetryWorker, TelemetryWorkerHandle};
use sc_transaction_pool_api::OffchainTransactionPoolFactory;
use sp_keystore::KeystorePtr;
use std::{collections::BTreeMap, sync::Mutex, fs::OpenOptions, io::Write};
use futures::StreamExt;
use substrate_prometheus_endpoint::Registry;
use tokio::time::sleep;

pub(crate) mod evm;
use crate::{chain_spec, rpc};

type ParachainClient = TFullClient<
	Block,
	RuntimeApi,
	WasmExecutor<(
		cumulus_client_service::ParachainHostFunctions,
		frame_benchmarking::benchmarking::HostFunctions,
	)>,
>;

type ParachainBackend = TFullBackend<Block>;

type ParachainBlockImport = TParachainBlockImport<Block, Arc<ParachainClient>, ParachainBackend>;

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
pub fn new_partial(
	config: &Configuration,
) -> Result<
	PartialComponents<
		ParachainClient,
		ParachainBackend,
		(),
		sc_consensus::DefaultImportQueue<Block>,
		sc_transaction_pool::FullPool<Block, ParachainClient>,
		(
			evm::BlockImport<Block, ParachainBlockImport, ParachainClient>,
			Option<Telemetry>,
			Option<TelemetryWorkerHandle>,
			Arc<FrontierBackend<Block, ParachainClient>>,
			FilterPool,
			FeeHistoryCache,
		),
	>,
	sc_service::Error,
> {
	let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let heap_pages = config
		.executor
		.default_heap_pages
		.map_or(DEFAULT_HEAP_ALLOC_STRATEGY, |h| HeapAllocStrategy::Static {
			extra_pages: h as _,
		});

	let executor = WasmExecutor::builder()
		.with_execution_method(config.executor.wasm_method)
		.with_onchain_heap_alloc_strategy(heap_pages)
		.with_offchain_heap_alloc_strategy(heap_pages)
		.with_max_runtime_instances(config.executor.max_runtime_instances)
		.with_runtime_cache_size(config.executor.runtime_cache_size)
		.build();

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts_record_import::<Block, RuntimeApi, _>(
			config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
			executor,
			true,
		)?;

	let client = Arc::new(client);

	let telemetry_worker_handle = telemetry.as_ref().map(|(worker, _)| worker.handle());

	let telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", None, worker.run());
		telemetry
	});

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	let frontier_backend = Arc::new(FrontierBackend::open(
		Arc::clone(&client),
		&config.database,
		&evm::db_config_dir(config),
	)?);

	let evm_since = chain_spec::Extensions::try_get(&config.chain_spec)
		.map(|e| e.evm_since)
		.unwrap_or(1);
	let block_import = evm::BlockImport::new(
		ParachainBlockImport::new(client.clone(), backend.clone()),
		client.clone(),
		frontier_backend.clone(),
		evm_since,
	);

	let import_queue = build_import_queue(
		client.clone(),
		block_import.clone(),
		config,
		telemetry.as_ref().map(|telemetry| telemetry.handle()),
		&task_manager,
	)?;

	let filter_pool: FilterPool = Arc::new(Mutex::new(BTreeMap::new()));
	let fee_history_cache: FeeHistoryCache = Arc::new(Mutex::new(BTreeMap::new()));

	Ok(PartialComponents {
		backend,
		client,
		import_queue,
		keystore_container,
		task_manager,
		transaction_pool,
		select_chain: (),
		other: (
			block_import,
			telemetry,
			telemetry_worker_handle,
			frontier_backend,
			filter_pool,
			fee_history_cache,
		),
	})
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
#[sc_tracing::logging::prefix_logs_with("Parachain")]
async fn start_node_impl(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	ethereum_config: evm::EthereumConfig,
	collator_options: CollatorOptions,
	para_id: ParaId,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient>)> {
	let parachain_config = prepare_node_config(parachain_config);

	let params = new_partial(&parachain_config)?;
	let (block_import, mut telemetry, telemetry_worker_handle, frontier_backend, filter_pool, fee_history_cache) =
		params.other;

	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let net_config = sc_network::config::FullNetworkConfiguration::<_, _, sc_network::NetworkWorker<Block, Hash>>::new(
		&parachain_config.network,
		prometheus_registry.clone(),
	);

	let client = params.client.clone();
	let backend = params.backend.clone();
	let mut task_manager = params.task_manager;

	let (relay_chain_interface, collator_key) = build_relay_chain_interface(
		polkadot_config,
		&parachain_config,
		telemetry_worker_handle,
		&mut task_manager,
		collator_options.clone(),
		None,
	)
	.await
	.map_err(|e| sc_service::Error::Application(Box::new(e) as Box<_>))?;

	let validator = parachain_config.role.is_authority();
	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let transaction_pool = params.transaction_pool.clone();
	let import_queue_service = params.import_queue.service();

	let (network, system_rpc_tx, tx_handler_controller, start_network, sync_service) =
		build_network(BuildNetworkParams {
			parachain_config: &parachain_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			para_id,
			spawn_handle: task_manager.spawn_handle(),
			relay_chain_interface: relay_chain_interface.clone(),
			import_queue: params.import_queue,
			net_config,
			sybil_resistance_level: CollatorSybilResistance::Resistant, // because of Aura
		})
		.await?;

	if parachain_config.offchain_worker.enabled {
		use futures::FutureExt;

		task_manager.spawn_handle().spawn(
			"offchain-workers-runner",
			"offchain-work",
			sc_offchain::OffchainWorkers::new(sc_offchain::OffchainWorkerOptions {
				runtime_api_provider: client.clone(),
				keystore: Some(params.keystore_container.keystore()),
				offchain_db: backend.offchain_storage(),
				transaction_pool: Some(OffchainTransactionPoolFactory::new(transaction_pool.clone())),
				network_provider: Arc::new(network.clone()),
				is_validator: parachain_config.role.is_authority(),
				enable_http_requests: false,
				custom_extensions: move |_| vec![],
			})
			.run(client.clone(), task_manager.spawn_handle())
			.boxed(),
		);
	}

	let overrides = Arc::new(crate::rpc::StorageOverrideHandler::new(client.clone()));
	let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
		task_manager.spawn_handle(),
		overrides.clone(),
		ethereum_config.eth_log_block_cache,
		ethereum_config.eth_statuses_cache,
		prometheus_registry.clone(),
	));

	// Sinks for pubsub notifications.
	// Everytime a new subscription is created, a new mpsc channel is added to the sink pool.
	// The MappingSyncWorker sends through the channel on block import and the subscription emits a
	// notification to the subscriber on receiving a message through this channel.
	// This way we avoid race conditions when using native substrate block import notification
	// stream.
	let pubsub_notification_sinks: fc_mapping_sync::EthereumBlockNotificationSinks<
		fc_mapping_sync::EthereumBlockNotification<Block>,
	> = Default::default();
	let pubsub_notification_sinks = Arc::new(pubsub_notification_sinks);

	let rpc_builder = {
		let client = client.clone();
		let is_authority = parachain_config.role.is_authority();
		let transaction_pool = transaction_pool.clone();
		let network = network.clone();
		let sync = sync_service.clone();
		let frontier_backend = frontier_backend.clone();
		let fee_history_cache = fee_history_cache.clone();
		let filter_pool = filter_pool.clone();
		let overrides = overrides.clone();
		let pubsub_notification_sinks = pubsub_notification_sinks.clone();
		let backend = backend.clone();

		Box::new(move |subscription_task_executor| {
			let deps = crate::rpc::FullDeps {
				client: client.clone(),
				pool: transaction_pool.clone(),
				backend: backend.clone(),
			};
			let module = rpc::create_full(deps)?;
			let eth_deps = rpc::Deps {
				client: client.clone(),
				pool: transaction_pool.clone(),
				graph: transaction_pool.pool().clone(),
				converter: Some(hydradx_runtime::TransactionConverter),
				is_authority,
				enable_dev_signer: ethereum_config.enable_dev_signer,
				network: network.clone(),
				sync: sync.clone(),
				frontier_backend: frontier_backend.clone(),
				overrides: overrides.clone(),
				block_data_cache: block_data_cache.clone(),
				filter_pool: filter_pool.clone(),
				max_past_logs: ethereum_config.max_past_logs,
				fee_history_cache: fee_history_cache.clone(),
				fee_history_cache_limit: ethereum_config.fee_history_limit,
				execute_gas_limit_multiplier: ethereum_config.execute_gas_limit_multiplier,
			};
			rpc::create(
				module,
				eth_deps,
				subscription_task_executor,
				pubsub_notification_sinks.clone(),
			)
			.map_err(Into::into)
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		rpc_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: params.keystore_container.keystore(),
		backend: backend.clone(),
		network: network.clone(),
		sync_service: sync_service.clone(),
		system_rpc_tx,
		tx_handler_controller,
		telemetry: telemetry.as_mut(),
	})?;

	evm::spawn_frontier_tasks(
		&task_manager,
		client.clone(),
		backend.clone(),
		frontier_backend.clone(),
		filter_pool.clone(),
		overrides,
		fee_history_cache.clone(),
		ethereum_config.fee_history_limit,
		sync_service.clone(),
		pubsub_notification_sinks,
	);

	// ---------------------------------------
	// ---------------------------------------
	// ---------------------------------------
	// Open a file for writing peer info logs.
	let peer_log_file = std::sync::Arc::new(Mutex::new(OpenOptions::new()
		.create(true)
		.append(true)
		.open("peer_info.log")
		.expect("Unable to open peer_info.log")));

	{
		// Clone network handle for logging peer events.
		let network_clone = network.clone();
		let peer_log_file_clone = peer_log_file.clone();
		let mut peer_events = network_clone.event_stream("peer-logging");
		task_manager.spawn_handle().spawn(
			"peer-ip-logger",
			None,
			async move {
				while let Some(event) = peer_events.next().await {
					match event {
						Event::PeerConnected(peer_id) => {
							let peer_id_str = peer_id.to_base58();
							if let Ok(net_state) = network_clone.network_state().await {
								if let Some(peer) = net_state.connected_peers.get(&peer_id_str) {
									let addresses: Vec<String> = peer.addresses.iter().map(|a| a.to_string()).collect();
									let client_version = peer.version_string.as_deref().unwrap_or("unknown");
									let log_line = format!("🌐 Peer connected: id={} addresses={:?} client={}\n", peer_id, addresses, client_version);
									let mut file = peer_log_file_clone.lock().unwrap();
									file.write_all(log_line.as_bytes()).unwrap();
								} else {
									let log_line = format!("🌐 Peer connected: id={}\n", peer_id);
									let mut file = peer_log_file_clone.lock().unwrap();
									file.write_all(log_line.as_bytes()).unwrap();
								}
							}
						}
						Event::PeerDisconnected(peer_id) => {
							let log_line = format!("Peer disconnected: id={}\n", peer_id);
							let mut file = peer_log_file_clone.lock().unwrap();
							file.write_all(log_line.as_bytes()).unwrap();
						}
						Event::NotificationStreamOpened { protocol, remote, info } => {
							let log_line = format!("Protocol stream opened with {} (protocol: {}) – info: {:?}\n", remote, protocol, info);
							let mut file = peer_log_file_clone.lock().unwrap();
							file.write_all(log_line.as_bytes()).unwrap();
						}
						Event::Dht(dht_event) => {
							let log_line = format!("DHT event: {:?}\n", dht_event);
							let mut file = peer_log_file_clone.lock().unwrap();
							file.write_all(log_line.as_bytes()).unwrap();
						}
						_ => {}
					}
				}
			}
		);

		// Periodically log discovered peers that are not connected.
		let network_clone2 = network.clone();
		let peer_log_file_clone2 = peer_log_file.clone();
		task_manager.spawn_handle().spawn(
			"peer-discovery-logger",
			None,
			async move {
				loop {
					if let Ok(net_state) = network_clone2.network_state().await {
						for (peer_id, peer_info) in net_state.not_connected_peers.iter() {
							let addrs: Vec<String> = peer_info.addresses.iter().map(|a| a.to_string()).collect();
							if !addrs.is_empty() {
								let log_line = format!("🔍 Discovered peer (not connected): id={} addresses={:?}\n", peer_id, addrs);
								let mut file = peer_log_file_clone2.lock().unwrap();
								file.write_all(log_line.as_bytes()).unwrap();
							}
						}
					}
					sleep(Duration::from_secs(60)).await;
				}
			}
		);
	}
	// ---------------------------------------
	// ---------------------------------------
	// ---------------------------------------

	let announce_block = {
		let sync_service = sync_service.clone();
		Arc::new(move |hash, data| sync_service.announce_block(hash, data))
	};

	let relay_chain_slot_duration = Duration::from_secs(6);

	let overseer_handle = relay_chain_interface
		.overseer_handle()
		.map_err(|e| sc_service::Error::Application(Box::new(e)))?;

	start_relay_chain_tasks(StartRelayChainTasksParams {
		client: client.clone(),
		announce_block: announce_block.clone(),
		para_id,
		relay_chain_interface: relay_chain_interface.clone(),
		task_manager: &mut task_manager,
		da_recovery_profile: if validator {
			DARecoveryProfile::Collator
		} else {
			DARecoveryProfile::FullNode
		},
		import_queue: import_queue_service,
		relay_chain_slot_duration,
		recovery_handle: Box::new(overseer_handle.clone()),
		sync_service: sync_service.clone(),
	})?;

	if validator {
		start_consensus(
			client.clone(),
			block_import,
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|t| t.handle()),
			&task_manager,
			relay_chain_interface.clone(),
			transaction_pool,
			params.keystore_container.keystore(),
			relay_chain_slot_duration,
			para_id,
			collator_key.expect("Command line arguments do not allow this. qed"),
			overseer_handle,
			announce_block,
		)?;
	}

	start_network.start_network();

	Ok((task_manager, client))
}

/// Build the import queue for the parachain runtime.
fn build_import_queue(
	client: Arc<ParachainClient>,
	block_import: evm::BlockImport<Block, ParachainBlockImport, ParachainClient>,
	config: &Configuration,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error> {
	Ok(
		cumulus_client_consensus_aura::equivocation_import_queue::fully_verifying_import_queue::<
			sp_consensus_aura::sr25519::AuthorityPair,
			_,
			_,
			_,
			_,
		>(
			client,
			block_import,
			move |_, _| async move {
				let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
				Ok(timestamp)
			},
			&task_manager.spawn_essential_handle(),
			config.prometheus_registry(),
			telemetry,
		),
	)
}

fn start_consensus(
	client: Arc<ParachainClient>,
	block_import: evm::BlockImport<Block, ParachainBlockImport, ParachainClient>,
	prometheus_registry: Option<&Registry>,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
	relay_chain_interface: Arc<dyn RelayChainInterface>,
	transaction_pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient>>,
	keystore: KeystorePtr,
	relay_chain_slot_duration: Duration,
	para_id: ParaId,
	collator_key: CollatorPair,
	overseer_handle: OverseerHandle,
	announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
) -> Result<(), sc_service::Error> {
	use cumulus_client_consensus_aura::collators::basic::{self as basic_aura, Params as BasicAuraParams};

	// NOTE: because we use Aura here explicitly, we can use `CollatorSybilResistance::Resistant`
	// when starting the network.

	let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
		task_manager.spawn_handle(),
		client.clone(),
		transaction_pool,
		prometheus_registry,
		telemetry.clone(),
	);

	let proposer = Proposer::new(proposer_factory);

	let collator_service = CollatorService::new(
		client.clone(),
		Arc::new(task_manager.spawn_handle()),
		announce_block,
		client.clone(),
	);

	let params = BasicAuraParams {
		create_inherent_data_providers: move |_, ()| async move { Ok(()) },
		block_import,
		para_client: client,
		relay_client: relay_chain_interface,
		keystore,
		overseer_handle,
		collator_key,
		para_id,
		proposer,
		collator_service,
		// Very limited proposal time.
		authoring_duration: Duration::from_millis(500),
		// The instant seals block authorship has no significant transactions
		// waiting time.
		slot_duration: Duration::from_secs(6),
	};

	start_collator(params).await.map_err(Into::into)
}

/// Build the relay chain interface for interacting with the relay chain.
async fn build_relay_chain_interface(
	polkadot_config: Configuration,
	parachain_config: &Configuration,
	telemetry_worker_handle: Option<TelemetryWorkerHandle>,
	task_manager: &mut TaskManager,
	collator_options: CollatorOptions,
	instant_seal: Option<Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>>,
) -> Result<(Arc<dyn RelayChainInterface>, CollatorPair), polkadot_service::Error> {
	use polkadot_service::PolkadotFullConfiguration;
	use rococo_service::RelayChainNode;

	let polkadot_config = PolkadotFullConfiguration {
		network: polkadot_config.network,
		keystore: polkadot_config.keystore,
		database: polkadot_config.database,
		informant_output_format: polkadot_config.informant_output_format,
		telemetry: polkadot_config.telemetry,
		prometheus_config: polkadot_config.prometheus_config,
		telemetry_endpoints: polkadot_config.telemetry_endpoints,
		tracing_targets: Default::default(),
		tracing_receiver: Default::default(),
		chain_spec: polkadot_config.chain_spec,
		role: polkadot_config.role,
		base_path: polkadot_config.base_path,
		offchain_worker: polkadot_config.offchain_worker,
		state_cache_size: polkadot_config.state_cache_size,
		state_cache_child_ratio: polkadot_config.state_cache_child_ratio,
		role_full: polkadot_config.role_full,
	};

	let collator_key = collator_options
		.keystore
		.read()
		.key::<polkadot_primitives::v2::CollatorId>(&collator_pair::ID, &[])
		.expect("Collator key not found in keystore");

	let (relay_chain_interface, collator_key) = start_full_node(
		polkadot_config,
		parachain_config,
		telemetry_worker_handle,
		task_manager,
		collator_options.relay_chain_rpc_url.as_ref().map(String::as_str),
		instant_seal,
	)
	.await?;

	Ok((relay_chain_interface, collator_key))
}

/// Creates a network configuration from the given service configuration.
#[allow(clippy::too_many_arguments)]
struct BuildNetworkParams<'a, P: sc_transaction_pool::ChainApi> {
	parachain_config: &'a Configuration,
	client: Arc<ParachainClient>,
	transaction_pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient>>,
	para_id: ParaId,
	spawn_handle: sc_service::SpawnTaskHandle,
	relay_chain_interface: Arc<dyn RelayChainInterface>,
	import_queue: sc_consensus::DefaultImportQueue<Block>,
	net_config: sc_network::config::FullNetworkConfiguration<Block, Hash, sc_network::NetworkWorker<Block, Hash>>,
	sybil_resistance_level: CollatorSybilResistance,
}

/// Create a polkadot service for the network.
async fn build_network<P: sc_transaction_pool::ChainApi>(
	params: BuildNetworkParams<'_, P>,
) -> sc_service::error::Result<(
	Arc<sc_service::NetworkStarter>,
	sc_service::SpawnTasksHandle,
	Arc<sc_network::NetworkService<Block, Hash>>,
	sc_service::SpawnTasksResult<Arc<sc_network::NetworkService<Block, Hash>>>,
	sc_network::NetworkStarter,
	sc_service::ArcDataSyncService<Block>,
)> {
	let parachain_config = params.parachain_config;
	let network_params = sc_service::BuildNetworkParams {
		config: parachain_config,
		client: params.client,
		transaction_pool: params.transaction_pool,
		spawn_handle: params.spawn_handle,
		import_queue: params.import_queue,
		on_demand: None,
		block_announce_validator_builder: None,
		enable_metrics: parachain_config.prometheus_registry().is_some(),
		warp_sync: None,
		sybil_resistance_level: params.sybil_resistance_level,
	};
	let network_service = sc_service::build_network(network_params).await?;
	let network = network_service.network.clone();
	let sync_service = network_service.sync.clone();
	let on_demand = network_service.on_demand;
	Ok((
		Arc::new(network_service.network_starter),
		network_service.spawn_tasks_handle,
		network_service.network,
		network_service.spawn_tasks_result,
		network_service.network_starter,
		network_service.data_sync_service,
	))
}

/// The chain specification for the parachain node.
pub fn chain_spec() -> crate::chain_spec::ChainSpec {
	crate::chain_spec::development_config().expect("Failed to load chain spec")
}

/// The extended network identity for a node.
pub fn extended_network_identity(
	network: &sc_network::NetworkService<Block, Hash>,
) -> sc_network::NetworkPeerInfo {
	network.identity()
}

/// The network identity for a node.
pub fn network_identity(network: &sc_network::NetworkService<Block, Hash>) -> sc_network::NetworkPeerInfo {
	let identity = network.identity();
	identity
}

/// The node identity as a JSON string.
pub fn node_identity_json(network: &sc_network::NetworkService<Block, Hash>) -> String {
	serde_json::to_string(&network.identity()).expect("Failed to serialize node identity to JSON")
}

/// `ServiceFactory` implementation.
pub struct ServiceFactory<E>(std::marker::PhantomData<E>);

impl<E> Default for ServiceFactory<E> {
	fn default() -> Self {
		ServiceFactory(std::marker::PhantomData)
	}
}

impl<E> sc_service::ServiceFactory<Block, hydradx_runtime::RuntimeApi, E> for ServiceFactory<E>
where
	E: sc_executor::NativeExecutionDispatch + 'static,
{
	fn new_service(
		&self,
		config: Configuration,
	) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient>)> {
		futures::executor::block_on(start_node_impl(
			config,
			Default::default(),
			evm::EthereumConfig::default(),
			CollatorOptions::default(),
			2000_u32.into(),
		))
	}
}
