//! A node in this process, an owner wallet it funds, and a block producer for `--wait`.

use std::sync::Arc;
use std::time::Duration;

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar::Scalar;
use firebreak_core::devnet::LocalNode;
use firebreak_core::journal::{self, JournalEntry};
use firebreak_core::store::{self, DelegationPackage, OwnerStatus};
use firebreak_core::{Chain, NETWORK, Voucher, build, keys};
use firebreak_owner::{Context, CreateAllowance, Files, init};
use flamed::config::ChainParamsFile;
use flamekd::{ReceivingAddress, util};
use flamepayments::Account;
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
    pub ctx: Context,
    pub merchant: ReceivingAddress,
    pub delegate_key: Scalar,
    miner: Option<JoinHandle<()>>,
}

impl Harness {
    /// A fresh owner wallet, a node whose genesis funds it, and a merchant and a delegate. With
    /// `mining`, a block is minted every 200 ms; without, only [`Harness::mint`] mints.
    pub async fn start(mining: bool) -> Harness {
        let dir = TempDir::new().expect("a temporary directory");
        init(&Files::new(dir.path().to_owned()), GENESIS).expect("init the owner");
        Harness::adopt(dir, mining).await
    }

    /// A node that starts from the network definition `init` wrote in `dir`, whose owner is
    /// already created, and a merchant and a delegate.
    pub async fn adopt(dir: TempDir, mining: bool) -> Harness {
        let params = ChainParamsFile::load(&Files::new(dir.path().to_owned()).chainparams())
            .expect("the network definition");
        let allocation = &params.genesis[0];
        let address = allocation.address.as_deref().expect("a genesis address");
        let node = Arc::new(LocalNode::start(address, allocation.qty_sparks).await);
        let chain = Chain::connect(&node.url()).expect("a client");
        let merchant = Account::from_seed(&[9; 64], NETWORK, 0)
            .expect("an account")
            .address_at(util::RECEIVING, 0)
            .expect("an address");
        let mut harness = Harness {
            ctx: Context::new(dir.path().to_owned(), chain),
            dir,
            node,
            merchant,
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

    /// The request to fund vouchers of `amounts` for the merchant and the delegate.
    pub fn request(&self, amounts: &[u64], wait: bool) -> CreateAllowance {
        CreateAllowance {
            merchant: self.merchant,
            delegate: self.delegate(),
            vouchers: amounts.to_vec(),
            wait,
        }
    }

    /// The delegate's verification key.
    pub fn delegate(&self) -> CompressedRistretto {
        keys::verification_key(&self.delegate_key)
    }

    /// The owner's files.
    pub fn files(&self) -> &Files {
        &self.ctx.files
    }

    /// The vouchers of the delegation package that was written for `allowance`.
    pub fn vouchers(&self, allowance: &str) -> Vec<Voucher> {
        let package: DelegationPackage =
            store::read(&self.files().package(allowance)).expect("the delegation package");
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

    /// Waits until `txid` is in a block, which a minter has to mint.
    pub async fn confirmed(&self, txid: &TxID) -> u64 {
        self.ctx
            .chain
            .wait_confirmed(txid, PATIENCE)
            .await
            .expect("the transaction confirms")
    }

    /// Every line of the public journal.
    pub fn journal(&self) -> Vec<JournalEntry> {
        let journal = journal::read_all(&self.files().journal()).expect("the journal");
        assert_eq!(journal.skipped, 0, "every journal line is an entry");
        journal.entries
    }

    /// The snapshot in `owner-status.json`.
    pub fn snapshot(&self) -> OwnerStatus {
        store::read(&self.files().status()).expect("the owner's snapshot")
    }

    /// Stops the block producer and the node.
    pub async fn finish(mut self) {
        if let Some(miner) = self.miner.take() {
            miner.abort();
            // The producer is aborted at an await point, so it holds no lock on the node.
            let _ = miner.await;
        }
        drop(self.ctx);
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
