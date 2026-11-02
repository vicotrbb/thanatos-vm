//! The machine: boot loaders, the virtio-mmio bus, devices, display and control API.
pub mod api;
pub mod boot;
pub mod config;
pub mod console;
pub mod devices;
pub mod machine;
pub mod mmio;
pub mod pl011;
pub mod serial;
pub mod server;
pub use thanatos_hv::GuestMemory;
