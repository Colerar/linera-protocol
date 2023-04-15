// Copyright (c) Facebook, Inc. and its affiliates.
// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use async_graphql::InputType;
use linera_base::identifiers::{BytecodeId, ChainId, EffectId, Owner};
use linera_chain::data_types::Certificate;
use linera_execution::Bytecode;
use linera_service::config::WalletState;
#[cfg(feature = "aws")]
use linera_views::test_utils::LocalStackTestContext;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::{
    env, fs,
    io::Write,
    ops::Range,
    path::PathBuf,
    process::{Command, Stdio},
    rc::Rc,
    str::FromStr,
    time::Duration,
};
use tempfile::{tempdir, TempDir};
use tokio::{process::Child, sync::Mutex};
use tonic_health::proto::{
    health_check_response::ServingStatus, health_client::HealthClient, HealthCheckRequest,
};
use tracing::{info, warn};

/// A static lock to prevent integration tests from running in parallel.
static INTEGRATION_TEST_GUARD: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

/// The name of the environment variable that allows specifying additional arguments to be passed
/// to `cargo` when starting client, server and proxy processes.
const CARGO_ENV: &str = "INTEGRATION_TEST_CARGO_PARAMS";

#[test_log::test(tokio::test)]
async fn test_examples_in_readme_simple() -> std::io::Result<()> {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;

    let dir = tempdir().unwrap();
    let file = std::io::BufReader::new(std::fs::File::open("../README.md")?);
    let mut quotes = get_bash_quotes(file)?;
    // Check that we have the expected number of examples starting with "```bash".
    assert_eq!(quotes.len(), 1);
    let quote = quotes.pop().unwrap();

    let mut test_script = std::fs::File::create(dir.path().join("test.sh"))?;
    write!(&mut test_script, "{}", quote)?;

    let status = Command::new("bash")
        .current_dir("..") // root of the repo
        .arg("-e")
        .arg("-x")
        .arg(dir.path().join("test.sh"))
        .status()?;
    assert!(status.success());
    Ok(())
}

#[allow(clippy::while_let_on_iterator)]
fn get_bash_quotes<R>(reader: R) -> std::io::Result<Vec<String>>
where
    R: std::io::BufRead,
{
    let mut result = Vec::new();
    let mut lines = reader.lines();

    while let Some(line) = lines.next() {
        let line = line?;
        if line.starts_with("```bash") {
            let mut quote = String::new();
            while let Some(line) = lines.next() {
                let line = line?;
                if line.starts_with("```") {
                    break;
                }
                quote += &line;
                quote += "\n";
            }
            result.push(quote);
        }
    }

    Ok(result)
}

#[cfg(feature = "aws")]
mod aws_test {
    use super::*;

    const ROCKSDB_STORAGE: &str = "--storage rocksdb:server_\"$I\"_\"$J\".db";
    const DYNAMO_DB_STORAGE: &str = "--storage dynamodb:server-\"$I\":localstack";

    const BUILD: &str = "cargo build";
    const AWS_BUILD: &str = "cargo build --features aws";

    #[test_log::test(tokio::test)]
    async fn test_examples_in_readme_with_dynamo_db() -> anyhow::Result<()> {
        let _localstack_guard = LocalStackTestContext::new().await?;
        let dir = tempdir().unwrap();
        let file = std::io::BufReader::new(std::fs::File::open("../README.md")?);
        let mut quotes = get_bash_quotes(file)?;
        // Check that we have the expected number of examples starting with "```bash".
        assert_eq!(quotes.len(), 1);
        let quote = quotes.pop().unwrap();
        assert_eq!(quote.matches(ROCKSDB_STORAGE).count(), 1);
        let quote = quote.replace(ROCKSDB_STORAGE, DYNAMO_DB_STORAGE);
        let quote = quote.replace(BUILD, AWS_BUILD);

        let mut test_script = std::fs::File::create(dir.path().join("test.sh"))?;
        write!(&mut test_script, "{}", quote)?;

        let status = Command::new("bash")
            .current_dir("..") // root of the repo
            .arg("-e")
            .arg("-x")
            .arg(dir.path().join("test.sh"))
            .status()?;
        assert!(status.success());
        Ok(())
    }
}

#[derive(Copy, Clone)]
enum Network {
    Grpc,
    Simple,
}

impl Network {
    fn internal(&self) -> &'static str {
        match self {
            Network::Grpc => "\"Grpc\"",
            Network::Simple => "{ Simple = \"Udp\" }",
        }
    }

    fn external(&self) -> &'static str {
        match self {
            Network::Grpc => "\"Grpc\"",
            Network::Simple => "{ Simple = \"Tcp\" }",
        }
    }

    fn external_short(&self) -> &'static str {
        match self {
            Network::Grpc => "grpc",
            Network::Simple => "tcp",
        }
    }
}

struct Client {
    tmp_dir: Rc<TempDir>,
    storage: String,
    wallet: String,
    genesis: String,
    max_pending_messages: usize,
    network: Network,
}

impl Client {
    fn new(tmp_dir: Rc<TempDir>, network: Network, id: usize) -> Self {
        Self {
            tmp_dir,
            storage: format!("rocksdb:client_{}.db", id),
            wallet: format!("wallet_{}.json", id),
            genesis: "genesis.json".to_string(),
            max_pending_messages: 10_000,
            network,
        }
    }

    fn client_run(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("cargo");
        command
            .current_dir(&self.tmp_dir.path().canonicalize().unwrap())
            .kill_on_drop(true)
            .arg("run");
        if let Ok(var) = env::var(CARGO_ENV) {
            command.args(var.split_whitespace());
        }
        command
            .arg("--features")
            .arg("benchmark")
            .arg("--manifest-path")
            .arg(env::current_dir().unwrap().join("Cargo.toml"))
            .args(["--bin", "client"])
            .arg("--")
            .args(["--wallet", &self.wallet])
            .args(["--genesis", &self.genesis])
            .args(["--send-timeout-us", "10000000"])
            .args(["--recv-timeout-us", "10000000"]);
        command
    }

    fn client_run_with_storage(&self) -> tokio::process::Command {
        let mut command = self.client_run();
        command
            .args(["--storage", &self.storage.to_string()])
            .args([
                "--max-pending-messages",
                &self.max_pending_messages.to_string(),
            ]);
        command
    }

    async fn generate_client_config(&self) {
        self.client_run()
            .args(["create_genesis_config", "10"])
            .args(["--initial-funding", "10"])
            .args(["--committee", "committee.json"])
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    }

    async fn run_command(command: &mut tokio::process::Command) -> Vec<u8> {
        let output = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "Command {:?} failed; stderr:\n{}\n(end stderr)",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    async fn publish_application<I, J>(
        &self,
        contract: PathBuf,
        service: PathBuf,
        arg: I,
        publisher: J,
    ) where
        I: ToString,
        J: Into<Option<ChainId>>,
    {
        Self::run_command(
            self.client_run_with_storage()
                .arg("publish")
                .args([contract, service])
                .arg(arg.to_string())
                .args(publisher.into().iter().map(ChainId::to_string)),
        )
        .await;
    }

    async fn run_node_service<I, P>(&self, chain_id: I, port: P) -> Child
    where
        I: Into<Option<ChainId>>,
        P: Into<Option<u16>>,
    {
        let port = port.into().unwrap_or(8080);
        let child = self
            .client_run_with_storage()
            .arg("service")
            .args(chain_id.into().as_ref().map(ChainId::to_string))
            .args(["--port".to_string(), port.to_string()])
            .spawn()
            .unwrap();
        let client = reqwest::Client::new();
        loop {
            let request = client
                .get(format!("http://localhost:{}/", port))
                .send()
                .await;
            if request.is_ok() {
                info!("Node service has started");
                break;
            } else {
                warn!("Waiting for node service to start");
                tokio::time::sleep(Duration::from_millis(1_000)).await;
            }
        }
        child
    }

    async fn query_validators(&self, chain_id: Option<ChainId>) {
        let mut command = self.client_run_with_storage();
        command.arg("query_validators");
        if let Some(chain_id) = chain_id {
            command.arg(&chain_id.to_string());
        }
        Self::run_command(&mut command).await;
    }

    async fn query_balance(&self, chain_id: ChainId) -> anyhow::Result<usize> {
        let stdout = Self::run_command(
            self.client_run_with_storage()
                .arg("query_balance")
                .arg(&chain_id.to_string()),
        )
        .await;
        let amount = String::from_utf8_lossy(stdout.as_slice()).to_string();
        Ok(amount.trim().parse()?)
    }

    async fn transfer(&self, amount: usize, from: ChainId, to: ChainId) {
        Self::run_command(
            self.client_run_with_storage()
                .arg("transfer")
                .arg(&amount.to_string())
                .args(["--from", &from.to_string()])
                .args(["--to", &to.to_string()]),
        )
        .await;
    }

    async fn benchmark(&self, max_in_flight: usize) {
        self.client_run_with_storage()
            .arg("benchmark")
            .args(["--max-in-flight", &max_in_flight.to_string()])
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    }

    async fn open_chain(
        &self,
        from: ChainId,
        to_owner: Option<Owner>,
    ) -> anyhow::Result<(ChainId, Certificate)> {
        let mut command = self.client_run_with_storage();
        command
            .arg("open_chain")
            .args(["--from", &from.to_string()]);

        if let Some(owner) = to_owner {
            command.args(["--to-public-key", &owner.to_string()]);
        }

        let stdout = Self::run_command(&mut command).await;

        let as_string = String::from_utf8_lossy(stdout.as_slice());
        let mut split = as_string.split('\n');
        let chain_id = ChainId::from_str(split.next().unwrap())?;
        let cert: Certificate = bcs::from_bytes(&hex::decode(split.next().unwrap())?)?;

        Ok((chain_id, cert))
    }

    fn get_wallet(&self) -> WalletState {
        WalletState::read_or_create(self.tmp_dir.path().join(&self.wallet).as_path()).unwrap()
    }

    async fn check_for_chain_in_wallet(&self, chain: ChainId) -> bool {
        self.get_wallet().get(chain).is_some()
    }

    async fn set_validator(&self, name: &str, port: usize, votes: usize) {
        let address = format!("{}:127.0.0.1:{}", self.network.external_short(), port);
        Self::run_command(
            self.client_run_with_storage()
                .arg("set_validator")
                .args(["--name", name])
                .args(["--address", &address])
                .args(["--votes", &votes.to_string()]),
        )
        .await;
    }

    async fn remove_validator(&self, name: &str) {
        Self::run_command(
            self.client_run_with_storage()
                .arg("remove_validator")
                .args(["--name", name]),
        )
        .await;
    }

    async fn keygen(&self) -> anyhow::Result<Owner> {
        let stdout = Self::run_command(self.client_run().arg("keygen")).await;
        Ok(Owner::from_str(
            String::from_utf8_lossy(stdout.as_slice()).trim(),
        )?)
    }

    async fn assign(
        &self,
        owner: Owner,
        chain_id: ChainId,
        certificate: Certificate,
    ) -> anyhow::Result<()> {
        Self::run_command(
            self.client_run_with_storage()
                .arg("assign")
                .args(["--key", &owner.to_string()])
                .args(["--chain", &chain_id.to_string()])
                .args(["--certificate", &hex::encode(bcs::to_bytes(&certificate)?)]),
        )
        .await;
        Ok(())
    }

    async fn synchronize_balance(&self, chain_id: ChainId) {
        Self::run_command(
            self.client_run_with_storage()
                .arg("sync_balance")
                .arg(&chain_id.to_string()),
        )
        .await;
    }
}

struct Validator {
    _proxy: Child,
    servers: Vec<Child>,
}

impl Validator {
    fn new(proxy: Child) -> Self {
        Self {
            _proxy: proxy,
            servers: vec![],
        }
    }

    fn add_server(&mut self, server: Child) {
        self.servers.push(server)
    }

    fn kill_server(&mut self, index: usize) {
        self.servers.remove(index);
    }
}

struct TestRunner {
    tmp_dir: Rc<TempDir>,
    network: Network,
}

impl TestRunner {
    fn new(network: Network) -> Self {
        Self {
            tmp_dir: Rc::new(tempdir().unwrap()),
            network,
        }
    }

    fn tmp_dir(&self) -> Rc<TempDir> {
        self.tmp_dir.clone()
    }

    fn cargo_run(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("cargo");
        command
            .current_dir(&self.tmp_dir.path().canonicalize().unwrap())
            .kill_on_drop(true)
            .arg("run");
        if let Ok(var) = env::var(CARGO_ENV) {
            command.args(var.split_whitespace());
        }
        command
            .arg("--manifest-path")
            .arg(env::current_dir().unwrap().join("Cargo.toml"))
            .arg("--features")
            .arg("benchmark");
        command
    }

    fn proxy_port(i: usize) -> usize {
        9000 + i * 100
    }

    fn shard_port(i: usize, j: usize) -> usize {
        9000 + i * 100 + j
    }

    fn internal_port(i: usize) -> usize {
        10000 + i * 100
    }

    fn metrics_port(i: usize) -> usize {
        11000 + i * 100
    }

    fn configuration_string(&self, server_number: usize) -> String {
        let n = server_number;
        let path = self
            .tmp_dir()
            .path()
            .canonicalize()
            .unwrap()
            .join(format!("validator_{n}.toml"));
        let port = Self::proxy_port(n);
        let internal_port = Self::internal_port(n);
        let metrics_port = Self::metrics_port(n);
        let external_protocol = self.network.external();
        let internal_protocol = self.network.internal();
        let mut content = format!(
            r#"
                server_config_path = "server_{n}.json"
                host = "127.0.0.1"
                port = {port}
                internal_host = "127.0.0.1"
                internal_port = {internal_port}
                external_protocol = {external_protocol}
                internal_protocol = {internal_protocol}
            "#
        );
        for k in 1..=4 {
            let shard_port = Self::shard_port(n, k);
            let shard_metrics_port = metrics_port + k;
            content.push_str(&format!(
                r#"
                
                [[shards]]
                host = "127.0.0.1"
                port = {shard_port}
                metrics_host = "127.0.0.1"
                metrics_port = {shard_metrics_port}
                "#
            ));
        }
        fs::write(&path, content).unwrap();
        path.into_os_string().into_string().unwrap()
    }

    async fn generate_initial_server_config(&self, n_validators: usize) {
        let mut command = self.cargo_run();
        command
            .args(["--bin", "server"])
            .arg("generate")
            .arg("--validators");
        for i in 1..n_validators + 1 {
            command.arg(&self.configuration_string(i));
        }
        command
            .args(["--committee", "committee.json"])
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    }

    async fn generate_server_config(&self, server_number: usize) -> anyhow::Result<String> {
        let output = self
            .cargo_run()
            .args(["--bin", "server"])
            .arg("generate")
            .arg("--validators")
            .arg(&self.configuration_string(server_number))
            .stdout(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;
        Ok(String::from_utf8_lossy(output.stdout.as_slice())
            .to_string()
            .trim()
            .to_string())
    }

    async fn run_proxy(&self, i: usize) -> Child {
        let child = self
            .cargo_run()
            .args(["--bin", "proxy"])
            .arg("--")
            .arg(format!("server_{}.json", i))
            .spawn()
            .unwrap();

        match self.network {
            Network::Grpc => {
                let port = Self::proxy_port(i);
                let connection =
                    tonic::transport::Endpoint::new(format!("http://127.0.0.1:{port}"))
                        .unwrap()
                        .connect_lazy();
                let mut client = HealthClient::new(connection);
                loop {
                    let result = client.check(HealthCheckRequest::default()).await;
                    if result.is_ok()
                        && result.unwrap().get_ref().status() == ServingStatus::Serving
                    {
                        info!("Validator proxy {i} has started");
                        break;
                    } else {
                        warn!("Waiting for validator proxy {i} to start");
                        tokio::time::sleep(Duration::from_millis(1_000)).await;
                    }
                }
            }
            Network::Simple => {
                info!("Letting validator proxy {i} start");
                tokio::time::sleep(Duration::from_millis(2_000)).await;
            }
        }
        child
    }

    async fn run_server(&self, i: usize, j: usize) -> Child {
        let child = self
            .cargo_run()
            .args(["--bin", "server"])
            .arg("run")
            .args(["--storage", &format!("rocksdb:server_{}_{}.db", i, j)])
            .args(["--server", &format!("server_{}.json", i)])
            .args(["--shard", &j.to_string()])
            .args(["--genesis", "genesis.json"])
            .spawn()
            .unwrap();

        match self.network {
            Network::Grpc => {
                let port = Self::shard_port(i, j);
                let connection =
                    tonic::transport::Endpoint::new(format!("http://127.0.0.1:{port}"))
                        .unwrap()
                        .connect_lazy();
                let mut client = HealthClient::new(connection);
                loop {
                    let result = client.check(HealthCheckRequest::default()).await;
                    if result.is_ok()
                        && result.unwrap().get_ref().status() == ServingStatus::Serving
                    {
                        info!("Validator server {i}:{j} has started");
                        break;
                    } else {
                        warn!("Waiting for validator server {i}:{j} to start");
                        tokio::time::sleep(Duration::from_millis(1_000)).await;
                    }
                }
            }
            Network::Simple => {
                info!("Letting validator server {i}:{j} start");
                tokio::time::sleep(Duration::from_millis(2_000)).await;
            }
        }
        child
    }

    async fn run_local_net(&self, n_validators: usize) -> Vec<Validator> {
        self.start_validators(1..n_validators + 1).await
    }

    async fn start_validators(&self, validator_range: Range<usize>) -> Vec<Validator> {
        let mut validators = vec![];
        for i in validator_range {
            let proxy = self.run_proxy(i).await;
            let mut validator = Validator::new(proxy);
            for j in 0..4 {
                let server = self.run_server(i, j).await;
                validator.add_server(server);
            }
            validators.push(validator);
        }
        validators
    }

    async fn build_application(&self, name: &str) -> (PathBuf, PathBuf) {
        let examples_dir = env::current_dir().unwrap().join("../linera-examples/");
        tokio::process::Command::new("cargo")
            .current_dir(self.tmp_dir.path().canonicalize().unwrap())
            .arg("build")
            .arg("--release")
            .args(["--target", "wasm32-unknown-unknown"])
            .arg("--manifest-path")
            .arg(examples_dir.join(name).join("Cargo.toml"))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();

        let release_dir = examples_dir.join("target/wasm32-unknown-unknown/release");
        let contract = release_dir.join(format!("{}_contract.wasm", name.replace('-', "_")));
        let service = release_dir.join(format!("{}_service.wasm", name.replace('-', "_")));

        (contract, service)
    }
}

async fn get_application_uri<I, P>(chain_id: I, port: P) -> String
where
    I: Into<Option<ChainId>>,
    P: Into<Option<u16>>,
{
    let query_string = if let Some(chain_id) = chain_id.into() {
        format!(
            "query {{ applications(chainId: \"{}\") {{ link }}}}",
            chain_id
        )
    } else {
        "query { applications { link }}".to_string()
    };
    let query = json!({ "query": query_string });
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://localhost:{}/", port.into().unwrap_or(8080)))
        .json(&query)
        .send()
        .await
        .unwrap();
    let response_body: Value = res.json().await.unwrap();
    let application_uri = response_body
        .get("data")
        .unwrap()
        .get("applications")
        .unwrap()
        .as_array()
        .unwrap()
        .get(0)
        .unwrap()
        .get("link")
        .unwrap();
    application_uri.as_str().unwrap().to_string()
}

async fn publish_application<P>(contract: PathBuf, service: PathBuf, port: P) -> Certificate
where
    P: Into<Option<u16>>,
{
    let contract_code = Bytecode::load_from_file(&contract).await.unwrap();
    let service_code = Bytecode::load_from_file(&service).await.unwrap();
    let query_string = format!(
        "mutation {{ publishBytecodes(contract: {}, service: {}) }}",
        contract_code.to_value(),
        service_code.to_value(),
    );
    let query = json!({ "query": query_string });
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://localhost:{}/", port.into().unwrap_or(8080)))
        .json(&query)
        .send()
        .await
        .unwrap();
    let response_body: Value = res.json().await.unwrap();
    if let Some(errors) = response_body.get("errors") {
        let mut error_string = errors.to_string();
        if error_string.len() > 10000 {
            error_string = format!(
                "{}..{}",
                &error_string[..5000],
                &error_string[(error_string.len() - 5000)..]
            );
        }
        panic!("publish_application failed: {}", error_string);
    }
    serde_json::from_value(
        response_body
            .get("data")
            .unwrap()
            .get("publishBytecodes")
            .unwrap()
            .clone(),
    )
    .unwrap()
}

async fn create_application<P>(bytecode_id: BytecodeId, port: P)
where
    P: Into<Option<u16>>,
{
    let query_string = format!(
        "mutation {{ createApplication(bytecodeId: {}, parameters: [], \
        initializationArgument: [], requiredApplicationIds: []) }}",
        bytecode_id.to_value(),
    );
    let query = json!({ "query": query_string });
    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://localhost:{}/", port.into().unwrap_or(8080)))
        .json(&query)
        .send()
        .await
        .unwrap();
    let response_body: Value = res.json().await.unwrap();
    if let Some(errors) = response_body.get("errors") {
        panic!("create_application failed: {}", errors);
    }
}

async fn get_counter_value(application_uri: &str) -> u64 {
    let response_body = query_application(application_uri, "query { value }").await;
    response_body
        .get("data")
        .unwrap()
        .get("value")
        .unwrap()
        .as_u64()
        .unwrap()
}

async fn query_application(application_uri: &str, query_string: &str) -> Value {
    let query = json!({ "query": query_string });
    let client = reqwest::Client::new();
    let res = client
        .post(application_uri)
        .json(&query)
        .send()
        .await
        .unwrap();
    if !res.status().is_success() {
        panic!(
            "Query \"{}\" failed: {}",
            query_string,
            res.text().await.unwrap()
        );
    }
    res.json().await.unwrap()
}

async fn increment_counter_value(application_uri: &str, increment: u64) {
    let query_string = format!(
        "mutation {{  executeOperation(operation: {{ increment: {} }})}}",
        increment
    );
    let query = json!({ "query": query_string });
    let client = reqwest::Client::new();
    client
        .post(application_uri)
        .json(&query)
        .send()
        .await
        .unwrap();
}

#[test_log::test(tokio::test)]
async fn end_to_end() {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;

    let network = Network::Grpc;
    let runner = TestRunner::new(network);
    let client = Client::new(runner.tmp_dir(), network, 1);
    let n_validators = 4;

    let original_counter_value = 35;
    let increment = 5;

    runner.generate_initial_server_config(n_validators).await;
    client.generate_client_config().await;
    let _local_net = runner.run_local_net(n_validators).await;
    let (contract, service) = runner.build_application("counter-graphql").await;

    client
        .publish_application(contract, service, original_counter_value, None)
        .await;
    let _node_service = client.run_node_service(None, None).await;

    let application_uri = get_application_uri(None, None).await;

    let counter_value = get_counter_value(&application_uri).await;
    assert_eq!(counter_value, original_counter_value);

    increment_counter_value(&application_uri, increment).await;

    let counter_value = get_counter_value(&application_uri).await;
    assert_eq!(counter_value, original_counter_value + increment);
}

#[test_log::test(tokio::test)]
async fn test_multiple_wallets() {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;

    // Create runner and two clients.
    let runner = TestRunner::new(Network::Grpc);
    let client_1 = Client::new(runner.tmp_dir(), Network::Grpc, 1);
    let client_2 = Client::new(runner.tmp_dir(), Network::Grpc, 2);
    let n_validators = 4;

    // Create initial server and client config.
    runner.generate_initial_server_config(n_validators).await;
    client_1.generate_client_config().await;

    // Start local network.
    let _local_net = runner.run_local_net(n_validators).await;

    // Get some chain owned by Client 1.
    let chain_1 = *client_1.get_wallet().chain_ids().first().unwrap();

    // Generate a key for Client 2.
    let client_2_key = client_2.keygen().await.unwrap();

    // Open chain on behalf of Client 2.
    let (chain_2, cert) = client_1
        .open_chain(chain_1, Some(client_2_key))
        .await
        .unwrap();

    // Assign chain_2 to client_2_key.
    client_2.assign(client_2_key, chain_2, cert).await.unwrap();

    // Check initial balance of Chain 1.
    assert_eq!(client_1.query_balance(chain_1).await.unwrap(), 10);

    // Transfer 5 units from Chain 1 to Chain 2.
    client_1.transfer(5, chain_1, chain_2).await;
    client_2.synchronize_balance(chain_2).await;

    assert_eq!(client_1.query_balance(chain_1).await.unwrap(), 5);
    assert_eq!(client_2.query_balance(chain_2).await.unwrap(), 5);

    // Transfer 2 units from Chain 2 to Chain 1.
    client_2.transfer(2, chain_2, chain_1).await;
    client_1.synchronize_balance(chain_1).await;

    assert_eq!(client_1.query_balance(chain_1).await.unwrap(), 7);
    assert_eq!(client_2.query_balance(chain_2).await.unwrap(), 3);
}

#[test_log::test(tokio::test)]
async fn reconfiguration_test_grpc() {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;
    test_reconfiguration(Network::Grpc).await;
}

#[test_log::test(tokio::test)]
async fn reconfiguration_test_simple() {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;
    test_reconfiguration(Network::Simple).await;
}

async fn test_reconfiguration(network: Network) {
    let runner = TestRunner::new(network);
    let client = Client::new(runner.tmp_dir(), network, 1);
    let n_validators = 4;

    runner.generate_initial_server_config(n_validators).await;
    client.generate_client_config().await;
    let mut local_net = runner.run_local_net(n_validators).await;

    client.query_validators(None).await;

    // Query balance for first and last user chain
    let chain_1 =
        ChainId::from_str("91c7b394ef500cd000e365807b770d5b76a6e8c9c2f2af8e58c205e521b5f646")
            .unwrap();
    let chain_2 =
        ChainId::from_str("170883d704512b1682064639bdda0aab27756727af8e0dc5732bae70b2e15997")
            .unwrap();
    assert_eq!(client.query_balance(chain_1).await.unwrap(), 10);
    assert_eq!(client.query_balance(chain_2).await.unwrap(), 10);

    // Transfer 10 units then 5 back
    client.transfer(10, chain_1, chain_2).await;
    client.transfer(5, chain_2, chain_1).await;

    // Restart last server (dropping it kills the process)
    let validator_4 = local_net.get_mut(3).unwrap();
    validator_4.kill_server(3);
    validator_4.add_server(runner.run_server(4, 3).await);

    // Query balances again
    assert_eq!(client.query_balance(chain_1).await.unwrap(), 5);
    assert_eq!(client.query_balance(chain_2).await.unwrap(), 15);

    // Launch local benchmark using all user chains
    client.benchmark(500).await;

    // Create derived chain
    let (chain_3, _) = client.open_chain(chain_1, None).await.unwrap();

    // Inspect state of derived chain
    assert!(client.check_for_chain_in_wallet(chain_3).await);

    // Create configurations for two more validators
    let server_5 = runner.generate_server_config(5).await.unwrap();
    let server_6 = runner.generate_server_config(6).await.unwrap();

    // Start the validators
    local_net.extend(runner.start_validators(5..7).await);

    // Add validator 5
    client.set_validator(&server_5, 9500, 100).await;

    assert_eq!(client.query_balance(chain_1).await.unwrap(), 5);
    client.query_validators(None).await;
    client.query_validators(Some(chain_1)).await;

    // Add validator 6
    client.set_validator(&server_6, 9600, 100).await;

    // Remove validator 5
    client.remove_validator(&server_5).await;
    local_net.remove(4);

    assert_eq!(client.query_balance(chain_1).await.unwrap(), 5);
    client.query_validators(None).await;
    client.query_validators(Some(chain_1)).await;
}

#[test_log::test(tokio::test)]
async fn social_user_pub_sub() {
    let _guard = INTEGRATION_TEST_GUARD.lock().await;

    let network = Network::Grpc;
    let runner = TestRunner::new(network);
    let client1 = Client::new(runner.tmp_dir(), network, 1);
    let client2 = Client::new(runner.tmp_dir(), network, 2);
    let n_validators = 4;

    // Create initial server and client config.
    runner.generate_initial_server_config(n_validators).await;
    client1.generate_client_config().await;

    // Start local network.
    let _local_net = runner.run_local_net(n_validators).await;
    let (contract, service) = runner.build_application("social").await;

    let chain1 = client1.get_wallet().default_chain().unwrap();
    let client2key = client2.keygen().await.unwrap();

    // Open chain on behalf of Client 2.
    let (chain2, cert) = client1.open_chain(chain1, Some(client2key)).await.unwrap();

    // Assign chain_2 to client_2_key.
    client2.assign(client2key, chain2, cert).await.unwrap();

    let _node_service1 = client1.run_node_service(chain1, 8080).await;
    let _node_service2 = client2.run_node_service(chain2, 8081).await;

    let cert = publish_application(contract, service, 8080).await;

    assert_eq!(cert.value.effects().len(), 1);
    let bytecode_id = BytecodeId(EffectId {
        chain_id: chain1,
        height: cert.value.block().height,
        index: 0,
    });
    create_application(bytecode_id, 8080).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let app1 = get_application_uri(chain1, 8080).await;
    let query = format!("mutation {{ subscribe(chainId: \"{}\") }}", chain2);
    query_application(&app1, &query).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let app2 = get_application_uri(chain2, 8081).await;
    let query = "mutation { post(text: \"Linera Social is the new Mastodon!\") }";
    query_application(&app2, query).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let query = "query { receivedPostsKeys(count: 5) { author, index } }";
    let expected_response = json!({"data": { "receivedPostsKeys": [
        { "author": chain2, "index": 0 }
    ]}});
    let response = query_application(&app1, query).await;
    assert_eq!(response, expected_response);
}
