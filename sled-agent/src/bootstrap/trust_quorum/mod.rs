//! The entry point for the trust quorum code
//!
//! The Trust quorum relies on IPv6 multicast discovery, rack secret handling,
//! and the SPDM protocol.
//!
//! Below is the trust quorum protocol for share retrieval over TCP.
//!
//! The following protocol is shown between two sleds only, but multicast
//! discovery and share requests will continue to run until enough shares
//! have been received to recreate the rack secret.
//!
//! Sled1                                      Sled2
//! =====                                      =====
//!  ||  ------- Multicast Discovery -------->  ||
//!  ||                                         ||
//!  ||  <-------- Respond with IP -----------  ||
//!  ||                                         ||
//!  ||  ---- Connect to TrustQuorum port --->  ||
//!  ||                                         ||
//!  ||  --------- SPDM Requests ------------>  ||
//!  ||                                         ||
//!  ||  <-------- SPDM Responses ------------  ||
//!  ||                                         ||
//!  ||  ----- SPDM Channel Established ------  ||
//!  ||                                         ||
//!  ||  --------- Request Share ------------>  ||
//!  ||                                         ||
//!  ||  <----------- Share ------------------  ||
//!

mod rack_secret;
mod server;
