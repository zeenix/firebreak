//! A node in this process, an owner who funds allowances for the merchant, a delegate who redeems
//! them, and a block producer for `--wait`.

use std::sync::Arc;
use std::time::Duration;

use curve25519_dalek::scalar::Scalar;
use firebreak_core::devnet::LocalNode;
use firebreak_core::journal::{self, Actor, JournalEntry};
use firebreak_core::store::{self, DelegationPackage, MerchantStatus};
use firebreak_core::{Chain, NETWORK, Voucher, build, keys};
use firebreak_merchant::{Context, Files, init};
use firebreak_owner::{AllowanceReport, CreateAllowance, create_allowance};
use flamekd::ReceivingAddress;
use flamevm::TxID;
use rand::rngs::OsRng;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};

/// What the genesis gives the owner, in sparks.
pub const GENESIS: u64 = 1_000;

/// How long a test waits for a block it expects.
pub const PATIENCE: Duration = Duration::from_secs(20);

/// An owner, a merchant and a delegate on a fresh devnet.
pub struct Harness {
    pub dir: TempDir,
    pub node: Arc<LocalNode>,
    /// The merchant's context.
    pub ctx: Context,
    /// The owner's context, which funds allowances for the merchant.
    pub owner: firebreak_owner::Context,
    pub delegate_key: Scalar,
    miner: Option<JoinHandle<()>>,
}

impl Harness {
    /// A fresh merchant and owner, and a node whose genesis funds the owner. With `mining`, a
    /// block is minted every 200 ms; without, only [`Harness::mint`] mints.
    pub async fn start(mining: bool) -> Harness {
        let dir = TempDir::new().expect("a temporary directory");
        let owner =
            firebreak_owner::init(&firebreak_owner::Files::new(dir.path().to_owned()), GENESIS)
                .expect("init the owner");
        init(&Files::new(dir.path().to_owned())).expect("init the merchant");
        let node =
            Arc::new(LocalNode::start(&owner.genesis_address.to_bech32(NETWORK), GENESIS).await);
        let client = || Chain::connect(&node.url()).expect("a client");
        let mut harness = Harness {
            ctx: Context::new(dir.path().to_owned(), client()),
            owner: firebreak_owner::Context::new(dir.path().to_owned(), client()),
            dir,
            node,
            delegate_key: keys::generate(&mut OsRng),
            miner: None,
        };
        if mining {
            harness.start_mining();
        }
        harness
    }

    /// Starts minting a block every 200 ms, as a devnet's minter does.
    pub fn start_mining(&mut self) {
        let node = Arc::clone(&self.node);
        self.miner = Some(tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(200));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let node = Arc::clone(&node);
                // A block that holds transactions takes a while to connect.
                tokio::task::spawn_blocking(move || node.mint())
                    .await
                    .expect("the minter");
            }
        }));
    }

    /// Mints one block now, and returns its height.
    pub async fn mint(&self) -> u64 {
        let node = Arc::clone(&self.node);
        tokio::task::spawn_blocking(move || node.mint())
            .await
            .expect("mint a block")
    }

    /// A context for the same files whose node is not there: nothing listens on port 1.
    pub fn dead(&self) -> Context {
        let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
        Context::new(self.dir.path().to_owned(), chain)
    }

    /// The merchant's files.
    pub fn files(&self) -> &Files {
        &self.ctx.files
    }

    /// The merchant's address, as the merchant published it.
    pub fn address(&self) -> ReceivingAddress {
        let text = store::read_line(&self.files().address()).expect("the published address");
        ReceivingAddress::from_bech32(&text, NETWORK).expect("an address")
    }

    /// The owner funds an allowance of `amounts` for the merchant, and it is in a block.
    pub async fn fund(&self, amounts: &[u64]) -> AllowanceReport {
        let request = CreateAllowance {
            merchant: self.address(),
            delegate: keys::verification_key(&self.delegate_key),
            vouchers: amounts.to_vec(),
            wait: self.miner.is_some(),
        };
        let report = create_allowance(&self.owner, &request)
            .await
            .expect("fund the allowance");
        if self.miner.is_none() {
            self.mint().await;
        }
        report
    }

    /// The vouchers of the delegation package that was written for `allowance`.
    pub fn vouchers(&self, allowance: &str) -> Vec<Voucher> {
        let path = self
            .dir
            .path()
            .join("public")
            .join(format!("allowance-{allowance}.json"));
        let package: DelegationPackage = store::read(&path).expect("the delegation package");
        package.vouchers().expect("the package's vouchers")
    }

    /// The delegate redeems `vouchers` to the merchant, and the node takes the transaction into
    /// its mempool.
    pub async fn redeem(&self, vouchers: &[&Voucher]) -> TxID {
        let unsigned = build::redemption(vouchers).expect("build the redemption");
        let tx = build::sign(unsigned, &[self.delegate_key]).expect("sign the redemption");
        let ids: Vec<[u8; 32]> = vouchers.iter().map(|voucher| voucher.id()).collect();
        let proofs = self.ctx.chain.fresh_proofs(&ids).await.expect("proofs");
        let bytes = build::package(tx, proofs).expect("package the redemption");
        self.ctx
            .chain
            .submit(bytes)
            .await
            .expect("the node admits the redemption")
    }

    /// The delegate redeems `vouchers` to the merchant, and the redemption is in a block.
    pub async fn pay(&self, vouchers: &[&Voucher]) -> TxID {
        let txid = self.redeem(vouchers).await;
        if self.miner.is_none() {
            self.mint().await;
        }
        self.confirmed(&txid).await;
        txid
    }

    /// Waits until `txid` is in a block, which a minter has to mint.
    pub async fn confirmed(&self, txid: &TxID) -> u64 {
        self.ctx
            .chain
            .wait_confirmed(txid, PATIENCE)
            .await
            .expect("the transaction confirms")
    }

    /// The merchant's lines of the public journal, which the owner's funding lines share.
    pub fn journal(&self) -> Vec<JournalEntry> {
        let journal = journal::read_all(&self.files().journal()).expect("the journal");
        assert_eq!(journal.skipped, 0, "every journal line is an entry");
        journal
            .entries
            .into_iter()
            .filter(|entry| entry.actor == Actor::Merchant)
            .collect()
    }

    /// The snapshot in `merchant-status.json`.
    pub fn snapshot(&self) -> MerchantStatus {
        store::read(&self.files().status()).expect("the merchant's snapshot")
    }

    /// Stops the block producer and the node.
    pub async fn finish(mut self) {
        if let Some(miner) = self.miner.take() {
            miner.abort();
            // The producer is aborted at an await point, so it holds no lock on the node.
            let _ = miner.await;
        }
        drop(self.ctx);
        drop(self.owner);
        // A block that was being minted when the producer stopped still holds the node.
        let mut node = self.node;
        for _ in 0..100 {
            match Arc::try_unwrap(node) {
                Ok(node) => return node.stop().await,
                Err(shared) => node = shared,
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    }
}
