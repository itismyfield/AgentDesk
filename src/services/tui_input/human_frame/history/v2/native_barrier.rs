//! A source observation can withhold execution attribution without changing delivery settlement.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Barrier {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) membership_range: [u64; 2],
    pub(crate) membership_row: String,
    pub(crate) foreign_range: [u64; 2],
    pub(crate) foreign_row: String,
    pub(crate) foreign_hashes: Vec<String>,
    pub(crate) through: u64,
    pub(crate) registrations_fingerprint: String,
}

impl Barrier {
    pub(crate) fn validate(&self, namespace: &str, ticket: &Ticket) -> Result<(), String> {
        let source = ticket.source.as_ref().ok_or("barrier source missing")?;
        source.validate(&ticket.key)?;
        if self.namespace != namespace
            || self.ticket != *ticket
            || self.membership_row.is_empty()
            || self.foreign_row.is_empty()
            || self.membership_row == self.foreign_row
            || self.foreign_hashes.is_empty()
            || self.membership_range[0] < source.offset
            || self.membership_range[0] >= self.membership_range[1]
            || self.membership_range[1] > self.foreign_range[0]
            || self.foreign_range[0] >= self.foreign_range[1]
            || self.foreign_range[1] > self.through
            || self.registrations_fingerprint.is_empty()
        {
            return Err("foreign prompt barrier identity/range mismatch".into());
        }
        Ok(())
    }
}

impl Settlement {
    pub(crate) fn barrier(&self, namespace: &str) -> Result<Option<Barrier>, String> {
        self.extra
            .get("foreign_prompt_barrier")
            .map(|v| {
                let proof: Barrier =
                    serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
                proof.validate(namespace, &self.ticket)?;
                if self.extra.get("any_key") != Some(&Value::Bool(true)) {
                    return Err("barrier has no key receipt".into());
                }
                Ok(proof)
            })
            .transpose()
    }
}

impl Registry {
    pub(crate) fn record_native_barrier(&self, proof: &Barrier) -> Result<bool, String> {
        proof.validate(&proof.namespace, &proof.ticket)?;
        self.native_transaction_checked(
            &proof.namespace,
            &proof.ticket.key,
            |snapshot| {
                let (registration, _) = snapshot
                    .locate(&proof.namespace, &proof.ticket.key)
                    .ok_or("barrier registration missing")?;
                let current = registration
                    .native_settlement
                    .as_ref()
                    .ok_or("native ticket missing")?;
                if current.ticket != proof.ticket
                    || current.outcome.is_some()
                    || current.extra.get("any_key") != Some(&Value::Bool(true))
                {
                    return Err("barrier registration settled or unkeyed".into());
                }
                if let Some(old) = current.barrier(&proof.namespace)? {
                    return if old == *proof {
                        Ok(())
                    } else {
                        Err("barrier already captured".into())
                    };
                }
                let source = proof
                    .ticket
                    .source
                    .as_ref()
                    .ok_or("barrier source missing")?;
                let registrations: Vec<_> = evidence_registrations(snapshot, true)?
                    .into_iter()
                    .filter(|r| {
                        r.ticket.channel_id == proof.ticket.channel_id
                            && r.ticket.source.as_ref().is_none_or(|s| {
                                s.provider == source.provider
                                    && s.session_id == source.session_id
                                    && s.transcript == source.transcript
                                    && s.file == source.file
                            })
                    })
                    .collect();
                if registrations_fingerprint(&registrations)? != proof.registrations_fingerprint
                    || !registrations.iter().any(|r| {
                        r.namespace == proof.namespace
                            && r.ticket == proof.ticket
                            && r.receipts.contains(&proof.membership_row)
                    })
                    || registrations.iter().any(|r| {
                        r.ticket.key["attempt"]["target"] == source.target
                            && (r.receipts.contains(&proof.foreign_row)
                                || ((r.ticket.source.is_none() || r.receipts.is_empty())
                                    && proof.foreign_hashes.contains(&r.frame_hash)))
                    })
                {
                    return Err("barrier classification changed".into());
                }
                Ok(())
            },
            |registration| {
                let current = registration
                    .native_settlement
                    .as_mut()
                    .ok_or("native ticket missing")?;
                if current.barrier(&proof.namespace)?.is_some() {
                    return Ok(false);
                }
                current.extra.insert(
                    "foreign_prompt_barrier".into(),
                    serde_json::to_value(proof).map_err(|e| e.to_string())?,
                );
                Ok(true)
            },
        )
    }
}
