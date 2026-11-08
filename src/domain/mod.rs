//! Pure transition mechanisms. Inputs (including request identities) are explicit;
//! orchestration, I/O and GPUI notifications stay with their existing owners.

pub(crate) mod choice;
pub(crate) mod history;
pub(crate) mod requests;
