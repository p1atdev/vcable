//! Real-time-safe primitives and validated routing configuration for VCable.

mod graph;
mod matrix;
mod ring;

pub use graph::{
    ClockDomain, EndpointDescriptor, EndpointDirection, EndpointId, GraphError, Route, RouteId,
    RoutingGraph,
};
pub use matrix::{ChannelMatrix, MatrixError};
pub use ring::{Consumer, Producer, RingBufferError, spsc_ring_buffer};
