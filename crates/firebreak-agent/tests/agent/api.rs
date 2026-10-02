//! The HTTP API: what the dashboard reads, what a payment answers, and who may ask for the key.

use std::fs;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::http::header::CACHE_CONTROL;
use firebreak_agent::Agent;
use firebreak_core::NETWORK;
use firebreak_core::chain::{Chain, ContractState, TxState};
use firebreak_core::devnet::{Parties, rng};
use flamevm::TxID;
use serde_json::json;

use crate::fixture::World;
use crate::http::Client;
use crate::proxy::{Fault, Proxy};

/// The transaction a reply names.
fn txid_of(text: &str) -> TxID {
    let bytes: [u8; 32] = hex::decode(text)
        .expect("hex")
        .try_into()
        .expect("32 bytes");
    TxID(bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_status_has_the_shape_the_dashboard_reads_and_never_the_secret() {
    let world = World::start().await;
    let client = Client::serve(Arc::clone(&world.agent), false).await;
    let reply = client.get("/api/status").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[CACHE_CONTROL], "no-store");
    let published = fs::read_to_string(world.files.delegate_key()).expect("the public key");
    let status = &reply.json;
    assert_eq!(status["delegate"], published.trim());
    assert!(status["tip_height"].is_u64(), "{status}");
    assert!(status.get("warning").is_none(), "{status}");
    let [allowance] = status["allowances"]
        .as_array()
        .expect("allowances")
        .as_slice()
    else {
        panic!("expected one allowance in {status}");
    };
    assert_eq!(allowance["allowance"], world.funded.allowance());
    assert_eq!(allowance["merchant"], world.net.merchant());
    assert_eq!(allowance["total"], "100");
    let vouchers = allowance["vouchers"].as_array().expect("vouchers");
    let funded = world.funded.ids().into_iter().zip(["50", "20", "20", "10"]);
    assert_eq!(vouchers.len(), 4);
    for (voucher, (id, qty)) in vouchers.iter().zip(funded) {
        assert_eq!(voucher["id"], hex::encode(id));
        assert_eq!(voucher["qty"], qty);
        assert_eq!(voucher["state"], "unspent");
        assert!(voucher["txid"].is_null(), "{voucher}");
    }

    let secret = hex::encode(
        world
            .files
            .read()
            .await
            .expect("the store")
            .delegate_key
            .to_bytes(),
    );
    assert!(
        !reply.body.contains(&secret),
        "the status holds the secret key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payment_over_http_answers_with_its_transaction_and_a_refusal_with_its_stage() {
    let world = World::start().await;
    let client = Client::serve(Arc::clone(&world.agent), false).await;
    let merchant = world.net.merchant();

    // The allowance may be left out, since only one is imported, and the media type may carry
    // parameters.
    let body = json!({"merchant": merchant, "amount": "60"}).to_string();
    let reply = client
        .post_as("/api/pay", Some("application/json; charset=utf-8"), &body)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let [fifty, _, _, ten]: [[u8; 32]; 4] = world.funded.ids().try_into().expect("four vouchers");
    assert_eq!(
        reply.json["vouchers"],
        json!([hex::encode(fifty), hex::encode(ten)])
    );
    assert_eq!(reply.json["state"], "redeemed");
    assert!(reply.json.get("warnings").is_none(), "{}", reply.body);
    let txid = txid_of(reply.json["txid"].as_str().expect("a transaction id"));
    let confirmed = world
        .net
        .chain
        .tx_state(&txid)
        .await
        .expect("the transaction's state");
    assert!(
        matches!(confirmed, TxState::Confirmed { .. }),
        "{confirmed:?}"
    );
    assert_eq!(reply.json.as_object().expect("an object").len(), 3);

    // A payment that stops says where, with a status the client can act on.
    let elsewhere = Parties::new(&mut rng(5))
        .merchant_address()
        .to_bech32(NETWORK);
    let allowance = world.funded.allowance();
    let refusals = [
        (
            json!({"merchant": merchant, "amount": "110"}),
            400,
            "selection",
            "insufficient authority",
        ),
        (
            json!({"allowance": allowance, "merchant": merchant, "amount": "30"}),
            400,
            "selection",
            "no exact combination of vouchers for 30 (denominations: 20, 20)",
        ),
        (
            json!({"merchant": elsewhere, "amount": "20"}),
            400,
            "policy",
            "refusing to pay",
        ),
        (
            json!({"allowance": "0000000000000000", "merchant": merchant, "amount": "20"}),
            400,
            "input",
            "no allowance 0000000000000000 is imported",
        ),
        (
            json!({"merchant": merchant, "amount": 20}),
            400,
            "input",
            "expected a string",
        ),
        (
            json!({"merchant": merchant, "amount": 20.5}),
            400,
            "input",
            "expected a string",
        ),
        (
            json!({"merchant": merchant, "amount": "20.5"}),
            400,
            "input",
            "plain non-negative",
        ),
        (
            json!({"merchant": merchant, "amount": "-20"}),
            400,
            "input",
            "plain non-negative",
        ),
        (
            json!({"merchant": merchant, "amount": "020"}),
            400,
            "input",
            "plain non-negative",
        ),
        (
            json!({"merchant": merchant, "amount": ""}),
            400,
            "input",
            "plain non-negative",
        ),
        (
            json!({"amount": "20"}),
            400,
            "input",
            "missing field `merchant`",
        ),
        (
            json!({"merchant": merchant, "amount": "20", "fee": "1"}),
            400,
            "input",
            "unknown field",
        ),
    ];
    for (body, status, stage, text) in refusals {
        let reply = client.post("/api/pay", &body).await;
        assert_eq!(reply.status.as_u16(), status, "{body}: {}", reply.body);
        assert_eq!(reply.json["stage"], stage, "{body}: {}", reply.body);
        let error = reply.json["error"].as_str().expect("an error message");
        assert!(error.contains(text), "{body}: {error}");
    }
    let reply = client
        .post_as("/api/pay", Some("application/json"), "not json")
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json["stage"], "input");

    // A page of another origin can send a body without a preflight only as a form or as plain
    // text, so anything but JSON is refused.
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let reply = client.post_as("/api/pay", content_type, &body).await;
        assert_eq!(
            reply.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
        assert_eq!(reply.json["stage"], "input");
    }
    // None of it paid anything else.
    let spent = world
        .net
        .chain
        .states(&world.funded.ids())
        .await
        .expect("states");
    let still_unspent = spent
        .iter()
        .filter(|s| matches!(s, ContractState::Unspent(_)))
        .count();
    assert_eq!(still_unspent, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_payments_never_reserve_the_same_voucher() {
    let world = World::start().await;
    let client = Arc::new(Client::serve(Arc::clone(&world.agent), false).await);
    let body = json!({"merchant": world.net.merchant(), "amount": "20"});

    // Three requests for 20 at once, and there are two vouchers of 20.
    let requests: Vec<_> = (0..3)
        .map(|_| {
            let (client, body) = (Arc::clone(&client), body.clone());
            tokio::spawn(async move { client.post("/api/pay", &body).await })
        })
        .collect();
    let mut replies = Vec::new();
    for request in requests {
        replies.push(request.await.expect("a reply"));
    }
    let (paid, refused): (Vec<_>, Vec<_>) = replies
        .iter()
        .partition(|reply| reply.status == StatusCode::OK);
    assert_eq!(
        (paid.len(), refused.len()),
        (2, 1),
        "{replies:?}",
        replies = bodies(&replies)
    );

    // The two redeem different vouchers, the 20s, in different transactions, and the chain has
    // confirmed both.
    let [_, twenty, other_twenty, _]: [[u8; 32]; 4] =
        world.funded.ids().try_into().expect("four vouchers");
    let mut redeemed: Vec<String> = paid
        .iter()
        .map(|reply| {
            reply.json["vouchers"][0]
                .as_str()
                .expect("a voucher")
                .to_owned()
        })
        .collect();
    redeemed.sort();
    let mut twenties = vec![hex::encode(twenty), hex::encode(other_twenty)];
    twenties.sort();
    assert_eq!(redeemed, twenties);
    for reply in &paid {
        assert_eq!(reply.json["state"], "redeemed", "{}", reply.body);
    }
    let txids: Vec<TxID> = paid
        .iter()
        .map(|reply| txid_of(reply.json["txid"].as_str().expect("a transaction id")))
        .collect();
    assert_ne!(txids[0], txids[1]);
    for txid in &txids {
        let state = world
            .net
            .chain
            .tx_state(txid)
            .await
            .expect("the transaction's state");
        assert!(matches!(state, TxState::Confirmed { .. }), "{state:?}");
    }
    let states = world
        .net
        .chain
        .states(&[twenty, other_twenty])
        .await
        .expect("states");
    for state in &states {
        assert!(matches!(state, ContractState::Spent { .. }), "{state}");
    }

    // The third found nothing left that makes 20.
    let reply = refused[0];
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json["stage"], "selection");
    assert_eq!(
        reply.json["error"],
        "no exact combination of vouchers for 20 (denominations: 50, 10)"
    );
}

/// The bodies of `replies`, for a message.
fn bodies(replies: &[crate::http::Reply]) -> Vec<String> {
    replies
        .iter()
        .map(|reply| format!("{} {}", reply.status, reply.body))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delegated_key_is_served_only_where_the_server_was_told_to_reveal_it() {
    let world = World::start().await;
    let records = world.files.read().await.expect("the store");
    let secret = hex::encode(records.delegate_key.to_bytes());
    let public = hex::encode(records.public().to_bytes());

    let closed = Client::serve(Arc::clone(&world.agent), false).await;
    let reply = closed.get("/api/delegated-key").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(!reply.body.contains(&secret));
    // Nothing else gives it away either.
    assert!(!closed.get("/api/status").await.body.contains(&secret));
    assert_eq!(
        closed.get("/api/nothing-here").await.status,
        StatusCode::NOT_FOUND
    );

    let open = Client::serve(Arc::clone(&world.agent), true).await;
    let reply = open.get("/api/delegated-key").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[CACHE_CONTROL], "no-store");
    assert_eq!(reply.json, json!({"public": public, "secret": secret}));
    // It is read with a GET and with nothing else, and the status stays clean.
    let reply = open.post("/api/delegated-key", &json!({})).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(!open.get("/api/status").await.body.contains(&secret));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_is_down_leaves_the_status_answering_with_a_warning() {
    let world = World::start().await;
    let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
    let blind = Arc::new(Agent::new(world.files.clone(), chain));
    let client = Client::serve(blind, false).await;

    let reply = client.get("/api/status").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(reply.json["tip_height"].is_null(), "{}", reply.body);
    let warning = reply.json["warning"].as_str().expect("a warning");
    assert!(warning.contains("as last recorded"), "{warning}");
    let vouchers = reply.json["allowances"][0]["vouchers"]
        .as_array()
        .expect("vouchers");
    assert!(vouchers.iter().all(|voucher| voucher["state"] == "unspent"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payment_that_stops_at_the_node_is_a_bad_gateway_and_says_what_is_known() {
    let world = World::start_manual().await;
    let body = json!({"merchant": world.net.merchant(), "amount": "60"});

    // A node that cannot be asked: nothing was submitted.
    let chain = Chain::connect("http://127.0.0.1:1").expect("a client");
    let blind = Arc::new(Agent::new(world.files.clone(), chain));
    let client = Client::serve(blind, false).await;
    let reply = client.post("/api/pay", &body).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.body);
    assert_eq!(reply.json["stage"], "node");
    assert!(
        reply.json["error"]
            .as_str()
            .expect("an error")
            .starts_with("nothing was submitted")
    );
    assert!(reply.json.get("txid").is_none(), "{}", reply.body);

    // A node whose answer to the submission is lost: the transaction may have been taken, so the
    // reply names it.
    let proxy = Proxy::start(&world.net.node.url()).await;
    proxy.inject(Fault::LoseAnswer);
    let chain = Chain::connect(proxy.url()).expect("a client");
    let lossy = Arc::new(Agent::new(world.files.clone(), chain));
    let client = Client::serve(lossy, false).await;
    let reply = client.post("/api/pay", &body).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY, "{}", reply.body);
    assert_eq!(reply.json["stage"], "unknown");
    let txid = txid_of(
        reply.json["txid"]
            .as_str()
            .expect("the transaction that may exist"),
    );
    world.net.node.mint();
    let state = world
        .net
        .chain
        .tx_state(&txid)
        .await
        .expect("the transaction's state");
    assert!(matches!(state, TxState::Confirmed { .. }), "{state:?}");
}
