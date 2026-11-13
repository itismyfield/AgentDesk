//! Consultation dispatch recording used by the engine `recordConsultationDispatch` op.
//! Its `dispatched` entry write never owes run completion, so the DB body stays whole.

pub(crate) use crate::db::auto_queue::record_consultation_dispatch_on_pg;
