//! To build the binary:
//!
//!     `cargo build --release --bin rotate`
//!
//!
//!
//!
//!
use std::collections::HashMap;
use std::env;
use std::ops::Deref;

use avail_subxt::avail::Client;
use avail_subxt::config::Header as HeaderTrait;
use avail_subxt::primitives::Header;
use avail_subxt::{api, build_client};
use codec::{Decode, Encode};
use log::debug;
use plonky2x::frontend::ecc::ed25519::gadgets::verify::DUMMY_SIGNATURE;
use serde::de::Error;
use serde::Deserialize;
use sp_core::ed25519::{self, Public as EdPublic, Signature};
use sp_core::{blake2_256, bytes, Pair, H256};
use subxt::rpc::RpcParams;
use vectorx::input::types::StoredJustificationData;
use vectorx::input::{RedisClient, RpcDataFetcher};

#[derive(Deserialize, Debug)]
pub struct SubscriptionMessageResult {
    pub result: String,
    pub subscription: String,
}

#[derive(Deserialize, Debug)]
pub struct SubscriptionMessage {
    pub jsonrpc: String,
    pub params: SubscriptionMessageResult,
    pub method: String,
}

#[derive(Clone, Debug, Decode, Encode, Deserialize)]
pub struct Precommit {
    pub target_hash: H256,
    /// The target block's number
    pub target_number: u32,
}

#[derive(Clone, Debug, Decode, Deserialize)]
pub struct SignedPrecommit {
    pub precommit: Precommit,
    /// The signature on the message.
    pub signature: Signature,
    /// The Id of the signer.
    pub id: EdPublic,
}
#[derive(Clone, Debug, Decode, Deserialize)]
pub struct Commit {
    pub target_hash: H256,
    /// The target block's number.
    pub target_number: u32,
    /// Precommits for target block or any block after it that justify this commit.
    pub precommits: Vec<SignedPrecommit>,
}

#[derive(Clone, Debug, Decode)]
pub struct GrandpaJustification {
    pub round: u64,
    pub commit: Commit,
    pub votes_ancestries: Vec<Header>,
}

impl<'de> Deserialize<'de> for GrandpaJustification {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = bytes::deserialize(deserializer)?;
        Self::decode(&mut &encoded[..])
            .map_err(|codec_err| D::Error::custom(format!("Invalid decoding: {:?}", codec_err)))
    }
}

#[derive(Debug, Decode)]
pub struct Authority(EdPublic, u64);

#[derive(Debug, Encode)]
pub enum SignerMessage {
    DummyMessage(u32),
    PrecommitMessage(Precommit),
}

#[tokio::main]
pub async fn main() {
    env::set_var("RUST_LOG", "debug");
    dotenv::dotenv().ok();
    env_logger::init();

    // Save every 90 blocks (every 30 minutes).
    const BLOCK_SAVE_INTERVAL: usize = 90;
    debug!(
        "Starting indexer, saving every {} blocks.",
        BLOCK_SAVE_INTERVAL
    );

    let url: &str = "wss://kate.avail.tools:443/ws";

    let c: Client = build_client(url, false).await.unwrap();
    let t = c.rpc().deref();
    let sub: Result<avail_subxt::rpc::Subscription<GrandpaJustification>, subxt::Error> = t
        .subscribe(
            "grandpa_subscribeJustifications",
            RpcParams::new(),
            "grandpa_unsubscribeJustifications",
        )
        .await;

    let mut r: RedisClient = RedisClient::new().await;

    let mut sub = sub.unwrap();

    // Wait for new justification.
    while let Some(Ok(justification)) = sub.next().await {
        // Initialize data fetcher (re-initialize every new event to avoid connection reset).
        let fetcher = RpcDataFetcher::new().await;

        if justification.commit.target_number % BLOCK_SAVE_INTERVAL as u32 != 0 {
            continue;
        }
        debug!(
            "New justification from block {}",
            justification.commit.target_number
        );

        // Note: justification.commit.target_hash is probably block_hash.
        // Noticed this because retrieved the correct header from commit.target_hash, but the hash
        // doesn't match header.hash()

        // Get the header corresponding to the new justification.
        let header = c
            .rpc()
            .header(Some(justification.commit.target_hash))
            .await
            .unwrap()
            .unwrap();

        // A bit redundant, but just to make sure the hash is correct.
        let block_hash = justification.commit.target_hash;
        let header_hash = header.hash();
        let calculated_hash: H256 = Encode::using_encoded(&header, blake2_256).into();
        if header_hash != calculated_hash {
            continue;
        }

        // Get current authority set ID.
        let set_id_key = api::storage().grandpa().current_set_id();
        let authority_set_id = c
            .storage()
            .at(block_hash)
            .fetch(&set_id_key)
            .await
            .unwrap()
            .unwrap();

        // Form a message which is signed in the justification.
        let signed_message = Encode::encode(&(
            &SignerMessage::PrecommitMessage(justification.commit.precommits[0].clone().precommit),
            &justification.round,
            &authority_set_id,
        ));

        // Verify all the signatures of the justification and extract the public keys.
        // TODO: Check if the authorities always going to be in the same order? Otherwise sort them.
        let validators = justification
            .commit
            .precommits
            .iter()
            .filter_map(|precommit| {
                let is_ok = <ed25519::Pair as Pair>::verify(
                    &precommit.clone().signature,
                    signed_message.as_slice(),
                    &precommit.clone().id,
                );
                if is_ok {
                    Some((
                        precommit.clone().id.0.to_vec(),
                        precommit.clone().signature.0.to_vec(),
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let pubkeys = validators.iter().map(|v| v.0.clone()).collect::<Vec<_>>();
        let signatures = validators.iter().map(|v| v.1.clone()).collect::<Vec<_>>();

        // Create map from pubkey to signature.
        let mut pubkey_to_signature = HashMap::new();
        for (pubkey, signature) in pubkeys.iter().zip(signatures.iter()) {
            pubkey_to_signature.insert(pubkey.to_vec(), signature.to_vec());
        }

        // Check that at least 2/3 of the validators signed the justification.
        // Note: Assumes the validator set have equal voting power.
        let authorities = fetcher.get_authorities(header.number - 1).await;
        let num_authorities = authorities.len();
        if 3 * pubkeys.len() < num_authorities * 2 {
            continue;
        }

        // Create justification data.
        let mut justification_pubkeys = Vec::new();
        let mut justification_signatures = Vec::new();
        let mut validator_signed = Vec::new();
        for authority_pubkey in authorities.iter() {
            if let Some(signature) = pubkey_to_signature.get(authority_pubkey) {
                justification_pubkeys.push(authority_pubkey.to_vec());
                justification_signatures.push(signature.to_vec());
                validator_signed.push(true);
            } else {
                justification_pubkeys.push(authority_pubkey.to_vec());
                justification_signatures.push(DUMMY_SIGNATURE.to_vec());
                validator_signed.push(false);
            }
        }

        // Add justification to Redis.
        let store_justification_data = StoredJustificationData {
            block_number: header.number,
            signed_message: signed_message.clone(),
            pubkeys: justification_pubkeys,
            signatures: justification_signatures,
            num_authorities: authorities.len(),
            validator_signed,
        };
        r.add_justification(store_justification_data).await;
    }
}
