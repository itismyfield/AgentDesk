use std::{fmt, marker::PhantomData};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{IgnoredAny, MapAccess, Visitor, value::MapAccessDeserializer},
};
use sha2::{Digest, Sha256};

use super::*;

const EPOCH: u32 = 2;
const SCHEMA: u32 = 1;
const HASH_DOMAIN: &[u8] = b"agentdesk/restart-v2/phase-event/schema-1";

#[derive(Serialize)]
#[serde(transparent)]
struct MapOnly<T>(T);

struct MapOnlyVisitor<T>(PhantomData<T>);

impl<'de, T> Visitor<'de> for MapOnlyVisitor<T>
where
    T: Deserialize<'de>,
{
    type Value = MapOnly<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        T::deserialize(MapAccessDeserializer::new(map)).map(MapOnly)
    }
}

impl<'de, T> Deserialize<'de> for MapOnly<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(MapOnlyVisitor(PhantomData))
    }
}

#[derive(Deserialize)]
struct EpochProbe {
    epoch: u64,
}

#[derive(Deserialize)]
struct SchemaProbe {
    schema: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RequiredPreviousHash {
    Null(()),
    Hash(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEvent {
    epoch: u32,
    schema: u32,
    request_id: String,
    attempt_id: String,
    provider_hex: String,
    channel_hex: String,
    nonce: String,
    sequence: u64,
    previous_hash: RequiredPreviousHash,
    event_hash: String,
    kind: MapOnly<WireKind>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WireKind {
    Empty(EmptyWireKind),
    Terminal(TerminalWireKind),
    Receipt(ReceiptWireKind),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyWireKind {
    phase: EmptyWirePhase,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EmptyWirePhase {
    Bound,
    Started,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TerminalWireKind {
    phase: TerminalWirePhase,
    outcome: TerminalOutcomeV2,
    proof: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalWirePhase {
    Terminal,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptWireKind {
    phase: ReceiptWirePhase,
    receipt: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptWirePhase {
    Receipt,
}

#[derive(Serialize)]
struct WirePreimage<'a> {
    epoch: u32,
    schema: u32,
    request_id: &'a str,
    attempt_id: &'a str,
    provider_hex: &'a str,
    channel_hex: &'a str,
    nonce: &'a str,
    sequence: u64,
    previous_hash: &'a RequiredPreviousHash,
    kind: &'a MapOnly<WireKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhaseCodecError;

pub(super) fn decode(raw: &[u8]) -> PhaseEventDispositionV2 {
    if serde_json::from_slice::<IgnoredAny>(raw).is_err() {
        return poison(raw, PhasePoisonV2::MalformedJson);
    }
    let Ok(MapOnly(EpochProbe { epoch })) = serde_json::from_slice(raw) else {
        return poison(raw, PhasePoisonV2::InvalidCurrentRecord);
    };
    if epoch != u64::from(EPOCH) {
        return DecodeDisposition::Unsupported { raw: raw.into() };
    }
    let Ok(MapOnly(SchemaProbe { schema })) = serde_json::from_slice(raw) else {
        return poison(raw, PhasePoisonV2::InvalidCurrentRecord);
    };
    if schema != u64::from(SCHEMA) {
        return DecodeDisposition::Unsupported { raw: raw.into() };
    }
    let Ok(MapOnly(wire)) = serde_json::from_slice(raw) else {
        return poison(raw, PhasePoisonV2::InvalidCurrentRecord);
    };
    let Ok(event) = into_domain(wire) else {
        return poison(raw, PhasePoisonV2::InvalidCurrentRecord);
    };
    match event_digest(&event) {
        Ok(digest) if digest == event.event_hash => DecodeDisposition::Accepted {
            value: event,
            raw: raw.into(),
        },
        Ok(_) => poison(raw, PhasePoisonV2::InvalidEventHash),
        Err(_) => poison(raw, PhasePoisonV2::InvalidCurrentRecord),
    }
}

pub(super) fn encode(event: &PhaseEventV2) -> Result<Vec<u8>, PhaseCodecError> {
    if event_digest(event)? != event.event_hash {
        return Err(PhaseCodecError);
    }
    let wire = into_wire(event);
    serde_json::to_vec(&wire).map_err(|_| PhaseCodecError)
}

fn poison(raw: &[u8], reason: PhasePoisonV2) -> PhaseEventDispositionV2 {
    DecodeDisposition::Poison {
        reason,
        raw: raw.into(),
    }
}

fn into_domain(wire: WireEvent) -> Result<PhaseEventV2, PhaseCodecError> {
    if wire.epoch != EPOCH || wire.schema != SCHEMA {
        return Err(PhaseCodecError);
    }
    let provider_text = component_text(&wire.provider_hex)?;
    let channel_text = component_text(&wire.channel_hex)?;
    let provider = ProviderIdentityV2::parse(&provider_text).map_err(|_| PhaseCodecError)?;
    let channel = ChannelIdentityV2::parse(&channel_text).map_err(|_| PhaseCodecError)?;
    canonical_component(&wire.provider_hex, provider.as_str())?;
    canonical_component(&wire.channel_hex, channel.as_str())?;
    Ok(PhaseEventV2 {
        identity: PhaseIdentityV2 {
            request_id: RequestIdV2::parse(&wire.request_id).map_err(|_| PhaseCodecError)?,
            attempt_id: AttemptIdV2::parse(&wire.attempt_id).map_err(|_| PhaseCodecError)?,
            provider,
            channel,
            nonce: NonceV2::parse(&wire.nonce).map_err(|_| PhaseCodecError)?,
        },
        sequence: wire.sequence,
        previous_hash: match wire.previous_hash {
            RequiredPreviousHash::Null(()) => None,
            RequiredPreviousHash::Hash(text) => Some(parse_digest(&text)?),
        },
        event_hash: parse_digest(&wire.event_hash)?,
        kind: match wire.kind.0 {
            WireKind::Empty(EmptyWireKind {
                phase: EmptyWirePhase::Bound,
            }) => PhaseKindV2::Bound,
            WireKind::Empty(EmptyWireKind {
                phase: EmptyWirePhase::Started,
            }) => PhaseKindV2::Started,
            WireKind::Terminal(TerminalWireKind { outcome, proof, .. }) => PhaseKindV2::Terminal {
                outcome,
                terminal_proof: SafeRelativeRefV2::parse(&proof).map_err(|_| PhaseCodecError)?,
            },
            WireKind::Receipt(ReceiptWireKind { receipt, .. }) => PhaseKindV2::Receipt {
                durable_receipt: SafeRelativeRefV2::parse(&receipt).map_err(|_| PhaseCodecError)?,
            },
        },
    })
}

fn component_text(encoded: &str) -> Result<String, PhaseCodecError> {
    String::from_utf8(hex::decode(encoded).map_err(|_| PhaseCodecError)?)
        .map_err(|_| PhaseCodecError)
}

fn canonical_component(encoded: &str, decoded: &str) -> Result<(), PhaseCodecError> {
    (hex::encode(decoded.as_bytes()).as_bytes() == encoded.as_bytes())
        .then_some(())
        .ok_or(PhaseCodecError)
}

fn parse_digest(text: &str) -> Result<EventDigestV2, PhaseCodecError> {
    let bytes: [u8; 32] = hex::decode(text)
        .map_err(|_| PhaseCodecError)?
        .try_into()
        .map_err(|_| PhaseCodecError)?;
    let digest = EventDigestV2(bytes);
    (hex::encode(digest.0).as_bytes() == text.as_bytes())
        .then_some(digest)
        .ok_or(PhaseCodecError)
}

fn into_wire(event: &PhaseEventV2) -> WireEvent {
    WireEvent {
        epoch: EPOCH,
        schema: SCHEMA,
        request_id: event.identity.request_id.as_str().to_owned(),
        attempt_id: event.identity.attempt_id.as_str().to_owned(),
        provider_hex: hex::encode(event.identity.provider.as_str().as_bytes()),
        channel_hex: hex::encode(event.identity.channel.as_str().as_bytes()),
        nonce: event.identity.nonce.as_str().to_owned(),
        sequence: event.sequence,
        previous_hash: match event.previous_hash {
            None => RequiredPreviousHash::Null(()),
            Some(hash) => RequiredPreviousHash::Hash(hex::encode(hash.0)),
        },
        event_hash: hex::encode(event.event_hash.0),
        kind: MapOnly(match &event.kind {
            PhaseKindV2::Bound => WireKind::Empty(EmptyWireKind {
                phase: EmptyWirePhase::Bound,
            }),
            PhaseKindV2::Started => WireKind::Empty(EmptyWireKind {
                phase: EmptyWirePhase::Started,
            }),
            PhaseKindV2::Terminal {
                outcome,
                terminal_proof,
            } => WireKind::Terminal(TerminalWireKind {
                phase: TerminalWirePhase::Terminal,
                outcome: *outcome,
                proof: terminal_proof.as_str().to_owned(),
            }),
            PhaseKindV2::Receipt { durable_receipt } => WireKind::Receipt(ReceiptWireKind {
                phase: ReceiptWirePhase::Receipt,
                receipt: durable_receipt.as_str().to_owned(),
            }),
        }),
    }
}

pub(super) fn event_digest(event: &PhaseEventV2) -> Result<EventDigestV2, PhaseCodecError> {
    let wire = into_wire(event);
    let semantic = WirePreimage {
        epoch: wire.epoch,
        schema: wire.schema,
        request_id: &wire.request_id,
        attempt_id: &wire.attempt_id,
        provider_hex: &wire.provider_hex,
        channel_hex: &wire.channel_hex,
        nonce: &wire.nonce,
        sequence: wire.sequence,
        previous_hash: &wire.previous_hash,
        kind: &wire.kind,
    };
    let bytes = serde_json::to_vec(&semantic).map_err(|_| PhaseCodecError)?;
    let mut digest = Sha256::new();
    digest.update(HASH_DOMAIN);
    digest.update(b"\0");
    digest.update(bytes);
    Ok(EventDigestV2(digest.finalize().into()))
}

#[cfg(test)]
const GOLDEN_BOUND: &str = r#"{"epoch":2,"schema":1,"request_id":"123e4567-e89b-12d3-a456-426614174000","attempt_id":"123e4567-e89b-12d3-a456-426614174001","provider_hex":"436c617564652fceb2","channel_hex":"7468726561643a303031","nonce":"nonce-1","sequence":0,"previous_hash":null,"event_hash":"f41459e3cad159b926461dffcf51a8dd48f4a563b3c5742b5815b5ad972aa4f0","kind":{"phase":"bound"}}"#;
#[cfg(test)]
pub(super) fn fixture(
    sequence: u64,
    previous_hash: Option<EventDigestV2>,
    kind: PhaseKindV2,
) -> PhaseEventV2 {
    let mut event = match decode(GOLDEN_BOUND.as_bytes()) {
        DecodeDisposition::Accepted { value, .. } => value,
        _ => panic!("golden fixture rejected"),
    };
    event.sequence = sequence;
    event.previous_hash = previous_hash;
    event.kind = kind;
    event.event_hash = event_digest(&event).unwrap();
    event
}

#[cfg(test)]
mod high_risk_recovery {
    use super::*;

    const HASH: &str = "f41459e3cad159b926461dffcf51a8dd48f4a563b3c5742b5815b5ad972aa4f0";
    const PREIMAGE: &str = r#"{"epoch":2,"schema":1,"request_id":"123e4567-e89b-12d3-a456-426614174000","attempt_id":"123e4567-e89b-12d3-a456-426614174001","provider_hex":"436c617564652fceb2","channel_hex":"7468726561643a303031","nonce":"nonce-1","sequence":0,"previous_hash":null,"kind":{"phase":"bound"}}"#;

    fn assert_poison(raw: &[u8], reason: PhasePoisonV2) {
        match std::panic::catch_unwind(|| decode(raw)).unwrap() {
            DecodeDisposition::Poison {
                reason: actual,
                raw: owned,
            } => assert_eq!((actual, &*owned), (reason, raw)),
            other => panic!(
                "expected Poison({reason:?}) for raw {:?}, got {other:?}",
                String::from_utf8_lossy(raw)
            ),
        }
    }

    fn assert_unsupported(raw: &[u8]) {
        assert_eq!(
            std::panic::catch_unwind(|| decode(raw)).unwrap(),
            DecodeDisposition::Unsupported { raw: raw.into() }
        );
    }

    fn terminal(outcome: TerminalOutcomeV2) -> PhaseKindV2 {
        PhaseKindV2::Terminal {
            outcome,
            terminal_proof: SafeRelativeRefV2::parse("proofs/terminal.json").unwrap(),
        }
    }

    fn assert_kind(kind: PhaseKindV2, nested: &str) {
        let event = fixture(0, None, kind);
        let wire = encode(&event).unwrap();
        assert!(wire.ends_with(format!(r#","kind":{nested}}}"#).as_bytes()));
        assert_eq!(
            decode(&wire),
            DecodeDisposition::Accepted {
                value: event,
                raw: wire.clone().into()
            }
        );
    }

    #[test]
    fn all_phase_kinds_round_trip_to_golden_canonical_wire() {
        let digest = Sha256::new()
            .chain_update(HASH_DOMAIN)
            .chain_update(b"\0")
            .chain_update(PREIMAGE)
            .finalize();
        assert_eq!(hex::encode(digest), HASH);
        assert_eq!(
            encode(&fixture(0, None, PhaseKindV2::Bound)).unwrap(),
            GOLDEN_BOUND.as_bytes()
        );
        assert_kind(PhaseKindV2::Bound, r#"{"phase":"bound"}"#);
        assert_kind(PhaseKindV2::Started, r#"{"phase":"started"}"#);
        for (outcome, tag) in [
            (TerminalOutcomeV2::Completed, "completed"),
            (TerminalOutcomeV2::RolledBack, "rolled_back"),
            (TerminalOutcomeV2::Failed, "failed"),
            (TerminalOutcomeV2::Cancelled, "cancelled"),
        ] {
            assert_kind(
                terminal(outcome),
                &format!(
                    r#"{{"phase":"terminal","outcome":"{tag}","proof":"proofs/terminal.json"}}"#
                ),
            );
        }
        assert_kind(
            PhaseKindV2::Receipt {
                durable_receipt: SafeRelativeRefV2::parse("receipts/한글.json").unwrap(),
            },
            r#"{"phase":"receipt","receipt":"receipts/한글.json"}"#,
        );
    }

    #[test]
    fn provider_and_channel_require_canonical_lower_utf8_hex() {
        for text in ["Cláude/β", "Cla\u{301}ude/β", "A", "a"] {
            let encoded = hex::encode(text);
            let mut wire = into_wire(&fixture(0, None, PhaseKindV2::Bound));
            wire.provider_hex = encoded.clone();
            wire.channel_hex = encoded;
            let event = into_domain(wire).unwrap();
            assert_eq!(event.identity.provider.as_str().as_bytes(), text.as_bytes());
            assert_eq!(event.identity.channel.as_str().as_bytes(), text.as_bytes());
        }
        assert_ne!(hex::encode("Cláude/β"), hex::encode("Cla\u{301}ude/β"));
        assert_ne!(hex::encode("A"), hex::encode("a"));
        for invalid in ["436C61756465", "0", "zz", "ff", "", "00"] {
            for provider in [true, false] {
                let mut wire = into_wire(&fixture(0, None, PhaseKindV2::Bound));
                if provider {
                    wire.provider_hex = invalid.into()
                } else {
                    wire.channel_hex = invalid.into()
                }
                assert!(into_domain(wire).is_err(), "accepted {invalid:?}");
            }
        }
    }

    #[test]
    fn unknown_fields_invalid_values_and_reencoding_preserve_exact_raw() {
        let mut reordered = format!(
            r#" {{"kind":{{"phase":"bound"}},"event_hash":"{HASH}","previous_hash":null,"sequence":0,"nonce":"nonce-1","channel_hex":"7468726561643a303031","provider_hex":"436c617564652fceb2","attempt_id":"123e4567-e89b-12d3-a456-426614174001","request_id":"123e4567-e89b-12d3-a456-426614174000","schema":1,"epoch":2}} "#
        );
        reordered.push('\n');
        let DecodeDisposition::Accepted { value, raw } = decode(reordered.as_bytes()) else {
            panic!("not accepted")
        };
        assert_eq!(&*raw, reordered.as_bytes());
        assert_eq!(encode(&value).unwrap(), GOLDEN_BOUND.as_bytes());
        let started =
            String::from_utf8(encode(&fixture(0, None, PhaseKindV2::Started)).unwrap()).unwrap();
        let terminal = String::from_utf8(
            encode(&fixture(0, None, terminal(TerminalOutcomeV2::Completed))).unwrap(),
        )
        .unwrap();
        let upper_hash = HASH.to_uppercase();
        for (source, from, to) in [
            (GOLDEN_BOUND, "{", r#"{"epoch":2,"#),
            (GOLDEN_BOUND, "{", r#"{"owner":"x","#),
            (
                GOLDEN_BOUND,
                r#"{"phase":"bound"}"#,
                r#"{"phase":"bound","extra":1}"#,
            ),
            (
                GOLDEN_BOUND,
                r#"{"phase":"bound"}"#,
                r#"{"phase":"bound","phase":"bound"}"#,
            ),
            (
                &started,
                r#"{"phase":"started"}"#,
                r#"{"phase":"started","extra":1}"#,
            ),
            (GOLDEN_BOUND, r#""previous_hash":null,"#, ""),
            (GOLDEN_BOUND, "123e4567", "123E4567"),
            (GOLDEN_BOUND, "nonce-1", r#"bad\u0000nonce"#),
            (&terminal, "proofs/terminal.json", "../proof"),
            (GOLDEN_BOUND, "bound", "binding"),
            (&terminal, "completed", "unknown"),
            (GOLDEN_BOUND, HASH, &upper_hash),
        ] {
            let invalid = source.replacen(from, to, 1);
            assert_poison(invalid.as_bytes(), PhasePoisonV2::InvalidCurrentRecord);
        }
        for kind in [
            r#"["bound"]"#,
            r#"["started"]"#,
            r#"["terminal","completed","proofs/x"]"#,
            r#"["receipt","r"]"#,
        ] {
            let invalid = GOLDEN_BOUND.replace(r#"{"phase":"bound"}"#, kind);
            assert_poison(invalid.as_bytes(), PhasePoisonV2::InvalidCurrentRecord);
        }
        assert_poison(
            GOLDEN_BOUND.replace(HASH, &"0".repeat(64)).as_bytes(),
            PhasePoisonV2::InvalidEventHash,
        );
    }

    #[test]
    fn malformed_and_unknown_versions_classify_without_panicking_or_losing_raw() {
        let deep = format!("{}0{}", "[".repeat(140), "]".repeat(140)).into_bytes();
        for raw in [b"".as_slice(), b"{", b"\xff", b"{}{}"] {
            assert_poison(raw, PhasePoisonV2::MalformedJson);
        }
        assert_poison(&deep, PhasePoisonV2::InvalidCurrentRecord);
        for raw in [
            "null",
            "{}",
            "[2]",
            "[3]",
            r#"{"epoch":2,"epoch":2}"#,
            r#"{"epoch":"2"}"#,
            r#"{"epoch":2}"#,
            r#"{"epoch":2,"schema":1,"schema":1}"#,
            r#"{"epoch":2,"schema":"1"}"#,
        ] {
            assert_poison(raw.as_bytes(), PhasePoisonV2::InvalidCurrentRecord);
        }
        for raw in [r#"{"epoch":3}"#, r#"{"epoch":2,"schema":2}"#] {
            assert_unsupported(raw.as_bytes());
        }
        let odd = br#"[{"epoch":2},true,null,"odd"]"#;
        assert_poison(odd, PhasePoisonV2::InvalidCurrentRecord);
    }
}
