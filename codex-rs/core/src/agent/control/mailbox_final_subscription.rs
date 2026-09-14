//! Durable mailbox intent projects into exact live observers, never from audit history.

#[path = "mailbox_final_subscription_acceptance.rs"]
pub(super) mod acceptance;
#[path = "mailbox_final_subscription_binding.rs"]
mod binding;
#[path = "mailbox_final_subscription_delivery.rs"]
mod delivery;
#[path = "mailbox_final_subscription_policy.rs"]
mod policy;
#[path = "mailbox_final_subscription_recovery.rs"]
mod recovery;
