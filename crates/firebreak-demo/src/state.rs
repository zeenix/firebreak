//! The dashboard's state: everything the page shows, gathered into one document.
//!
//! The state comes from the public files under `<data>/public`, the agent's status API and the
//! node. It reads nothing private: no owner, agent or merchant secret file is ever opened.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flamed_rpc::HttpClient;
use serde::Serialize;

use crate::agent::Agent;
use crate::journal::{self, Journal, Views};
use crate::node::{self, NodeTip, Probe};
use crate::status::{self, AgentStatus, MerchantStatus, OwnerStatus, Source};

/// What the dashboard server needs to answer: where the files are and who to ask.
pub struct App {
    data_dir: PathBuf,
    node: HttpClient,
    agent: Agent,
    views: Views,
}

/// Everything the page shows.
///
/// A source that cannot be read says so and leaves the rest alone, so a missing file or a node
/// that is down shows as that and nothing else.
#[derive(Debug, Serialize)]
pub struct State {
    /// When this document was assembled, in unix seconds.
    pub generated: u64,
    pub node: Source<NodeTip>,
    pub agent: Source<AgentStatus>,
    pub owner: Source<OwnerStatus>,
    pub merchant: Source<MerchantStatus>,
    /// The public journal with every transaction decoded: the Public observer's data.
    pub observer: Source<Journal>,
}

impl App {
    /// A server for the files under `data_dir`, the node at `rpc` and the agent at `agent`.
    pub fn new(data_dir: PathBuf, rpc: &str, agent: &str) -> Result<App, String> {
        Ok(App {
            data_dir,
            node: node::client(rpc)?,
            agent: Agent::new(agent),
            views: Mutex::new(HashMap::new()),
        })
    }

    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Gathers the state. Only a failure of the server itself is an error here: every source that
    /// cannot be read is part of the state instead.
    pub async fn state(self: &Arc<App>) -> Result<State, String> {
        // Reading files and verifying transactions is blocking work.
        let app = Arc::clone(self);
        let files = tokio::task::spawn_blocking(move || read_public(&app.data_dir, &app.views))
            .await
            .map_err(|error| format!("reading the public files failed: {error}"))?;
        let mut probe = Probe::new(&self.node);
        let (agent, tip) = tokio::join!(agent_status(&self.agent), probe.tip());
        let node_is_up = tip.is_ok();
        let mut state = State {
            generated: unix_now(),
            node: tip.into(),
            agent,
            owner: files.owner,
            merchant: files.merchant,
            observer: files.observer,
        };
        if node_is_up {
            state.annotate(&mut probe).await;
        }
        Ok(state)
    }
}

/// What the public files hold.
struct PublicFiles {
    owner: Source<OwnerStatus>,
    merchant: Source<MerchantStatus>,
    observer: Source<Journal>,
}

fn read_public(data_dir: &Path, views: &Views) -> PublicFiles {
    let public = data_dir.join("public");
    PublicFiles {
        owner: status::read_status(&public.join("owner-status.json")),
        merchant: status::read_status(&public.join("merchant-status.json")),
        observer: journal::read(&public.join("journal.jsonl"), views),
    }
}

async fn agent_status(agent: &Agent) -> Source<AgentStatus> {
    let reply = match agent.get("/api/status", AGENT_TIMEOUT).await {
        Ok(reply) => reply,
        Err(message) => return Source::failed(format!("the agent is unreachable: {message}")),
    };
    if !reply.status.is_success() {
        return Source::failed(format!("the agent answered HTTP {}", reply.status.as_u16()));
    }
    match serde_json::from_slice(&reply.body) {
        Ok(status) => Source::ok(status),
        Err(error) => Source::failed(format!(
            "the agent's status is not the expected JSON: {error}"
        )),
    }
}

impl State {
    /// Adds what the node says about every voucher, recovery, spend and journal entry, so a
    /// snapshot that a role last wrote some time ago is shown next to the chain's own answer.
    async fn annotate(&mut self, probe: &mut Probe<'_>) {
        let contracts = probe.contracts(&self.voucher_ids()).await;
        let chain_of = |id: &str| contracts.get(&id.to_ascii_lowercase()).cloned();
        if let Some(owner) = &mut self.owner.data {
            for allowance in &mut owner.allowances {
                for voucher in &mut allowance.vouchers {
                    voucher.chain = chain_of(&voucher.id);
                }
                for recovery in &mut allowance.recovery {
                    recovery.chain = Some(probe.transaction(&recovery.txid).await);
                }
            }
        }
        if let Some(agent) = &mut self.agent.data {
            for voucher in agent.allowances.iter_mut().flat_map(|a| &mut a.vouchers) {
                voucher.chain = chain_of(&voucher.id);
            }
        }
        if let Some(merchant) = &mut self.merchant.data {
            for spend in &mut merchant.spends {
                spend.chain = Some(probe.transaction(&spend.txid).await);
            }
        }
        if let Some(journal) = &mut self.observer.data {
            for entry in &mut journal.entries {
                let Some(txid) = entry.txid_to_ask().map(str::to_owned) else {
                    continue;
                };
                entry.chain = Some(probe.transaction(&txid).await);
            }
        }
    }

    /// The IDs of every voucher the owner's snapshot and the agent know.
    fn voucher_ids(&self) -> Vec<String> {
        let owned = self
            .owner
            .data
            .iter()
            .flat_map(|owner| &owner.allowances)
            .flat_map(|allowance| &allowance.vouchers)
            .map(|voucher| voucher.id.clone());
        let delegated = self
            .agent
            .data
            .iter()
            .flat_map(|agent| &agent.allowances)
            .flat_map(|allowance| &allowance.vouchers)
            .map(|voucher| voucher.id.clone());
        owned.chain(delegated).collect()
    }
}

/// How long the agent gets to answer a status request.
const AGENT_TIMEOUT: Duration = Duration::from_secs(2);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::{Json, Router};
    use firebreak_core::build;
    use firebreak_core::devnet::{Devnet, Parties, fund, rng};
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;
    use crate::node::{ContractChain, TxChain};
    use crate::status::SourceState;
    use crate::testing::{self, SECRET};

    fn app(data_dir: &Path, rpc: &str, agent: &str) -> Arc<App> {
        Arc::new(App::new(data_dir.to_path_buf(), rpc, agent).expect("an app"))
    }

    #[test]
    fn an_empty_data_directory_has_nothing_initialised_yet() {
        let dir = tempdir().expect("a directory");
        let files = read_public(dir.path(), &Mutex::new(HashMap::new()));

        for state in [
            files.owner.state,
            files.merchant.state,
            files.observer.state,
        ] {
            assert_eq!(state, SourceState::Missing);
        }
        assert_eq!(files.owner.message.as_deref(), Some("not initialised yet"));
        assert!(files.owner.data.is_none() && files.merchant.data.is_none());
    }

    #[test]
    fn status_files_and_a_journal_with_a_bad_line_are_read_and_nothing_else_is_passed_on() {
        let dir = tempdir().expect("a directory");
        testing::write_public(dir.path(), "owner-status.json", &testing::owner_status());
        testing::write_public(
            dir.path(),
            "merchant-status.json",
            &testing::merchant_status(),
        );
        testing::write_public(dir.path(), "journal.jsonl", &testing::journal());
        let files = read_public(dir.path(), &Mutex::new(HashMap::new()));

        let owner = files.owner.data.as_ref().expect("the owner snapshot");
        assert_eq!(owner.updated, Some(1_696_000_000));
        assert_eq!(owner.tip_height, Some(12));
        assert_eq!(owner.wallet.balance, "900");
        let allowance = &owner.allowances[0];
        assert_eq!(allowance.total, "100");
        assert_eq!(allowance.vouchers.len(), 2);
        assert_eq!(allowance.vouchers[0].state, "redeemed");
        assert_eq!(allowance.recovery[0].state, "pending");
        let merchant = files.merchant.data.as_ref().expect("the merchant snapshot");
        assert_eq!(merchant.receipts[0].memo, "coffee");
        assert_eq!(merchant.balance, "50");

        // The journal: two entries, newest first, and the line that is not JSON is reported.
        let journal = files.observer.data.as_ref().expect("the journal");
        assert_eq!((journal.total, journal.shown), (2, 2));
        let times: Vec<u64> = journal.entries.iter().map(|entry| entry.time).collect();
        assert_eq!(times, [200, 100]);
        assert_eq!(journal.malformed.len(), 1);
        assert_eq!(journal.malformed[0].line, 2);
        let rejected = &journal.entries[0];
        assert_eq!((rejected.line, rejected.outcome.as_str()), (4, "rejected"));
        assert_eq!(
            rejected.error.as_deref(),
            Some("the prover refused to release the token")
        );
        assert!(rejected.tx.is_none() && rejected.txid.is_none());
        assert_eq!(rejected.inputs, ["22".repeat(32)]);
        let accepted = &journal.entries[1];
        assert_eq!(accepted.txid_to_ask(), Some("bb".repeat(32).as_str()));
        let undecodable = accepted.tx.as_ref().expect("its bytes are described");
        assert!(!undecodable.decoded, "the bytes are not a transaction");

        // Fields the spec does not name, however secret, never reach the page.
        let everything = [
            serde_json::to_string(&files.owner).expect("json"),
            serde_json::to_string(&files.merchant).expect("json"),
            serde_json::to_string(&files.observer).expect("json"),
        ]
        .concat();
        assert!(!everything.contains(SECRET), "{everything}");
    }

    #[test]
    fn a_status_file_that_is_not_the_expected_json_is_an_error_not_a_crash() {
        let dir = tempdir().expect("a directory");
        testing::write_public(dir.path(), "owner-status.json", "{\"wallet\": 7}");
        testing::write_public(dir.path(), "merchant-status.json", "half a fi");
        let files = read_public(dir.path(), &Mutex::new(HashMap::new()));

        for source in [files.owner.state, files.merchant.state] {
            assert_eq!(source, SourceState::Error);
        }
        let message = files.merchant.message.expect("a reason");
        assert!(
            message.starts_with("merchant-status.json is not the expected JSON"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn every_source_down_still_gives_a_state_that_says_so() {
        let dir = tempdir().expect("a directory");
        testing::write_public(dir.path(), "owner-status.json", &testing::owner_status());
        testing::write_public(dir.path(), "journal.jsonl", &testing::journal());
        let app = app(dir.path(), &testing::unused_url(), &testing::unused_url());
        let state = app.state().await.expect("a state");

        assert!(state.generated > 1_696_000_000);
        assert_eq!(state.node.state, SourceState::Error);
        assert!(
            state
                .node
                .message
                .expect("a reason")
                .starts_with("the node is unreachable")
        );
        assert_eq!(state.agent.state, SourceState::Error);
        assert!(
            state
                .agent
                .message
                .expect("a reason")
                .starts_with("the agent is unreachable")
        );
        assert_eq!(state.owner.state, SourceState::Ok);
        assert_eq!(state.merchant.state, SourceState::Missing);
        assert_eq!(state.observer.state, SourceState::Ok);
        // With the node down nothing is claimed about the chain.
        let owner = state.owner.data.expect("the owner snapshot");
        assert!(
            owner.allowances[0]
                .vouchers
                .iter()
                .all(|voucher| voucher.chain.is_none())
        );
        let journal = state.observer.data.expect("the journal");
        assert!(journal.entries.iter().all(|entry| entry.chain.is_none()));
    }

    #[tokio::test]
    async fn the_agent_status_comes_from_the_agent_without_its_extras() {
        let dir = tempdir().expect("a directory");
        let url = testing::serve(testing::fake_agent(Arc::default())).await;
        let app = app(dir.path(), &testing::unused_url(), &url);
        let state = app.state().await.expect("a state");

        let agent = state.agent.data.as_ref().expect("the agent's status");
        assert_eq!(agent.delegate, "dd".repeat(32));
        assert_eq!(agent.tip_height, Some(12));
        assert_eq!(agent.allowances[0].vouchers[0].state, "unspent");
        let text = serde_json::to_string(&state.agent).expect("json");
        assert!(!text.contains(SECRET), "{text}");
    }

    #[tokio::test]
    async fn an_agent_that_answers_with_an_error_is_reported_with_its_status() {
        let dir = tempdir().expect("a directory");
        let broken = Router::new().route("/api/status", get(|| async { StatusCode::BAD_GATEWAY }));
        let url = testing::serve(broken).await;
        let state = app(dir.path(), &testing::unused_url(), &url)
            .state()
            .await
            .expect("a state");

        assert_eq!(state.agent.state, SourceState::Error);
        assert_eq!(
            state.agent.message.as_deref(),
            Some("the agent answered HTTP 502")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_node_says_what_became_of_the_journals_transactions_and_the_vouchers() {
        // An allowance of 50, 20, 20 and 10; the delegate redeems the 50 and the 10.
        let mut rng = rng(7);
        let parties = Parties::new(&mut rng);
        let mut devnet = Devnet::new(&parties.owner);
        let allowance = fund(&mut devnet, &parties, &mut rng);
        let vouchers = &allowance.vouchers;
        let paying = [&vouchers[0], &vouchers[3]];
        let unsigned = build::redemption(&paying).expect("build the redemption");
        let tx = build::sign(unsigned, &[parties.delegate_key]).expect("sign the redemption");
        let proofs = paying
            .iter()
            .map(|voucher| devnet.proof(&voucher.id()))
            .collect();
        let bytes = build::package(tx, proofs).expect("package the redemption");
        let txid = devnet
            .node
            .submit(&bytes)
            .expect("the node admits the redemption");

        // The node serves JSON-RPC, and the dashboard finds it there.
        let Devnet { node, .. } = devnet;
        let node = Arc::new(Mutex::new(node));
        let bind = "127.0.0.1:0".parse().expect("an address");
        let (address, _server) = flamed::serve(Arc::clone(&node), bind).await.expect("serve");

        let ids: Vec<String> = vouchers
            .iter()
            .map(|voucher| hex::encode(voucher.id()))
            .collect();
        let unknown = "ab".repeat(32);
        let dir = tempdir().expect("a directory");
        let vouchers: Vec<_> = ids
            .iter()
            .map(|id| json!({"id": id, "state": "unspent"}))
            .collect();
        let owner = json!({"allowances": [{
            "allowance": "0123456789abcdef",
            "vouchers": vouchers,
            "recovery": [{"txid": unknown, "vouchers": [ids[1], ids[2]], "state": "pending"}],
        }]});
        testing::write_public(dir.path(), "owner-status.json", &owner.to_string());
        // The journal records the redemption, and a second attempt with the same bytes but the
        // wrong transaction ID.
        let redeemed = json!({
            "time": 300, "actor": "agent", "action": "redeem", "txid": hex::encode(txid.0),
            "tx": hex::encode(&bytes), "inputs": [ids[0], ids[3]], "stage": "node",
            "outcome": "accepted",
        });
        let misrecorded = json!({
            "time": 250, "actor": "agent", "action": "redeem", "txid": "cd".repeat(32),
            "tx": hex::encode(&bytes), "inputs": [], "stage": "node", "outcome": "unknown",
        });
        let journal = format!("{redeemed}\n{misrecorded}\n");
        testing::write_public(dir.path(), "journal.jsonl", &journal);
        // The agent reports the same vouchers.
        let held: Vec<_> = ids
            .iter()
            .map(|id| json!({"id": id, "qty": "1", "state": "unspent", "txid": null}))
            .collect();
        let status = json!({"delegate": "dd".repeat(32), "allowances": [{
            "allowance": "0123456789abcdef", "merchant": "tf1merchant", "total": "100",
            "vouchers": held,
        }]});
        let agent = Router::new().route(
            "/api/status",
            get(move || {
                let status = status.clone();
                async move { Json(status) }
            }),
        );
        let agent = testing::serve(agent).await;
        let app = app(dir.path(), &format!("http://{address}"), &agent);

        // Before a block: the transaction waits in the mempool and every voucher is unspent.
        let state = app.state().await.expect("a state");
        assert_eq!(state.node.data.as_ref().expect("the tip").height, 1);
        let entry = &state.observer.data.as_ref().expect("the journal").entries[0];
        assert_eq!(entry.chain, Some(TxChain::Mempool));
        let view = entry.tx.as_ref().expect("the transaction is described");
        assert!(view.verified, "{:?}", view.verification_error);
        assert_eq!(entry.txid_matches, Some(true));
        let owner = state.owner.data.as_ref().expect("the owner snapshot");
        for voucher in &owner.allowances[0].vouchers {
            assert_eq!(voucher.chain, Some(ContractChain::Unspent));
        }
        let agent = state.agent.data.as_ref().expect("the agent's status");
        for voucher in &agent.allowances[0].vouchers {
            assert_eq!(voucher.chain, Some(ContractChain::Unspent));
        }

        // After one: it is confirmed, the two vouchers are spent by it, the others are not, and
        // a recovery the node never saw is unknown, not pending forever.
        node.lock()
            .expect("not poisoned")
            .mint_block()
            .expect("mint a block");
        let state = app.state().await.expect("a state");
        assert_eq!(state.node.data.as_ref().expect("the tip").height, 2);
        let entry = &state.observer.data.as_ref().expect("the journal").entries[0];
        assert_eq!(entry.chain, Some(TxChain::Confirmed { height: 2 }));
        let owner = state.owner.data.as_ref().expect("the owner snapshot");
        let chains: Vec<_> = owner.allowances[0]
            .vouchers
            .iter()
            .map(|v| v.chain.clone())
            .collect();
        let spent = Some(ContractChain::Spent {
            height: 2,
            txid: hex::encode(txid.0),
        });
        assert_eq!(
            chains,
            [
                spent.clone(),
                Some(ContractChain::Unspent),
                Some(ContractChain::Unspent),
                spent
            ]
        );
        assert_eq!(
            owner.allowances[0].recovery[0].chain,
            Some(TxChain::Unknown)
        );
        let agent = state.agent.data.as_ref().expect("the agent's status");
        let chains: Vec<_> = agent.allowances[0]
            .vouchers
            .iter()
            .map(|v| v.chain.clone())
            .collect();
        assert_eq!(chains[0], chains[3]);
        assert!(matches!(
            chains[0],
            Some(ContractChain::Spent { height: 2, .. })
        ));
        assert_eq!(chains[1], Some(ContractChain::Unspent));

        // The attempt whose recorded ID is not the bytes' own is flagged, and the node, asked
        // about the ID it recorded, has never heard of it.
        let journal = state.observer.data.as_ref().expect("the journal");
        let entry = &journal.entries[1];
        assert_eq!(entry.txid_matches, Some(false));
        assert_eq!(entry.chain, Some(TxChain::Unknown));
    }
}
