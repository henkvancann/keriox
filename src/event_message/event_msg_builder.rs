use crate::{derivation::{basic::Basic, self_addressing::SelfAddressing}, error::Error, event::sections::key_config::nxt_commitment, event::{
        event_data::{
            delegated::{DelegatedInceptionEvent, DelegatedRotationEvent},
            interaction::InteractionEvent,
            rotation::RotationEvent,
        },
        sections::{threshold::SignatureThreshold, WitnessConfig},
        SerializationFormats,
    }, event::{
        event_data::{inception::InceptionEvent, EventData},
        sections::seal::Seal,
        sections::InceptionWitnessConfig,
        sections::KeyConfig,
        Event, EventMessage,
    }, keys::PublicKey, prefix::{BasicPrefix, IdentifierPrefix, SelfAddressingPrefix}};
use ed25519_dalek::Keypair;
use rand::rngs::OsRng;

pub struct EventMsgBuilder {
    event_type: EventType,
    prefix: IdentifierPrefix,
    sn: u64,
    key_threshold: SignatureThreshold,
    keys: Vec<BasicPrefix>,
    next_keys: Vec<BasicPrefix>,
    prev_event: SelfAddressingPrefix,
    data: Vec<Seal>,
    delegator: IdentifierPrefix,
    format: SerializationFormats,
    derivation: SelfAddressing,
}

#[derive(Clone, Debug)]
pub enum EventType {
    Inception,
    Rotation,
    Interaction,
    DelegatedInception,
    DelegatedRotation,
}

impl EventType {
    pub fn is_establishment_event(&self) -> bool {
        match self {
            EventType::Inception
            | EventType::Rotation
            | EventType::DelegatedInception
            | EventType::DelegatedRotation => true,
            _ => false,
        }
    }
}

impl EventMsgBuilder {
    pub fn new(event_type: EventType) -> Result<Self, Error> {
        let mut rng = OsRng {};
        let kp = Keypair::generate(&mut rng);
        let nkp = Keypair::generate(&mut rng);
        let pk = PublicKey::new(kp.public.to_bytes().to_vec());
        let npk = PublicKey::new(nkp.public.to_bytes().to_vec());
        let basic_pref = Basic::Ed25519.derive(pk);
        Ok(EventMsgBuilder {
            event_type,
            prefix: IdentifierPrefix::default(),
            keys: vec![basic_pref],
            next_keys: vec![Basic::Ed25519.derive(npk)],
            key_threshold: SignatureThreshold::Simple(1),
            sn: 1,
            prev_event: SelfAddressing::Blake3_256.derive(&[0u8; 32]),
            data: vec![],
            delegator: IdentifierPrefix::default(),
            format: SerializationFormats::JSON,
            derivation: SelfAddressing::Blake3_256,
        })
    }

    pub fn with_prefix(self, prefix: IdentifierPrefix) -> Self {
        EventMsgBuilder { prefix, ..self }
    }

    pub fn with_keys(self, keys: Vec<BasicPrefix>) -> Self {
        EventMsgBuilder { keys, ..self }
    }

    pub fn with_next_keys(self, next_keys: Vec<BasicPrefix>) -> Self {
        EventMsgBuilder { next_keys, ..self }
    }

    pub fn with_sn(self, sn: u64) -> Self {
        EventMsgBuilder { sn, ..self }
    }
    pub fn with_previous_event(self, prev_event: SelfAddressingPrefix) -> Self {
        EventMsgBuilder { prev_event, ..self }
    }

    pub fn with_seal(mut self, seals: Vec<Seal>) -> Self {
        self.data.extend(seals);
        EventMsgBuilder { ..self }
    }

    pub fn with_delegator(self, delegator: IdentifierPrefix) -> Self {
        EventMsgBuilder {
            delegator,
            ..self
        }
    }

    pub fn with_threshold(self, threshold: SignatureThreshold) -> Self {
        EventMsgBuilder {
            key_threshold: threshold,
            ..self
        }
    }

    pub fn build(self) -> Result<EventMessage, Error> {
        let next_key_hash = nxt_commitment(
            &self.key_threshold,
            &self.next_keys,
            &SelfAddressing::Blake3_256,
        );
        let key_config = KeyConfig::new(self.keys, Some(next_key_hash), Some(self.key_threshold));
        let prefix =
            if self.prefix == IdentifierPrefix::default() && key_config.public_keys.len() == 1 {
                IdentifierPrefix::Basic(key_config.clone().public_keys[0].clone())
            } else {
                self.prefix
            };

        Ok(match self.event_type {
            EventType::Inception => {
                let icp_event = InceptionEvent {
                    key_config,
                    witness_config: InceptionWitnessConfig::default(),
                    inception_configuration: vec![],
                    data: vec![],
                };

                match prefix {
                    IdentifierPrefix::Basic(_) => Event {
                        prefix,
                        sn: 0,
                        event_data: EventData::Icp(icp_event),
                    }
                    .to_message(self.format)?,
                    IdentifierPrefix::SelfAddressing(_) => {
                        icp_event.incept_self_addressing(self.derivation, self.format)?
                    }
                    _ => todo!(),
                }
            }

            EventType::Rotation => Event {
                prefix,
                sn: self.sn,
                event_data: EventData::Rot(RotationEvent {
                    previous_event_hash: self.prev_event,
                    key_config,
                    witness_config: WitnessConfig::default(),
                    data: self.data,
                }),
            }
            .to_message(self.format)?,
            EventType::Interaction => Event {
                prefix,
                sn: self.sn,
                event_data: EventData::Ixn(InteractionEvent {
                    previous_event_hash: self.prev_event,
                    data: self.data,
                }),
            }
            .to_message(self.format)?,
            EventType::DelegatedInception => {
                let icp_data = InceptionEvent {
                    key_config,
                    witness_config: InceptionWitnessConfig::default(),
                    inception_configuration: vec![],
                    data: vec![],
                };
                DelegatedInceptionEvent {
                    inception_data: icp_data,
                    delegator: self.delegator,
                }
                .incept_self_addressing(self.derivation, self.format)?
            }
            EventType::DelegatedRotation => {
                let rotation_data = RotationEvent {
                    previous_event_hash: self.prev_event,
                    key_config,
                    witness_config: WitnessConfig::default(),
                    data: self.data,
                };
                Event {
                    prefix,
                    sn: self.sn,
                    event_data: EventData::Drt(DelegatedRotationEvent {
                        rotation_data,
                    }),
                }
                .to_message(self.format)?
            }
        })
    }
}
