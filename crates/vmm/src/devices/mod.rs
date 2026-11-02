//! Virtio devices. Each is one file implementing [`crate::mmio::Device`].
pub mod blk;
pub mod rng;
pub mod vsock;
