//! The production socket launch adapter with scripted OS and peer reads, for the composed Discord scenarios
//! upper-crate tests run (KRX-TS).

use super::*;
use crate::services::session_host::herdr::provenance::{ProcessStart, StartIdentity};
use std::time::SystemTime;

// The composed Discord scenario keeps the production socket launch adapter and scripts only OS/peer reads.
impl SocketHerdrLaunchHost {
    pub(crate) fn socket_rig_for_test(endpoint: HerdrEndpoint, os: Box<dyn ProcessOs>) -> Self {
        let mut host = Self::new(vec![endpoint], HerdrSocketConfig::default());
        host.os = os;
        host.read_restore = |transport, _| match transport.server_witness() {
            Ok(witness) => RestoreResume::Off { witness },
            Err(reason) => RestoreResume::Unverified(reason),
        };
        host.sources.clear();
        let (endpoint, transport) = host.endpoints.pop().unwrap();
        let transport = Arc::try_unwrap(transport)
            .unwrap_or_else(|_| panic!("unshared fixture transport"))
            .with_peer_reader(Box::new(|_| {
                Ok((
                    7,
                    ProcessStart {
                        identity: StartIdentity::Darwin {
                            seconds: 1_000,
                            micros: 0,
                        },
                        wall_clock: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
                    },
                ))
            }));
        host.endpoints.push((endpoint, Arc::new(transport)));
        host
    }
}
