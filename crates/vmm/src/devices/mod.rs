//! Virtio devices. Each is one file implementing [`crate::mmio::Device`].
pub mod blk;
pub mod gpu;
pub mod input;
pub mod net;
pub mod rng;
pub mod vsock;
