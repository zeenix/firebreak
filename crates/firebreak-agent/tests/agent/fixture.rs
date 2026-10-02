//! A local node, an owner who funds an allowance for the agent, and the agent that imports it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use firebreak_agent::{Agent, Files, PayRequest, import};
use firebreak_core::chain::Chain;
use firebreak_core::devnet::{DENOMINATIONS, GENESIS_SPARKS, LocalNode, Parties, output, rng};
use firebreak_core::store::{self, AgentStore, DelegationPackage, VoucherState};
use firebreak_core::{NETWORK, Payout, Voucher, VoucherPolicy, build, keys, voucher, wallet};
use flamekd::util;
use flamepayments::{Opening, OutputSpec, PreparedOutput, prepare_output};
use flamevm::{Anchor, Contract, Predicate, TxID};
use rand::rngs::StdRng;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// The memo of every voucher's receipt.
const MEMO: &[u8] = b"firebreak voucher";

/// How often the background miner mints a block.
const BLOCK_INTERVAL: Duration = Duration::from_millis(200);

/// How long the fixture waits for its own funding to confirm.
const PATIENCE: Duration = Duration::from_secs(30);

/// A local node with the owner's genesis allocation, a block miner and a data directory.
pub struct Devnet {
    pub node: Arc<LocalNode>,
    pub chain: Chain,
    pub parties: Parties,
    pub rng: StdRng,
    pub dir: TempDir,
    miner: Option<JoinHandle<()>>,
}

impl Devnet {
    /// Starts the node. With `mining` a task mints a block every 200 ms, and otherwise blocks
    /// come only when a test calls [`LocalNode::mint`].
    pub async fn start(mining: bool) -> Devnet {
        let mut rng = rng(1);
        let parties = Parties::new(&mut rng);
        let owner = parties
            .owner
            .address_at(util::RECEIVING, 0)
            .expect("the owner's address");
        let node = Arc::new(LocalNode::start(&owner.to_bech32(NETWORK), GENESIS_SPARKS).await);
        let chain = Chain::connect(&node.url()).expect("a client");
        let miner = mining.then(|| mine(Arc::clone(&node)));
        Devnet {
            node,
            chain,
            parties,
            rng,
            dir: tempfile::tempdir().expect("a data directory"),
            miner,
        }
    }

    /// Funds one voucher per denomination for the agent whose records are `delegate`, out of the
    /// owner's one unspent output, with the rest as change to the owner's change address
    /// `change_index`. The funding is confirmed when this returns.
    pub async fn fund(
        &mut self,
        delegate: &AgentStore,
        denominations: &[u64],
        change_index: u32,
    ) -> Funded {
        let merchant = self.parties.merchant_address();
        let synced = wallet::sync(&self.chain, &self.parties.owner, 0..1, 0..change_index + 1)
            .await
            .expect("sync the owner");
        let source = synced
            .outputs
            .iter()
            .find(|output| output.spent.is_none())
            .expect("the owner has an unspent output");
        let policies: Vec<VoucherPolicy> = denominations
            .iter()
            .map(|_| VoucherPolicy {
                merchant,
                delegate: delegate.public(),
                owner: keys::verification_key(&self.parties.owner_key),
                blinding: keys::random_bytes(&mut self.rng),
            })
            .collect();
        let prepared: Vec<PreparedOutput> = denominations
            .iter()
            .map(|qty| {
                let spec = OutputSpec {
                    memo: MEMO.to_vec(),
                    ..output(merchant, *qty)
                };
                prepare_output(&spec, &mut self.rng).expect("prepare a voucher")
            })
            .collect();
        let total: u64 = denominations.iter().sum();
        let change_address = self
            .parties
            .owner
            .address_at(util::CHANGE, change_index)
            .expect("the change address");
        let change = prepare_output(&output(change_address, source.qty - total), &mut self.rng)
            .expect("prepare the change");
        let mut payouts: Vec<Payout<'_>> = policies
            .iter()
            .zip(&prepared)
            .map(|(policy, prepared)| Payout::Voucher { policy, prepared })
            .collect();
        payouts.push(Payout::Wallet {
            to: change_address.spending_key().compress(),
            prepared: &change,
        });

        let proofs = self
            .chain
            .fresh_proofs(&[source.id])
            .await
            .expect("a proof of the owner's output");
        let input = source
            .to_input(&self.parties.owner, proofs[0].clone())
            .expect("the owner's output as an input");
        let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("funding");
        let created = build::outputs(unsigned.log());
        let vouchers: Vec<Voucher> = policies
            .iter()
            .zip(denominations)
            .map(|(policy, qty)| {
                let predicate = policy.predicate().expect("a predicate");
                let (contract, _) = created
                    .iter()
                    .find(|(contract, _)| contract.predicate.to_point() == predicate)
                    .expect("the funding creates the voucher");
                Voucher::new(*policy, contract.clone(), *qty).expect("a voucher")
            })
            .collect();
        let txid = unsigned.log().txid();
        let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign the funding");
        let bytes = build::package(tx, vec![input.proof().clone()]).expect("package the funding");
        self.chain
            .submit(bytes)
            .await
            .expect("the node admits the funding");
        self.node.mint();
        self.chain
            .wait_confirmed(&txid, PATIENCE)
            .await
            .expect("the funding confirms");
        Funded {
            txid,
            vouchers,
            openings: prepared.iter().map(|prepared| prepared.opening).collect(),
        }
    }

    /// The `tf1...` address of the merchant that every allowance pays.
    pub fn merchant(&self) -> String {
        self.parties.merchant_address().to_bech32(NETWORK)
    }
}

impl Drop for Devnet {
    fn drop(&mut self) {
        if let Some(miner) = &self.miner {
            miner.abort();
        }
    }
}

/// The vouchers of a funded allowance, with what the owner kept to recover them.
pub struct Funded {
    pub txid: TxID,
    pub vouchers: Vec<Voucher>,
    pub openings: Vec<Opening>,
}

impl Funded {
    /// The delegation package the owner hands to the agent.
    pub fn package(&self) -> DelegationPackage {
        DelegationPackage::from_vouchers(self.txid, &self.vouchers).expect("a package")
    }

    /// The allowance's id.
    pub fn allowance(&self) -> String {
        self.package().allowance
    }

    /// The ids of the vouchers, in the order they were funded.
    pub fn ids(&self) -> Vec<[u8; 32]> {
        self.vouchers.iter().map(Voucher::id).collect()
    }

    /// Writes the package under `data_dir/public`, as the owner does, and gives its path.
    pub fn write_package(&self, data_dir: &Path) -> PathBuf {
        let package = self.package();
        let path = data_dir
            .join("public")
            .join(format!("allowance-{}.json", package.allowance));
        store::write_public(&path, &package).expect("write the package");
        path
    }
}

/// A node, an agent with a store of its own, and an allowance of 50, 20, 20 and 10 that the owner
/// funded for it and the agent imported. The agent has reconciled, so every voucher is unspent.
pub struct World {
    pub net: Devnet,
    pub files: Files,
    pub agent: Arc<Agent>,
    pub funded: Funded,
}

impl World {
    /// A world in which a miner mints a block every 200 ms.
    pub async fn start() -> World {
        World::new(true).await
    }

    /// A world in which blocks come only when the test mints them.
    pub async fn start_manual() -> World {
        World::new(false).await
    }

    async fn new(mining: bool) -> World {
        let mut net = Devnet::start(mining).await;
        let files = Files::new(net.dir.path().to_path_buf());
        files.init().await.expect("initialise the agent");
        let records = files.read().await.expect("the agent's store");
        let funded = net.fund(&records, &DENOMINATIONS, 0).await;
        let path = funded.write_package(net.dir.path());
        import::packages(&files, vec![path])
            .await
            .expect("import the package");
        let agent = Arc::new(Agent::new(files.clone(), net.chain.clone()));
        agent.status().await.expect("reconcile");
        World {
            net,
            files,
            agent,
            funded,
        }
    }

    /// Funds a second allowance of `denominations` for the same agent and imports it.
    pub async fn fund_another(&mut self, denominations: &[u64]) -> Funded {
        let records = self.files.read().await.expect("the agent's store");
        let funded = self.net.fund(&records, denominations, 1).await;
        let path = funded.write_package(self.net.dir.path());
        import::packages(&self.files, vec![path])
            .await
            .expect("import the package");
        funded
    }

    /// A request to pay `amount` sparks to the allowance's merchant, leaving the allowance out.
    pub fn request(&self, amount: u64) -> PayRequest {
        PayRequest {
            allowance: None,
            merchant: self.net.merchant(),
            amount,
        }
    }

    /// What the agent has recorded of each voucher of the first allowance, in funding order:
    /// the face value, the state and the redemption.
    pub async fn recorded(&self) -> Vec<(u64, VoucherState, Option<TxID>)> {
        let records = self.files.read().await.expect("the agent's store");
        records.allowances[0]
            .vouchers
            .iter()
            .map(|voucher| (voucher.qty, voucher.state, voucher.txid))
            .collect()
    }
}

/// The delegation package for vouchers worth `quantities` that exist only on paper: they are
/// correct contracts for `delegate`, but no node ever saw them. For tests of the files.
pub fn paper_package(delegate: &AgentStore, quantities: &[u64], seed: u8) -> DelegationPackage {
    let mut rng = rng(u64::from(seed));
    let parties = Parties::new(&mut rng);
    let vouchers: Vec<Voucher> = quantities
        .iter()
        .enumerate()
        .map(|(index, qty)| {
            let policy = VoucherPolicy {
                merchant: parties.merchant_address(),
                delegate: delegate.public(),
                owner: keys::verification_key(&parties.owner_key),
                blinding: keys::random_bytes(&mut rng),
            };
            let spec = OutputSpec {
                memo: MEMO.to_vec(),
                ..output(policy.merchant, *qty)
            };
            let prepared = prepare_output(&spec, &mut rng).expect("prepare a voucher");
            let contract = Contract::new(
                Predicate::opaque(policy.predicate().expect("a predicate")),
                Anchor([seed.wrapping_add(index as u8); 32]),
                voucher::payload(prepared.token, prepared.note),
            )
            .expect("a portable payload");
            Voucher::new(policy, contract, *qty).expect("a voucher")
        })
        .collect();
    DelegationPackage::from_vouchers(TxID([seed; 32]), &vouchers).expect("a package")
}

/// Mints a block every [`BLOCK_INTERVAL`], for as long as the task lives.
fn mine(node: Arc<LocalNode>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(BLOCK_INTERVAL).await;
            node.mint();
        }
    })
}
