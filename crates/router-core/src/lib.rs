mod capabilities;
mod circuit;
mod config;
mod error;
mod observability;
mod router;

pub use capabilities::{Capabilities, Requirements};
pub use circuit::{CircuitBreaker, CircuitState};
pub use config::{
    AliasConfig, AppConfig, AuthConfig, ConfigSource, ModelConfig, ProviderConfig, ProviderKind,
    ProviderProtocol, STANDARD_PROVIDERS, ServerConfig, StandardProvider, WebUiConfig,
};
pub use error::RouterError;
pub use observability::{
    DIRECT_ROUTE, RouteLabels, UNDECLARED_MODEL, UpstreamFailure, is_safe_request_id,
};
pub use router::{
    AdapterMap, AuthStore, EventStream, ProviderAdapter, ProviderEvent, ProviderStream,
    RouteFailure, RoutedStream, Router, RuntimeRouterKeyInfo,
};
