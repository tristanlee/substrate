use futures::prelude::*;
use num_traits::AsPrimitive;
use sc_cli::SubstrateCli;
use sc_client_api::call_executor::ExecutorProvider;
use sc_consensus_babe::SlotProportion;
use sc_executor::NativeExecutionDispatch;
use sc_network::Event;
use sc_service::error::Error as ServiceError;
use sc_telemetry::TelemetryWorker;
use sp_api::ConstructRuntimeApi;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT};
use sp_transaction_pool::runtime_api::TaggedTransactionQueue;
use std::{str::FromStr, sync::Arc};

type StateBackend<Block> =
	sc_client_db::SyncingCachingState<sc_client_db::RefTrackingState<Block>, Block>;

fn new_full<Block, RuntimeApi, Executor>(
	mut config: sc_service::Configuration,
) -> Result<sc_service::TaskManager, ServiceError>
where
	Block: BlockT + std::marker::Unpin,
	<Block as BlockT>::Hash: FromStr,
	<<Block as BlockT>::Header as HeaderT>::Number: AsPrimitive<usize>,
	Executor: NativeExecutionDispatch + 'static,
	RuntimeApi: ConstructRuntimeApi<Block, sc_service::TFullClient<Block, RuntimeApi, Executor>>
		+ Send
		+ Sync
		+ 'static,
	<RuntimeApi as ConstructRuntimeApi<
		Block,
		sc_service::TFullClient<Block, RuntimeApi, Executor>,
	>>::RuntimeApi: TaggedTransactionQueue<Block>
		+ sp_consensus_babe::BabeApi<Block>
		+ sp_block_builder::BlockBuilder<Block>
		+ sp_api::ApiExt<Block, StateBackend = StateBackend<Block>>
		+ sc_finality_grandpa::GrandpaApi<Block>
		+ sp_offchain::OffchainWorkerApi<Block>
		+ sp_api::Metadata<Block>
		+ sp_session::SessionKeys<Block>
		+ sp_authority_discovery::AuthorityDiscoveryApi<Block>,
{
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

	let (client, backend, keystore_container, mut task_manager) =
		sc_service::new_full_parts::<Block, RuntimeApi, Executor>(
			&config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
		)?;
	let client = Arc::new(client);

	let mut telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", worker.run());
		telemetry
	});

	let select_chain = sc_consensus::LongestChain::new(backend.clone());

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
		client.clone(),
		&(client.clone() as Arc<_>),
		select_chain.clone(),
		telemetry.as_ref().map(|x| x.handle()),
	)?;
	let justification_import = grandpa_block_import.clone();

	let (block_import, babe_link) = sc_consensus_babe::block_import(
		sc_consensus_babe::Config::get_or_compute(&*client)?,
		grandpa_block_import,
		client.clone(),
	)?;

	let slot_duration = babe_link.config().slot_duration();
	let import_queue = sc_consensus_babe::import_queue(
		babe_link.clone(),
		block_import.clone(),
		Some(Box::new(justification_import)),
		client.clone(),
		select_chain.clone(),
		move |_, ()| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_duration(
					*timestamp,
					slot_duration,
				);

			let uncles =
				sp_authorship::InherentDataProvider::<<Block as BlockT>::Header>::check_inherents();

			Ok((timestamp, slot, uncles))
		},
		&task_manager.spawn_essential_handle(),
		config.prometheus_registry(),
		sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
		telemetry.as_ref().map(|x| x.handle()),
	)?;

	let auth_disc_publish_non_global_ips = config.network.allow_non_globals_in_dht;

	config.network.extra_sets.push(sc_finality_grandpa::grandpa_peers_set_config());
	let warp_sync = Arc::new(sc_finality_grandpa::warp_proof::NetworkProvider::new(
		backend.clone(),
		grandpa_link.shared_authority_set().clone(),
	));

	let (network, system_rpc_tx, network_starter) =
		sc_service::build_network(sc_service::BuildNetworkParams {
			config: &config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			spawn_handle: task_manager.spawn_handle(),
			import_queue,
			on_demand: None,
			block_announce_validator_builder: None,
			warp_sync: Some(warp_sync),
		})?;

	if config.offchain_worker.enabled {
		sc_service::build_offchain_workers(
			&config,
			task_manager.spawn_handle(),
			client.clone(),
			network.clone(),
		);
	}

	let role = config.role.clone();
	let force_authoring = config.force_authoring;
	let backoff_authoring_blocks =
		Some(sc_consensus_slots::BackoffAuthoringOnFinalizedHeadLagging::default());
	let name = config.network.node_name.clone();
	let enable_grandpa = !config.disable_grandpa;
	let prometheus_registry = config.prometheus_registry().cloned();

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		config,
		backend: backend.clone(),
		client: client.clone(),
		keystore: keystore_container.sync_keystore(),
		network: network.clone(),
		rpc_extensions_builder: Box::new(|_, _| Ok(())),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		on_demand: None,
		remote_blockchain: None,
		system_rpc_tx,
		telemetry: telemetry.as_mut(),
	})?;

	if let sc_service::config::Role::Authority { .. } = &role {
		let proposer = sc_basic_authorship::ProposerFactory::new(
			task_manager.spawn_handle(),
			client.clone(),
			transaction_pool.clone(),
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|x| x.handle()),
		);

		let can_author_with =
			sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

		let client_clone = client.clone();
		let slot_duration = babe_link.config().slot_duration();
		let babe_config = sc_consensus_babe::BabeParams {
			keystore: keystore_container.sync_keystore(),
			client: client.clone(),
			select_chain,
			env: proposer,
			block_import,
			sync_oracle: network.clone(),
			justification_sync_link: network.clone(),
			create_inherent_data_providers: move |parent, ()| {
				let client_clone = client_clone.clone();
				async move {
					let uncles = sc_consensus_uncles::create_uncles_inherent_data_provider(
						&*client_clone,
						parent,
					)?;

					let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

					let slot =
						sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_duration(
							*timestamp,
							slot_duration,
						);

					let storage_proof =
						sp_transaction_storage_proof::registration::new_data_provider(
							&*client_clone,
							&parent,
						)?;

					Ok((timestamp, slot, uncles, storage_proof))
				}
			},
			force_authoring,
			backoff_authoring_blocks,
			babe_link,
			can_author_with,
			block_proposal_slot_portion: SlotProportion::new(0.5),
			max_block_proposal_slot_portion: None,
			telemetry: telemetry.as_ref().map(|x| x.handle()),
		};

		let babe = sc_consensus_babe::start_babe(babe_config)?;
		task_manager.spawn_essential_handle().spawn_blocking("babe-proposer", babe);
	}

	// Spawn authority discovery module.
	if role.is_authority() {
		let authority_discovery_role =
			sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore());
		let dht_event_stream =
			network.event_stream("authority-discovery").filter_map(|e| async move {
				match e {
					Event::Dht(e) => Some(e),
					_ => None,
				}
			});
		let (authority_discovery_worker, _service) =
			sc_authority_discovery::new_worker_and_service_with_config(
				sc_authority_discovery::WorkerConfig {
					publish_non_global_ips: auth_disc_publish_non_global_ips,
					..Default::default()
				},
				client.clone(),
				network.clone(),
				Box::pin(dht_event_stream),
				authority_discovery_role,
				prometheus_registry.clone(),
			);

		task_manager
			.spawn_handle()
			.spawn("authority-discovery-worker", authority_discovery_worker.run());
	}

	// if the node isn't actively participating in consensus then it doesn't
	// need a keystore, regardless of which protocol we use below.
	let keystore =
		if role.is_authority() { Some(keystore_container.sync_keystore()) } else { None };

	if enable_grandpa {
		let config = sc_finality_grandpa::Config {
			// FIXME #1578 make this available through chainspec
			gossip_duration: std::time::Duration::from_millis(333),
			justification_period: 512,
			name: Some(name),
			observer_enabled: false,
			keystore,
			local_role: role,
			telemetry: telemetry.as_ref().map(|x| x.handle()),
		};

		// start the full GRANDPA voter
		// NOTE: non-authorities could run the GRANDPA observer protocol, but at
		// this point the full voter should provide better guarantees of block
		// and vote data availability than the observer. The observer has not
		// been tested extensively yet and having most nodes in a network run it
		// could lead to finality stalls.
		let grandpa_config = sc_finality_grandpa::GrandpaParams {
			config,
			link: grandpa_link,
			network: network.clone(),
			telemetry: telemetry.as_ref().map(|x| x.handle()),
			voting_rule: sc_finality_grandpa::VotingRulesBuilder::default().build(),
			prometheus_registry,
			shared_voter_state: sc_finality_grandpa::SharedVoterState::empty(),
		};

		// the GRANDPA voter task is considered infallible, i.e.
		// if it fails we take down the service with it.
		task_manager.spawn_essential_handle().spawn_blocking(
			"grandpa-voter",
			sc_finality_grandpa::run_grandpa_voter(grandpa_config)?,
		);
	}

	network_starter.start_network();

	Ok(task_manager)
}

#[derive(structopt::StructOpt)]
struct Cli<GenesisConfig, Extension = sc_chain_spec::NoExtension> {
	#[structopt(skip)]
	_phantom: std::marker::PhantomData<(GenesisConfig, Extension)>,
	#[structopt(flatten)]
	run: sc_cli::RunCmd,
}

impl<GenesisConfig, Extension> SubstrateCli for Cli<GenesisConfig, Extension>
where
	GenesisConfig: sc_chain_spec::RuntimeGenesis + 'static,
	Extension:
		sp_runtime::DeserializeOwned + Send + Sync + sc_service::ChainSpecExtension + 'static,
{
	fn impl_name() -> String {
		"Runtime Hoster".into()
	}

	fn impl_version() -> String {
		Default::default()
	}

	fn description() -> String {
		env!("CARGO_PKG_DESCRIPTION").into()
	}

	fn author() -> String {
		env!("CARGO_PKG_AUTHORS").into()
	}

	fn support_url() -> String {
		"https://github.com/paritytech/substrate/issues/new".into()
	}

	fn copyright_start_year() -> i32 {
		2017
	}

	fn load_spec(&self, id: &str) -> std::result::Result<Box<dyn sc_service::ChainSpec>, String> {
		Ok(Box::new(sc_chain_spec::GenericChainSpec::<GenesisConfig, Extension>::from_json_file(
			std::path::PathBuf::from(id),
		)?))
	}

	fn native_runtime_version(
		_: &Box<dyn sc_chain_spec::ChainSpec>,
	) -> &'static sp_api::RuntimeVersion {
		&sp_api::RuntimeVersion {
			spec_name: sp_runtime::RuntimeString::Borrowed(""),
			impl_name: sp_runtime::RuntimeString::Borrowed(""),
			authoring_version: 0,
			spec_version: 0,
			impl_version: 0,
			apis: std::borrow::Cow::Borrowed(&[]),
			transaction_version: 0,
		}
	}
}

pub fn run<Block, RuntimeApi, Executor, GenesisConfig, Extension>() -> Result<(), sc_cli::Error>
where
	Block: BlockT + std::marker::Unpin,
	<Block as BlockT>::Hash: FromStr,
	<<Block as BlockT>::Header as HeaderT>::Number: AsPrimitive<usize>,
	Executor: NativeExecutionDispatch + 'static,
	RuntimeApi: ConstructRuntimeApi<Block, sc_service::TFullClient<Block, RuntimeApi, Executor>>
		+ Send
		+ Sync
		+ 'static,
	<RuntimeApi as ConstructRuntimeApi<
		Block,
		sc_service::TFullClient<Block, RuntimeApi, Executor>,
	>>::RuntimeApi: TaggedTransactionQueue<Block>
		+ sp_consensus_babe::BabeApi<Block>
		+ sp_block_builder::BlockBuilder<Block>
		+ sp_api::ApiExt<Block, StateBackend = StateBackend<Block>>
		+ sc_finality_grandpa::GrandpaApi<Block>
		+ sp_offchain::OffchainWorkerApi<Block>
		+ sp_api::Metadata<Block>
		+ sp_session::SessionKeys<Block>
		+ sp_authority_discovery::AuthorityDiscoveryApi<Block>,
	GenesisConfig: sc_chain_spec::RuntimeGenesis + 'static,
	Extension:
		sp_runtime::DeserializeOwned + Send + Sync + sc_service::ChainSpecExtension + 'static,
{
	let cli = Cli::<GenesisConfig, Extension>::from_args();

	let runner = cli.create_runner(&cli.run)?;
	runner.run_node_until_exit(|config| async move {
		new_full::<Block, RuntimeApi, Executor>(config)
	})?;

	Ok(())
}
