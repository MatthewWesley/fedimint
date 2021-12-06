pub mod config;
pub mod contracts;
mod db;

use crate::config::LightningModuleConfig;
use crate::contracts::incoming::{
    DecryptedPreimage, EncryptedPreimage, IncomingContractOffer, OfferId, PreimageDecryptionShare,
};
use crate::contracts::{
    Contract, ContractId, ContractOutcome, FundedContract, IdentifyableContract,
};
use crate::db::{
    AgreedDecryptionShareKey, AgreedDecryptionShareKeyPrefix, ContractKey, ContractKeyPrefix,
    ContractUpdateKey, OfferKey, OfferKeyPrefix, ProposeDecryptionShareKey,
    ProposeDecryptionShareKeyPrefix,
};
use async_trait::async_trait;
use bitcoin_hashes::Hash as BitcoinHash;
use itertools::Itertools;
use minimint_api::db::batch::{BatchItem, BatchTx};
use minimint_api::db::{Database, RawDatabase};
use minimint_api::encoding::{Decodable, Encodable};
use minimint_api::{Amount, FederationModule, PeerId};
use minimint_api::{InputMeta, OutPoint};
use secp256k1::rand::{CryptoRng, RngCore};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, trace, warn};

pub struct LightningModule {
    cfg: LightningModuleConfig,
    db: Arc<dyn RawDatabase>,
}

#[derive(Debug, Copy, Clone)]
pub struct ContractInput {
    pub crontract_id: contracts::ContractId,
    /// While for now we only support spending the entire contract we need to avoid
    pub amount: Amount,
    /// Of the three contract types only the outgoing one needs any other witness data than a
    /// signature. The signature is aggregated on the transaction level, so only the optional
    /// preimage remains.
    pub witness: Option<contracts::outgoing::Preimage>,
}

/// Represents an output of the Lightning module.
///
/// There are two sub-types:
///   * Normal contracts users may lock funds in
///   * Offers to buy preimages (see `contracts::incoming` docs)
///
/// The offer type exists to register `IncomingContractOffer`s. Instead of patching in a second way
/// of letting clients submit consensus items outside of transactions we let offers be a 0-amount
/// output. We need to take care to allow 0-input, 1-output transactions for that to allow users
/// to receive their fist tokens via LN without already having tokens.
pub enum ContractOrOfferOutput {
    Contract(ContractOutput),
    Offer(contracts::incoming::IncomingContractOffer),
}

#[derive(Debug, Encodable, Decodable)]
pub struct ContractOutput {
    pub amount: minimint_api::Amount,
    pub contract: contracts::Contract,
}

#[derive(Debug, Encodable, Decodable)]
pub struct ContractAccount {
    pub amount: minimint_api::Amount,
    pub contract: contracts::FundedContract,
}

#[derive(Debug, PartialEq, Eq, Encodable, Decodable)]
pub enum OutputOutcome {
    Contract {
        id: ContractId,
        // new_amount: minimint_api::Amount, // TODO: make optional, update later
        outcome: ContractOutcome,
    },
    Offer {
        id: OfferId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Encodable, Decodable)]
pub struct DecryptionShareCI {
    pub contract_id: ContractId,
    pub share: PreimageDecryptionShare,
}

#[async_trait(?Send)]
impl FederationModule for LightningModule {
    type Error = LightningModuleError;
    type TxInput = ContractInput;
    type TxOutput = ContractOrOfferOutput;
    type TxOutputOutcome = OutputOutcome;
    type ConsensusItem = DecryptionShareCI;

    async fn consensus_proposal<'a>(
        &'a self,
        _rng: impl RngCore + CryptoRng + 'a,
    ) -> Vec<Self::ConsensusItem> {
        self.db
            .find_by_prefix::<_, ProposeDecryptionShareKey, _>(&ProposeDecryptionShareKeyPrefix)
            .map(|res| {
                let (ProposeDecryptionShareKey(contract_id), share) = res.expect("DB error");
                DecryptionShareCI { contract_id, share }
            })
            .collect()
    }

    async fn begin_consensus_epoch<'a>(
        &'a self,
        mut batch: BatchTx<'a>,
        consensus_items: Vec<(PeerId, Self::ConsensusItem)>,
        _rng: impl RngCore + CryptoRng + 'a,
    ) {
        batch.append_from_iter(consensus_items.into_iter().filter_map(
            |(peer, decryption_share)| {
                let contract: ContractAccount = self
                    .db
                    .get_value(&ContractKey(decryption_share.contract_id))
                    .expect("DB error")
                    .or_else(|| {
                        warn!("Received decryption share fot non-incoming contract");
                        None
                    })?;
                let incoming_contract = match contract.contract {
                    FundedContract::Incoming(incoming) => incoming.contract,
                    _ => {
                        warn!(
                            "Received decryption share fot non-incoming contract from {}",
                            peer
                        );
                        return None;
                    }
                };

                if !self.validate_decryption_share(
                    peer,
                    &decryption_share.share,
                    &incoming_contract.encrypted_preimage,
                ) {
                    warn!("Received invalid decryption share from {}", peer);
                    return None;
                }

                Some(BatchItem::insert_new(
                    AgreedDecryptionShareKey(decryption_share.contract_id, peer),
                    decryption_share.share,
                ))
            },
        ));
        batch.commit();
    }

    fn validate_input<'a>(&self, input: &'a Self::TxInput) -> Result<InputMeta<'a>, Self::Error> {
        let account: ContractAccount = self
            .db
            .get_value(&ContractKey(input.crontract_id))
            .expect("DB error")
            .ok_or(LightningModuleError::UnknownContract(input.crontract_id))?;

        if account.amount < input.amount {
            return Err(LightningModuleError::InsufficientFunds(
                account.amount,
                input.amount,
            ));
        }

        let pub_key = match account.contract {
            FundedContract::Outgoing(outgoing) => {
                // TODO: properly define semantics, same as LN (> vs >=)
                if outgoing.timelock > self.block_height() {
                    // If the timelock hasn't expired yet …
                    let preimage_hash = bitcoin_hashes::sha256::Hash::hash(
                        &input
                            .witness
                            .as_ref()
                            .ok_or(LightningModuleError::MissingPreimage)?
                            .0[..],
                    );

                    // … and the spender provides a valid preimage …
                    if preimage_hash != outgoing.hash {
                        return Err(LightningModuleError::InvalidPreimage);
                    }

                    // … then the contract account can be spent using the gateway key,
                    outgoing.gateway_key
                } else {
                    // otherwise the user can claim the funds back.
                    outgoing.user_key
                }
            }
            FundedContract::Account(acc_contract) => acc_contract.key,
            FundedContract::Incoming(incoming) => match incoming.contract.decrypted_preimage {
                // Once the preimage has been decrypted …
                DecryptedPreimage::Pending => {
                    return Err(LightningModuleError::ContractNotReady);
                }
                // … either the user may spend the funds since they sold a valid preimage …
                DecryptedPreimage::Some(preimage) => preimage.0,
                // … or the gateway may claim back funds for not receiving the advertised preimage.
                DecryptedPreimage::Invalid => incoming.contract.gateway_key,
            },
        };

        Ok(InputMeta {
            amount: input.amount,
            puk_keys: Box::new(std::iter::once(pub_key)),
        })
    }

    fn apply_input<'a, 'b>(
        &'a self,
        mut batch: BatchTx<'a>,
        input: &'b Self::TxInput,
    ) -> Result<InputMeta<'b>, Self::Error> {
        let meta = self.validate_input(input)?;
        let amount = meta.amount;

        batch.append_maybe_update(
            ContractKey(input.crontract_id),
            move |_key, value: Option<ContractAccount>| {
                let mut account =
                    value.expect("Should fail validation if contract account doesn't exist");
                account.amount -= amount;
                Some(account)
            },
        );

        batch.commit();
        Ok(meta)
    }

    fn validate_output(&self, output: &Self::TxOutput) -> Result<Amount, Self::Error> {
        match output {
            ContractOrOfferOutput::Contract(contract) => {
                // Incoming contracts are special, they need to match an offer
                if let Contract::Incoming(incoming) = &contract.contract {
                    let offer = self
                        .db
                        .get_value::<_, IncomingContractOffer>(&OfferKey(incoming.hash))
                        .expect("DB error")
                        .ok_or(LightningModuleError::NoOffer(incoming.hash))?;

                    if contract.amount < offer.amount {
                        // If the account is not sufficiently funded fail the output
                        return Err(LightningModuleError::InsufficientIncomingFunding(
                            offer.amount,
                            contract.amount,
                        ));
                    }
                }

                if contract.amount == Amount::ZERO {
                    Err(LightningModuleError::ZeroOutput)
                } else {
                    Ok(contract.amount)
                }
            }
            ContractOrOfferOutput::Offer(offer) => {
                if !offer.encrypted_preimage.0.verify() {
                    Err(LightningModuleError::InvalidEncryptedPreimage)
                } else {
                    Ok(Amount::ZERO)
                }
            }
        }
    }

    fn apply_output<'a>(
        &'a self,
        mut batch: BatchTx<'a>,
        output: &'a Self::TxOutput,
        out_point: OutPoint,
    ) -> Result<Amount, Self::Error> {
        let amount = self.validate_output(output)?;

        match output {
            ContractOrOfferOutput::Contract(contract) => {
                let funded_contract = contract.contract.clone().to_funded(out_point);
                batch.append_maybe_update(
                    ContractKey(contract.contract.contract_id()),
                    move |_key, value: Option<ContractAccount>| {
                        Some(
                            value
                                .map(|mut value| {
                                    value.amount += amount;
                                    value
                                })
                                .unwrap_or_else(|| ContractAccount {
                                    amount,
                                    contract: funded_contract.clone(),
                                }),
                        )
                    },
                );
                batch.append_insert_new(
                    ContractUpdateKey(out_point),
                    OutputOutcome::Contract {
                        id: contract.contract.contract_id(),
                        outcome: contract.contract.to_outcome(),
                    },
                );

                if let Contract::Incoming(incoming) = &contract.contract {
                    let offer = self
                        .db
                        .get_value::<_, IncomingContractOffer>(&OfferKey(incoming.hash))
                        .expect("DB error")
                        .expect("offer exists if output is valid");

                    let deryption_share = self
                        .cfg
                        .threshold_sec_key
                        .decrypt_share(&incoming.encrypted_preimage.0)
                        .expect("We checked for decryption share validity on contract creation");
                    batch.append_insert_new(
                        ProposeDecryptionShareKey(contract.contract.contract_id()),
                        PreimageDecryptionShare(deryption_share),
                    );
                    batch.append_delete(OfferKey(offer.hash));
                }
            }
            ContractOrOfferOutput::Offer(offer) => {
                // TODO: sanity-check encrypted preimage size
                batch.append_insert_new(OfferKey(offer.hash), (*offer).clone());
            }
        }

        batch.commit();
        Ok(amount)
    }

    async fn end_consensus_epoch<'a>(
        &'a self,
        mut batch: BatchTx<'a>,
        _rng: impl RngCore + CryptoRng + 'a,
    ) {
        // Decrypt preimages
        let preimage_decraption_shares = self
            .db
            .find_by_prefix::<_, AgreedDecryptionShareKey, PreimageDecryptionShare>(
                &AgreedDecryptionShareKeyPrefix,
            )
            .map(|res| {
                let (key, value) = res.expect("DB error");
                (key.0, (key.1, value))
            })
            .into_group_map();

        for (contract_id, shares) in preimage_decraption_shares {
            if shares.len() < self.cfg.threshold {
                trace!(
                    "Too few decryption shares for contract {} ({} of min {})",
                    contract_id,
                    shares.len(),
                    self.cfg.threshold
                );
                continue;
            }
            debug!("Beginning to decrypt preimage of contract {}", contract_id);

            let contract = self
                .db
                .get_value::<_, ContractAccount>(&ContractKey(contract_id))
                .expect("DB error")
                .expect("decryption shares without contracts should be discarded earlier"); // FIXME: verify

            let (incoming_contract, out_point) = match contract.contract {
                FundedContract::Incoming(incoming) => (incoming.contract, incoming.out_point),
                _ => panic!(
                    "decryption shares without incoming contracts should be discarded earlier"
                ),
            };

            if !matches!(
                incoming_contract.decrypted_preimage,
                DecryptedPreimage::Pending
            ) {
                warn!("Tried to decrypt the same preimage twice, this should not happen.");
                continue;
            }

            let preimage = match self.cfg.threshold_pub_keys.decrypt(
                shares
                    .iter()
                    .map(|(peer, share)| (peer.to_usize(), &share.0)),
                &incoming_contract.encrypted_preimage.0,
            ) {
                Ok(preimage) => preimage,
                Err(_) => {
                    // TODO: check if that can happen even though shares are verified before
                    error!("Failed to decrypt preimage for {}", incoming_contract.hash);
                    continue;
                }
            };

            let decrypted_preimage = if preimage.len() != 32 {
                DecryptedPreimage::Invalid
            } else if incoming_contract.hash != bitcoin_hashes::sha256::Hash::hash(&preimage) {
                DecryptedPreimage::Invalid
            } else {
                if let Ok(preimage_key) = secp256k1::schnorrsig::PublicKey::from_slice(&preimage) {
                    DecryptedPreimage::Some(crate::contracts::incoming::Preimage(preimage_key))
                } else {
                    DecryptedPreimage::Invalid
                }
            };
            debug!(
                "Decrypted preimage of contract {}: {:?}",
                contract_id, decrypted_preimage
            );

            let decrypted_preimage_clone = decrypted_preimage.clone();
            batch.append_maybe_update(
                ContractKey(contract_id),
                move |_key, value: Option<ContractAccount>| {
                    let mut account = value.expect("checked before that it exists");
                    let mut incoming = match &mut account.contract {
                        FundedContract::Incoming(incoming) => incoming,
                        _ => unreachable!("previously checked that it's an incoming contrac"),
                    };

                    incoming.contract.decrypted_preimage = decrypted_preimage_clone.clone();
                    trace!("Updating contract account: {:?}", account);
                    Some(account)
                },
            );

            batch.append_maybe_update(
                ContractUpdateKey(out_point),
                move |key, value: Option<OutputOutcome>| {
                    let mut contract = value.expect("outcome was created on funding");
                    let outcome = match &mut contract {
                        OutputOutcome::Contract {
                            id,
                            outcome: ContractOutcome::Incoming(decryption_outcome),
                        } => decryption_outcome,
                        _ => panic!("We are expeccting an incoming contract"),
                    };
                    *outcome = decrypted_preimage.clone();
                    Some(contract)
                },
            );
        }
        batch.commit();
    }

    fn output_status(&self, out_point: OutPoint) -> Option<Self::TxOutputOutcome> {
        self.db
            .get_value(&ContractUpdateKey(out_point))
            .expect("DB error")
    }
}

impl LightningModule {
    pub fn new<D: RawDatabase + 'static>(cfg: LightningModuleConfig, db: D) -> LightningModule {
        LightningModule {
            cfg,
            db: Arc::new(db),
        }
    }

    fn validate_decryption_share(
        &self,
        peer: PeerId,
        share: &PreimageDecryptionShare,
        message: &EncryptedPreimage,
    ) -> bool {
        self.cfg
            .threshold_pub_keys
            .public_key_share(peer.to_usize())
            .verify_decryption_share(&share.0, &message.0)
    }

    fn block_height(&self) -> u32 {
        // FIXME: duplicate round consensus logic or define proper interface
        const DB_PREFIX_ROUND_CONSENSUS: u8 = 0x32;

        #[derive(Clone, Debug, Encodable, Decodable)]
        pub struct RoundConsensusKey;

        impl minimint_api::db::DatabaseKeyPrefixConst for RoundConsensusKey {
            const DB_PREFIX: u8 = DB_PREFIX_ROUND_CONSENSUS;
        }

        #[derive(Debug, Encodable, Decodable)]
        pub struct RoundConsensus {
            block_height: u32,
            fee_rate: u64,
            randomness_beacon: [u8; 32],
        }

        self.db
            .get_value::<_, RoundConsensus>(&RoundConsensusKey)
            .expect("DB error")
            .map(|rc| rc.block_height)
            .unwrap_or(0)
    }

    pub fn get_offers(&self) -> Vec<IncomingContractOffer> {
        self.db
            .find_by_prefix::<_, OfferKey, IncomingContractOffer>(&OfferKeyPrefix)
            .map(|res| res.expect("DB error").1)
            .collect()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LightningModuleError {
    #[error("The the input contract {0} does not exist")]
    UnknownContract(ContractId),
    #[error("The input contract has too little funds, got {0}, input spends {1}")]
    InsufficientFunds(Amount, Amount),
    #[error("An outgoing LN contract spend did not provide a preimage")]
    MissingPreimage,
    #[error("An outgoing LN contract spend provided a wrong preimage")]
    InvalidPreimage,
    #[error("Incoming contract not ready to be spent yet, decryption in progress")]
    ContractNotReady,
    #[error("Output contract value may not be zero unless it's an offer output")]
    ZeroOutput,
    #[error("Offer contains invalid threshold-encrypted data")]
    InvalidEncryptedPreimage,
    #[error(
        "The incoming LN account requires more funding according to the offer (need {0} got {1})"
    )]
    InsufficientIncomingFunding(Amount, Amount),
    #[error("No offer found for payment hash {0}")]
    NoOffer(secp256k1::hashes::sha256::Hash),
}
