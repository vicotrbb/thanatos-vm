//! The user-mode network: a smoltcp interface whose hardware is the guest's virtio-net queues,
//! NAT to ordinary host sockets, the outbound policy and port forwards.
pub mod policy;
mod stack;
mod wire;
pub use stack::{Net, Settings};
