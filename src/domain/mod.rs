//! Pure transition mechanisms. Inputs (including request identities and the
//! current time) are explicit; orchestration, I/O and GPUI notifications stay
//! with their existing owners. Nothing here imports GPUI or the rest of the
//! crate, so these tests also run standalone (see AGENTS.md).

pub(crate) mod choice;
pub(crate) mod fetch;
pub(crate) mod history;
pub(crate) mod requests;
pub(crate) mod supervisor;
