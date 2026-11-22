#![forbid(unsafe_code)]

mod client;
mod error;
mod kubeconfig;
mod resource;
mod transport;
mod types;
mod watch;

#[cfg(not(target_family = "wasm"))]
mod http;

pub use client::{Client, Limits, List, apply_request, get_request, list_request, watch_request};
pub use error::Error;
#[cfg(not(target_family = "wasm"))]
pub use http::{HttpBody, HttpTransport};
pub use kubeconfig::{Auth, Credentials, base64_decode, parse_yaml};
pub use resource::{
    CONFIG_MAP, DAEMON_SET, DEPLOYMENT, LEASE, NAME_BYTES_MAX, NODE, Resource, SERVICE,
    check_field_manager, check_name,
};
pub use transport::{APPLY_PATCH, Chunks, Request, Response, Transport, Verb};
pub use types::{
    ConfigMap, Container, DaemonSet, DaemonSetSpec, Deployment, DeploymentSpec, LabelSelector,
    Lease, LeaseSpec, ManagedFieldsEntry, Node, NodeCondition, NodeStatus, ObjectMeta, PodSpec,
    PodTemplateSpec, Service, ServicePort, ServiceSpec, object_name, object_resource_version,
};
pub use watch::{Event, Watch, decode_status};
