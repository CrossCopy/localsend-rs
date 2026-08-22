pub mod http;
pub mod multicast;
pub mod traits;

pub use http::{HttpDiscovery, local_ipv4_addresses, local_ipv4_interfaces};
pub use multicast::{MulticastConfig, MulticastDiscovery};
pub use traits::Discovery;
