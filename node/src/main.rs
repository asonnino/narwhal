// Copyright(C) Facebook, Inc. and its affiliates.
use crate::prometheus::start_prometheus_server;
use ::prometheus::default_registry;
use anyhow::{Context, Result};
use clap::{crate_name, crate_version, App, AppSettings, ArgMatches, SubCommand};
use config::Export as _;
use config::Import as _;
use config::{Committee, KeyPair, Parameters, WorkerId};
use consensus::{Consensus, ConsensusOutput};
use env_logger::Env;
use metrics::ConsensusMetrics;
use primary::Primary;
use std::sync::{Arc, RwLock};
use store::Store;
use tokio::sync::mpsc::{channel, Receiver};
use worker::Worker;

mod metrics;
mod prometheus;

/// The default channel capacity.
pub const CHANNEL_CAPACITY: usize = 1_000;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("A research implementation of Narwhal and Tusk.")
        .args_from_usage("-v... 'Sets the level of verbosity'")
        .subcommand(
            SubCommand::with_name("generate_keys")
                .about("Print a fresh key pair to file")
                .args_from_usage("--filename=<FILE> 'The file where to print the new key pair'"),
        )
        .subcommand(
            SubCommand::with_name("run")
                .about("Run a node")
                .args_from_usage("--keys=<FILE> 'The file containing the node keys'")
                .args_from_usage("--committee=<FILE> 'The file containing committee information'")
                .args_from_usage("--parameters=[FILE] 'The file containing the node parameters'")
                .args_from_usage("--store=<PATH> 'The path where to create the data store'")
                .args_from_usage("--prometheus=[Addr] 'The prometheus server address'")
                .subcommand(SubCommand::with_name("primary").about("Run a single primary"))
                .subcommand(
                    SubCommand::with_name("worker")
                        .about("Run a single worker")
                        .args_from_usage("--id=<INT> 'The worker id'"),
                )
                .setting(AppSettings::SubcommandRequiredElseHelp),
        )
        .setting(AppSettings::SubcommandRequiredElseHelp)
        .get_matches();

    let log_level = match matches.occurrences_of("v") {
        0 => "error",
        1 => "warn",
        2 => "info",
        3 => "debug",
        _ => "trace",
    };
    let mut logger = env_logger::Builder::from_env(Env::default().default_filter_or(log_level));
    #[cfg(feature = "benchmark")]
    logger.format_timestamp_millis();
    logger.init();

    match matches.subcommand() {
        ("generate_keys", Some(sub_matches)) => KeyPair::new()
            .export(sub_matches.value_of("filename").unwrap())
            .context("Failed to generate key pair")?,
        ("run", Some(sub_matches)) => run(sub_matches).await?,
        _ => unreachable!(),
    }
    Ok(())
}

// Runs either a worker or a primary.
async fn run(matches: &ArgMatches<'_>) -> Result<()> {
    let key_file = matches.value_of("keys").unwrap();
    let committee_file = matches.value_of("committee").unwrap();
    let parameters_file = matches.value_of("parameters");
    let store_path = matches.value_of("store").unwrap();

    // Read the committee and node's keypair from file.
    let keypair = KeyPair::import(key_file).context("Failed to load the node's keypair")?;
    let committee =
        Committee::import(committee_file).context("Failed to load the committee information")?;

    // Load default parameters if none are specified.
    let parameters = match parameters_file {
        Some(filename) => {
            Parameters::import(filename).context("Failed to load the node's parameters")?
        }
        None => Parameters::default(),
    };
    let updatable_parameters = Arc::new(RwLock::new(parameters.clone().into()));

    // Make the data store.
    let store = Store::new(store_path).context("Failed to create a store")?;

    // Channels the sequence of certificates.
    let (tx_output, rx_output) = channel(CHANNEL_CAPACITY);

    // Make a prometheus registry and start a prometheus server.
    let registry = match matches.value_of("prometheus") {
        Some(address) => {
            let registry = default_registry();

            let socket_address = address
                .parse()
                .context("Invalid prometheus socket address")?;
            let _handle =
                start_prometheus_server(socket_address, &registry, updatable_parameters.clone());

            Some(registry)
        }
        None => None,
    };

    // Check whether to run a primary, a worker, or an entire authority.
    let consensus_metrics = match matches.subcommand() {
        // Spawn the primary and consensus core.
        ("primary", _) => {
            let (tx_new_certificates, rx_new_certificates) = channel(CHANNEL_CAPACITY);
            let (tx_feedback, rx_feedback) = channel(CHANNEL_CAPACITY);
            Primary::spawn(
                keypair,
                committee.clone(),
                parameters.clone(),
                store,
                /* tx_consensus */ tx_new_certificates,
                /* rx_consensus */ rx_feedback,
            );
            Consensus::spawn(
                committee,
                parameters.gc_depth,
                /* rx_primary */ rx_new_certificates,
                /* tx_primary */ tx_feedback,
                tx_output,
            );

            // Consensus metrics.
            registry.map(|x| ConsensusMetrics::new(x))
        }

        // Spawn a single worker.
        ("worker", Some(sub_matches)) => {
            let id = sub_matches
                .value_of("id")
                .unwrap()
                .parse::<WorkerId>()
                .context("The worker id must be a positive integer")?;
            Worker::spawn(
                keypair.name,
                id,
                committee,
                parameters,
                updatable_parameters.clone(),
                store,
                registry,
            );

            // Consensus metrics.
            None
        }
        _ => unreachable!(),
    };

    // Analyze the consensus' output.
    analyze(rx_output, consensus_metrics).await;

    // If this expression is reached, the program ends and all other tasks terminate.
    unreachable!();
}

/// Receives an ordered list of certificates and apply any application-specific logic.
async fn analyze(mut rx_output: Receiver<ConsensusOutput>, metrics: Option<ConsensusMetrics>) {
    // NOTE: Here goes the application logic.
    #[cfg(not(feature = "benchmark"))]
    {
        let _metrics = metrics;
        while let Some(_output) = rx_output.recv().await {}
    }

    #[cfg(feature = "benchmark")]
    {
        let mut first_transaction_recorded = false;
        let mut last_transaction_time = 0;
        while let Some(output) = rx_output.recv().await {
            if let Some(metrics) = metrics.as_ref() {
                metrics.committed_certificates_total.inc();

                let commit_time = output.commit_time;
                let delta = commit_time - last_transaction_time;
                metrics.last_committed_transaction.inc_by(delta);
                last_transaction_time = commit_time;

                for payload in output.certificate.header.payload {
                    let batch_info = payload.batch_benchmark_info;
                    metrics.committed_bytes_total.inc_by(batch_info.size as u64);
                    metrics
                        .committed_sample_transactions_total
                        .inc_by(batch_info.sample_txs.len() as u64);

                    for (_id, send_time) in batch_info.sample_txs {
                        if !first_transaction_recorded {
                            metrics.first_sent_transaction.inc_by(send_time as u64);
                            first_transaction_recorded = true;
                        }

                        let latency = commit_time - send_time;
                        metrics.latency_total.inc_by(latency as u64);
                        metrics.latency_square_total.inc_by(latency.pow(2) as u64);
                    }
                }
            }
        }
    }
}
