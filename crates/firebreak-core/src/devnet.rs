//! An in-process Flame devnet with fresh parties and a funded allowance, for tests.
//!
//! Every transaction submitted here goes through the node's own admission path: the bytes are
//! decoded under the chain's limits, re-executed, verified, and checked against the UTXO set.
//! Blocks are minted on demand, so a test decides exactly what each block holds. Being test
//! scaffolding, its helpers panic on anything unexpected.

use curve25519_dalek::scalar::Scalar as DalekScalar;
use flamechain::utreexo::Proof;
use flamechain::{BlockTx, ChainParams, codec::contract_from_bytes};
use flamed::config::{ChainParamsFile, GenesisFile, GenesisSpec, NetworkName, NodeConfig};
use flamed::{Node, NodeError, ProofStatus, TxStatus};
use flamekd::{Network, ReceivingAddress, util};
use flamepayments::{Account, Opening, OutputSpec, PreparedOutput, open_note, prepare_output};
use flamevm::{Contract, ExternalTx, FLAME_FLAVOR, TxID, UnsignedTx};
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::{Payout, Voucher, VoucherPolicy, WalletInput, build, keys};

/// What the owner's wallet holds at genesis, in sparks.
pub const GENESIS_SPARKS: u64 = 1_000;
/// The allowance's vouchers, in sparks.
pub const DENOMINATIONS: [u64; 4] = [50, 20, 20, 10];

/// A seeded generator, so a failing test replays exactly.
pub fn rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

/// The three parties, with fresh keys.
pub struct Parties {
    /// The owner's wallet, which holds the genesis allocation.
    pub owner: Account,
    /// The owner's voucher authority: the key of every voucher's recovery branch.
    pub owner_key: DalekScalar,
    /// The delegated key: the key of every voucher's redemption branch.
    pub delegate_key: DalekScalar,
    /// The merchant's wallet.
    pub merchant: Account,
}

impl Parties {
    pub fn new(rng: &mut StdRng) -> Parties {
        let account = |rng: &mut StdRng| {
            Account::from_seed(&keys::random_bytes::<64, _>(rng), Network::Testnet, 0)
                .expect("a 64-byte seed derives an account")
        };
        Parties {
            owner: account(rng),
            owner_key: keys::generate(rng),
            delegate_key: keys::generate(rng),
            merchant: account(rng),
        }
    }

    /// The merchant address every voucher pays.
    pub fn merchant_address(&self) -> ReceivingAddress {
        self.merchant
            .address_at(util::RECEIVING, 0)
            .expect("merchant address")
    }

    /// A fresh voucher policy for the merchant.
    pub fn policy(&self, rng: &mut StdRng) -> VoucherPolicy {
        VoucherPolicy {
            merchant: self.merchant_address(),
            delegate: keys::verification_key(&self.delegate_key),
            owner: keys::verification_key(&self.owner_key),
            blinding: keys::random_bytes(rng),
        }
    }
}

/// A native-flavor output of `qty` to `address`.
pub fn output(address: ReceivingAddress, qty: u64) -> OutputSpec {
    OutputSpec {
        address,
        qty,
        flv: FLAME_FLAVOR,
        memo: Vec::new(),
    }
}

/// A devnet whose genesis gives the owner's first receiving address the whole supply.
pub struct Devnet {
    pub node: Node,
    /// The genesis allocation.
    pub genesis: Contract,
    _dir: tempfile::TempDir,
}

impl Devnet {
    pub fn new(owner: &Account) -> Devnet {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let address = owner
            .address_at(util::RECEIVING, 0)
            .expect("owner address")
            .to_bech32(Network::Testnet);
        let chainparams = ChainParamsFile {
            version: 1,
            network: NetworkName::Testnet,
            storage: Default::default(),
            limits: Default::default(),
            genesis: vec![GenesisSpec {
                address: Some(address),
                predicate: None,
                qty_sparks: GENESIS_SPARKS,
            }],
        };
        let path = dir.path().join("genesis.json");
        flamed::genesis::write(&chainparams, &path).expect("derive genesis.json");
        let genesis = GenesisFile::load(&path).expect("read genesis.json");
        let cfg = NodeConfig {
            data_dir: dir.path().to_path_buf(),
            genesis: Some(path),
            rpc_bind: "127.0.0.1:0".parse().expect("a socket address"),
            block_interval_secs: 15,
            minimum_fee: 0,
        };
        let node = Node::open(&genesis, &cfg).expect("open the node");
        let genesis =
            contract_from_bytes(&genesis.contracts[0].bytes.0).expect("the genesis contract");
        Devnet {
            node,
            genesis,
            _dir: dir,
        }
    }

    /// The proof of an unspent contract at the current tip.
    pub fn proof(&self, id: &[u8; 32]) -> Proof {
        match self.node.proof(id) {
            ProofStatus::Unspent(proof) => proof,
            other => panic!("expected an unspent contract, got {other:?}"),
        }
    }

    /// Whether the node holds `id` as unspent.
    pub fn is_unspent(&self, id: &[u8; 32]) -> bool {
        matches!(self.node.proof(id), ProofStatus::Unspent(_))
    }

    /// Whether the node records `id` as spent.
    pub fn is_spent(&self, id: &[u8; 32]) -> bool {
        matches!(self.node.proof(id), ProofStatus::Spent { .. })
    }

    /// The contract `id` as the node published it.
    pub fn published(&self, id: &[u8; 32]) -> Contract {
        let record = self.node.contract(id).expect("a known contract");
        contract_from_bytes(&record.contract).expect("a contract")
    }

    /// Serializes `tx` with its proofs, decodes and verifies the bytes the way the chain will,
    /// then offers them to the mempool.
    pub fn submit(&mut self, tx: ExternalTx, proofs: Vec<Proof>) -> Result<TxID, NodeError> {
        let bytes = build::package(tx, proofs).expect("package the transaction");
        let params = ChainParams::default();
        let decoded = BlockTx::from_bytes_bounded(&bytes, params.version, params.limits)
            .expect("the packaged bytes decode");
        decoded
            .tx
            .verify(build::LIMITS)
            .expect("the decoded transaction verifies");
        self.node.submit(&bytes)
    }

    /// Mints a block and checks that it confirmed `txid`.
    pub fn confirm(&mut self, txid: TxID) {
        self.node.mint_block().expect("mint a block");
        assert!(
            matches!(self.node.tx_status(&txid), TxStatus::Confirmed { .. }),
            "the transaction confirmed"
        );
    }

    /// The tokens under `address`'s spending key, opened with its viewing key, and whether each
    /// is spent.
    pub fn received(
        &self,
        account: &Account,
        branch: u32,
        n: u32,
    ) -> Vec<(Contract, Opening, bool)> {
        let address = account.address_at(branch, n).expect("address");
        let view_key = account.viewing_key_at(branch, n).expect("view key");
        self.node
            .scan(&[address.spending_key().compress().to_bytes()], 0)
            .into_iter()
            .map(|hit| {
                let contract = contract_from_bytes(&hit.bytes.0).expect("a contract");
                let note = open_note(
                    &contract,
                    hit.note.as_ref().map(|note| note.0.as_slice()),
                    &address,
                    &view_key,
                )
                .expect("the note opens");
                (contract, note.opening, hit.spent.is_some())
            })
            .collect()
    }
}

/// A funded allowance: its vouchers as published, and what the owner kept to recover them.
pub struct Allowance {
    pub vouchers: Vec<Voucher>,
    pub openings: Vec<Opening>,
    /// The owner's change output.
    pub change: Contract,
}

/// Funds one voucher per denomination from the genesis allocation, with the rest as change to
/// the owner's first change address, and confirms it.
pub fn fund(devnet: &mut Devnet, parties: &Parties, rng: &mut StdRng) -> Allowance {
    let merchant = parties.merchant_address();
    let policies: Vec<VoucherPolicy> = DENOMINATIONS.iter().map(|_| parties.policy(rng)).collect();
    let prepared: Vec<PreparedOutput> = DENOMINATIONS
        .iter()
        .map(|qty| prepare_output(&output(merchant, *qty), rng).expect("prepare a voucher"))
        .collect();
    let allowance: u64 = DENOMINATIONS.iter().sum();
    let change_address = parties
        .owner
        .address_at(util::CHANGE, 0)
        .expect("change address");
    let change = prepare_output(&output(change_address, GENESIS_SPARKS - allowance), rng)
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
    let input = WalletInput::clear(
        devnet.genesis.clone(),
        devnet.proof(&devnet.genesis.id()),
        parties
            .owner
            .spending_key_at(util::RECEIVING, 0)
            .expect("owner key"),
    )
    .expect("a clear input");
    let unsigned = build::funding(std::slice::from_ref(&input), &payouts, 0).expect("funding");
    let (vouchers, change) = funded(&unsigned, &policies, &change_address);

    let tx = build::sign(unsigned, &[input.signing_key()]).expect("sign the funding");
    let txid = devnet
        .submit(tx, vec![input.proof().clone()])
        .expect("the node admits the funding");
    devnet.confirm(txid);

    // From here on, every voucher is the one the node published.
    let vouchers = vouchers
        .into_iter()
        .map(|voucher| {
            let published = devnet.published(&voucher.id());
            Voucher::new(voucher.policy, published, voucher.qty).expect("a published voucher")
        })
        .collect();
    Allowance {
        vouchers,
        openings: prepared.iter().map(|prepared| prepared.opening).collect(),
        change,
    }
}

/// The vouchers and the change a funding transaction creates, read from its effect log.
fn funded(
    unsigned: &UnsignedTx,
    policies: &[VoucherPolicy],
    change: &ReceivingAddress,
) -> (Vec<Voucher>, Contract) {
    let created = build::outputs(unsigned.log());
    let vouchers = policies
        .iter()
        .zip(DENOMINATIONS)
        .map(|(policy, qty)| {
            let predicate = policy.predicate().expect("a predicate");
            let (contract, data) = created
                .iter()
                .find(|(contract, _)| contract.predicate.to_point() == predicate)
                .expect("the funding creates the voucher");
            assert!(data.is_none(), "nothing is logged after a voucher");
            Voucher::new(*policy, contract.clone(), qty).expect("a voucher")
        })
        .collect();
    let change = created
        .iter()
        .find(|(contract, _)| contract.predicate.to_point() == change.spending_key().compress())
        .map(|(contract, _)| contract.clone())
        .expect("the funding creates the change");
    (vouchers, change)
}
