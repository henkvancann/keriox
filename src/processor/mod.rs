use std::sync::Arc;

use crate::{database::sled::SledEventDatabase, derivation::self_addressing::SelfAddressing, error::Error, event::{
        event_data::EventData,
        sections::{
            seal::{EventSeal, Seal},
            KeyConfig,
        },
        EventMessage,
    }, event_message::{attachment::Attachment, parse::Deserialized, signed_event_message::{
            SignedEventMessage, SignedNontransferableReceipt, SignedTransferableReceipt,
            TimestampedSignedEventMessage,
        }}, prefix::{IdentifierPrefix, SelfAddressingPrefix}, state::{EventSemantics, IdentifierState}};

#[cfg(feature = "async")]
pub mod async_processing;
#[cfg(test)]
mod tests;

pub struct EventProcessor {
    pub db: Arc<SledEventDatabase>,
}

impl EventProcessor {
    pub fn new(db: Arc<SledEventDatabase>) -> Self {
        Self { db }
    }

    /// Compute State for Prefix
    ///
    /// Returns the current State associated with
    /// the given Prefix
    pub fn compute_state(&self, id: &IdentifierPrefix) -> Result<Option<IdentifierState>, Error> {
        // start with empty state
        let mut state = IdentifierState::default();
        if let Some(events) = self.db.get_kel_finalized_events(id) {
            // we sort here to get inception first
            let mut sorted_events = events.collect::<Vec<TimestampedSignedEventMessage>>();
            sorted_events.sort();
            for event in sorted_events {
                state = match state.clone().apply(&event.signed_event_message) {
                    Ok(s) => s,
                    // will happen when a recovery has overridden some part of the KEL,
                    Err(e) => match e {
                        // skip out of order and partially signed events
                        Error::EventOutOfOrderError | Error::NotEnoughSigsError => continue,
                        // stop processing here
                        _ => break,
                    },
                };
            }
        } else {
            // no inception event, no state
            return Ok(None);
        }
        Ok(Some(state))
    }

    /// Compute State for Prefix and sn
    ///
    /// Returns the State associated with the given
    /// Prefix after applying event of given sn.
    pub fn compute_state_at_sn(
        &self,
        id: &IdentifierPrefix,
        sn: u64,
    ) -> Result<Option<IdentifierState>, Error> {
        let mut state = IdentifierState::default();
        if let Some(events) = self.db.get_kel_finalized_events(id) {
            // TODO: testing approach if events come out sorted already (as they should coz of put sequence)
            let mut sorted_events = events.collect::<Vec<TimestampedSignedEventMessage>>();
            sorted_events.sort();
            for event in sorted_events
                .iter()
                .filter(|e| e.signed_event_message.event_message.event.sn <= sn)
            {
                state = state.apply(&event.signed_event_message.event_message)?;
            }
        } else {
            return Ok(None);
        }
        Ok(Some(state))
    }

    /// Get last establishment event seal for Prefix
    ///
    /// Returns the EventSeal of last establishment event
    /// from KEL of given Prefix.
    pub fn get_last_establishment_event_seal(
        &self,
        id: &IdentifierPrefix,
    ) -> Result<Option<EventSeal>, Error> {
        let mut state = IdentifierState::default();
        let mut last_est = None;
        if let Some(events) = self.db.get_kel_finalized_events(id) {
            for event in events {
                state = state.apply(&event.signed_event_message.event_message.event)?;
                // TODO: is this event.event.event stuff too ugly? =)
                last_est = match event.signed_event_message.event_message.event.event_data {
                    EventData::Icp(_) => Some(event.signed_event_message),
                    EventData::Rot(_) => Some(event.signed_event_message),
                    _ => last_est,
                }
            }
        } else {
            return Ok(None);
        }
        let seal = last_est.and_then(|event| {
            let event_digest = SelfAddressing::Blake3_256.derive(&event.serialize().unwrap());
            Some(EventSeal {
                prefix: event.event_message.event.prefix,
                sn: event.event_message.event.sn,
                event_digest,
            })
        });
        Ok(seal)
    }

    /// Get KERL for Prefix
    ///
    /// Returns the current validated KEL for a given Prefix
    pub fn get_kerl(&self, id: &IdentifierPrefix) -> Result<Option<Vec<u8>>, Error> {
        match self.db.get_kel_finalized_events(id) {
            Some(events) => Ok(Some(
                events
                    .map(|event| event.signed_event_message.serialize().unwrap_or_default())
                    .fold(vec![], |mut accum, serialized_event| {
                        accum.extend(serialized_event);
                        accum
                    }),
            )),
            None => Ok(None),
        }
    }

    /// Get keys from Establishment Event
    ///
    /// Returns the current Key Config associated with
    /// the given Prefix at the establishment event
    /// represented by sn and Event Digest
    fn get_keys_at_event(
        &self,
        id: &IdentifierPrefix,
        sn: u64,
        event_digest: &SelfAddressingPrefix,
    ) -> Result<Option<KeyConfig>, Error> {
        if let Ok(Some(event)) = self.get_event_at_sn(id, sn) {
            // if it's the event we're looking for
            if event_digest.verify_binding(&event.signed_event_message.event_message.serialize()?) {
                // return the config or error if it's not an establishment event
                Ok(Some(
                    match event.signed_event_message.event_message.event.event_data {
                        EventData::Icp(icp) => icp.key_config,
                        EventData::Rot(rot) => rot.key_config,
                        EventData::Dip(dip) => dip.inception_data.key_config,
                        EventData::Drt(drt) => drt.key_config,
                        // the receipt has a binding but it's NOT an establishment event
                        _ => Err(Error::SemanticError("Receipt binding incorrect".into()))?,
                    },
                ))
            } else {
                Err(Error::SemanticError("Event digests doesn't match".into()))
            }
        } else {
            Err(Error::SemanticError(
                "No event of given sn and prefix in database".into(),
            ))
        }
    }

    /// Validate delegating event seal.
    ///
    /// Validates binding between delegated and delegating events. The validation
    /// is based on delegating location seal and delegated event.
    fn validate_seal(&self, seal: EventSeal, delegated_event: &[u8]) -> Result<(), Error> {
        // Check if event of seal's prefix and sn is in db.
        if let Ok(Some(event)) = self.get_event_at_sn(&seal.prefix, seal.sn) {
            // Extract prior_digest and data field from delegating event.
            let data = match event.signed_event_message.event_message.event.event_data {
                EventData::Rot(rot) => rot.data,
                EventData::Ixn(ixn) => ixn.data,
                EventData::Drt(drt) => drt.data,
                _ => return Err(Error::SemanticError("Improper event type".to_string())),
            };

            // Check if event seal list contains delegating event seal.
            if !data.iter().any(|s| match s {
                Seal::Event(es) => es.event_digest.verify_binding(delegated_event),
                _ => false,
            }) {
                return Err(Error::SemanticError(
                    "Data field doesn't contain delegating event seal.".to_string(),
                ));
            };
        } else {
            return Err(Error::EventOutOfOrderError);
        }
        Ok(())
    }

    pub fn has_receipt(
        &self,
        id: &IdentifierPrefix,
        sn: u64,
        validator_pref: &IdentifierPrefix,
    ) -> Result<bool, Error> {
        Ok(if let Some(receipts) = self.db.get_receipts_t(id) {
            receipts
                .filter(|r| r.body.event.sn.eq(&sn))
                .any(|receipt| receipt.validator_seal.prefix.eq(validator_pref))
        } else {
            false
        })
    }

    /// Process
    ///
    /// Process a deserialized KERI message
    pub fn process(&self, data: Deserialized) -> Result<Option<IdentifierState>, Error> {
        match data {
            Deserialized::Event(e) => self.process_event(&e),
            Deserialized::NontransferableRct(rct) => self.process_witness_receipt(rct),
            Deserialized::TransferableRct(rct) => self.process_validator_receipt(rct),
        }
    }

    pub fn process_actual_event(
        &self,
        id: &IdentifierPrefix,
        event: impl EventSemantics,
    ) -> Result<Option<IdentifierState>, Error> {
        if let Some(state) = self.compute_state(id)? {
            Ok(Some(event.apply_to(state)?))
        } else {
            Ok(None)
        }
    }

    fn find_source_seal(event: &SignedEventMessage) -> Result<(u64, SelfAddressingPrefix), Error> {
        match event.attachments.last().ok_or(Error::SemanticError(
                "Missing source seal.".into(),
            ))? {
                Attachment::SealSourceCouplets(
                    ref source_seal_list,
                ) => {
                    let ss = source_seal_list
                        .last()
                        .ok_or(Error::SemanticError("Missing source seal".into()))?;
                    Ok((ss.sn, ss.digest.clone()))
                }
                _ => Err(Error::SemanticError("Missing source seal.".into())),
            }
    }

    /// Process Event
    ///
    /// Validates a Key Event against the latest state
    /// of the Identifier and applies it to update the state
    /// returns the updated state
    /// TODO improve checking and handling of errors!
    pub fn process_event(
        &self,
        signed_event: &SignedEventMessage,
    ) -> Result<Option<IdentifierState>, Error> {
        // Log event.

        let id = &signed_event.event_message.event.prefix;

        // If delegated event, check its delegator seal.
        match signed_event
            .event_message
            .event
            .event_data
            .clone()
        {
            EventData::Dip(dip) => {
                let (sn, dig) = Self::find_source_seal(&signed_event)?;
                let seal = EventSeal {
                    prefix: dip.delegator,
                    sn,
                    event_digest: dig,
                };
                self.validate_seal(seal, &signed_event.event_message.serialize()?)
            }
            EventData::Drt(_drt) => {
                let delegator = self
                    .compute_state(&signed_event.event_message.event.prefix)?
                    .ok_or(Error::SemanticError("Missing state of delegated identifier".into()))?
                    .delegator
                    .ok_or(Error::SemanticError("Missing delegator".into()))?;
                let (sn, dig) = Self::find_source_seal(&signed_event)?;
                let seal = EventSeal {
                    prefix: delegator,
                    sn,
                    event_digest: dig,
                };
                self.validate_seal(seal, &signed_event.event_message.serialize()?)
            }
            _ => Ok(()),
        }?;
        self.apply_to_state(signed_event.event_message.clone())
            .and_then(|new_state| {
                // add event from the get go and clean it up on failure later
                self.db.add_kel_finalized_event(signed_event.clone(), id)?;
                // match on verification result
                match new_state
                    .current
                    .verify(&signed_event.event_message.serialize()?, &signed_event.signatures)
                    .and_then(|result| {
                        if !result {
                            Err(Error::SignatureVerificationError)
                        } else {
                            // TODO should check if there are enough receipts and probably escrow
                            Ok(new_state)
                        }
                    }) {
                    Ok(state) => Ok(Some(state)),
                    Err(e) => {
                        match e {
                            // should not happen anymore
                            // Error::NotEnoughSigsError =>
                            // should not happen anymore
                            //Error::EventOutOfOrderError =>
                            Error::EventDuplicateError => {
                                self.db.add_duplicious_event(signed_event.clone(), id)?
                            }
                            _ => (),
                        };
                        // remove last added event
                        self.db.remove_kel_finalized_event(id, &signed_event)?;
                        Err(e)
                    }
                }
            })
    }

    /// Process Validator Receipt
    ///
    /// Checks the receipt against the receipted event
    /// and the state of the validator, returns the state
    /// of the identifier being receipted
    /// TODO improve checking and handling of errors!
    pub fn process_validator_receipt(
        &self,
        vrc: SignedTransferableReceipt,
    ) -> Result<Option<IdentifierState>, Error> {
        match &vrc.body.event.event_data {
            EventData::Rct(_r) => {
                if let Ok(Some(event)) =
                    self.get_event_at_sn(&vrc.body.event.prefix, vrc.body.event.sn)
                {
                    let kp = self.get_keys_at_event(
                        &vrc.validator_seal.prefix,
                        vrc.validator_seal.sn,
                        &vrc.validator_seal.event_digest,
                    )?;
                    if kp.is_some()
                        && kp.unwrap().verify(
                            &event.signed_event_message.event_message.serialize()?,
                            &vrc.signatures,
                        )?
                    {
                        self.db.add_receipt_t(vrc.clone(), &vrc.body.event.prefix)
                    } else {
                        Err(Error::SemanticError("Incorrect receipt signatures".into()))
                    }
                } else {
                    self.db
                        .add_escrow_t_receipt(vrc.clone(), &vrc.body.event.prefix)?;
                    Err(Error::SemanticError("Receipt escrowed".into()))
                }
            }
            _ => Err(Error::SemanticError("incorrect receipt structure".into())),
        }?;
        self.compute_state(&vrc.body.event.prefix)
    }

    /// Process Witness Receipt
    ///
    /// Checks the receipt against the receipted event
    /// returns the state of the Identifier being receipted,
    /// which may have been updated by un-escrowing events
    /// TODO improve checking and handling of errors!
    pub fn process_witness_receipt(
        &self,
        rct: SignedNontransferableReceipt,
    ) -> Result<Option<IdentifierState>, Error> {
        // check structure is correct
        match &rct.body.event.event_data {
            // get event which is being receipted
            EventData::Rct(_) => {
                let id = &rct.body.event.prefix.to_owned();
                if let Ok(Some(event)) =
                    self.get_event_at_sn(&rct.body.event.prefix, rct.body.event.sn)
                {
                    let serialized_event = event.signed_event_message.serialize()?;
                    let (_, mut errors): (Vec<_>, Vec<Result<bool, Error>>) = rct
                        .clone()
                        .couplets
                        .into_iter()
                        .map(|(witness, receipt)| witness.verify(&&serialized_event, &receipt))
                        .partition(Result::is_ok);
                    if errors.len() == 0 {
                        self.db.add_receipt_nt(rct, &id)?
                    } else {
                        let e = errors.pop().unwrap().unwrap_err();
                        return Err(e);
                    }
                } else {
                    self.db.add_escrow_nt_receipt(rct, &id)?
                }
                self.compute_state(&id)
            }
            _ => Err(Error::SemanticError("incorrect receipt structure".into())),
        }
    }

    pub fn get_event_at_sn(
        &self,
        id: &IdentifierPrefix,
        sn: u64,
    ) -> Result<Option<TimestampedSignedEventMessage>, Error> {
        if let Some(mut events) = self.db.get_kel_finalized_events(id) {
            Ok(events.find(|event| event.signed_event_message.event_message.event.sn == sn))
        } else {
            Ok(None)
        }
    }

    fn apply_to_state(&self, event: EventMessage) -> Result<IdentifierState, Error> {
        // get state for id (TODO cache?)
        self.compute_state(&event.event.prefix)
            // get empty state if there is no state yet
            .and_then(|opt| Ok(opt.map_or_else(|| IdentifierState::default(), |s| s)))
            // process the event update
            .and_then(|state| event.apply_to(state))
    }
}
