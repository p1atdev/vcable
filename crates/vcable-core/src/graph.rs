use crate::{ChannelMatrix, MatrixError};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, GraphError> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > 255
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
                {
                    return Err(GraphError::InvalidIdentifier(value));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

string_id!(EndpointId);
string_id!(RouteId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointDirection {
    Source,
    Sink,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ClockDomain(String);

impl ClockDomain {
    pub fn new(value: impl Into<String>) -> Result<Self, GraphError> {
        let value = value.into();
        if value.is_empty() || value.len() > 255 {
            return Err(GraphError::InvalidIdentifier(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointDescriptor {
    pub id: EndpointId,
    pub node: String,
    pub direction: EndpointDirection,
    pub channels: usize,
    pub sample_rate: u32,
    pub clock_domain: ClockDomain,
    /// Whether this device inherently feeds its sink back to its source.
    pub is_loopback: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub id: RouteId,
    pub source: EndpointId,
    pub sink: EndpointId,
    pub gain: f32,
    pub matrix: ChannelMatrix,
}

#[derive(Clone, Debug, Default)]
pub struct RoutingGraph {
    endpoints: BTreeMap<EndpointId, EndpointDescriptor>,
    routes: BTreeMap<RouteId, Route>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GraphError {
    InvalidIdentifier(String),
    ZeroChannels,
    InvalidSampleRate(u32),
    EndpointExists(EndpointId),
    EndpointNotFound(EndpointId),
    EndpointInUse(EndpointId),
    RouteExists(RouteId),
    RouteNotFound(RouteId),
    SourceDirectionRequired(EndpointId),
    SinkDirectionRequired(EndpointId),
    MatrixInputMismatch {
        expected: usize,
        actual: usize,
    },
    MatrixOutputMismatch {
        expected: usize,
        actual: usize,
    },
    GainNotFinite,
    CycleDetected {
        source_node: String,
        sink_node: String,
    },
    Matrix(MatrixError),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier(value) => write!(f, "invalid identifier: {value}"),
            Self::ZeroChannels => write!(f, "endpoint channel count must be greater than zero"),
            Self::InvalidSampleRate(rate) => write!(f, "invalid sample rate: {rate}"),
            Self::EndpointExists(id) => write!(f, "endpoint already exists: {id}"),
            Self::EndpointNotFound(id) => write!(f, "endpoint not found: {id}"),
            Self::EndpointInUse(id) => write!(f, "endpoint is referenced by a route: {id}"),
            Self::RouteExists(id) => write!(f, "route already exists: {id}"),
            Self::RouteNotFound(id) => write!(f, "route not found: {id}"),
            Self::SourceDirectionRequired(id) => write!(f, "endpoint is not a source: {id}"),
            Self::SinkDirectionRequired(id) => write!(f, "endpoint is not a sink: {id}"),
            Self::MatrixInputMismatch { expected, actual } => {
                write!(f, "matrix has {actual} inputs; source has {expected}")
            }
            Self::MatrixOutputMismatch { expected, actual } => {
                write!(f, "matrix has {actual} outputs; sink has {expected}")
            }
            Self::GainNotFinite => write!(f, "route gain must be finite"),
            Self::CycleDetected {
                source_node,
                sink_node,
            } => {
                write!(f, "route from {source_node} to {sink_node} creates a cycle")
            }
            Self::Matrix(error) => error.fmt(f),
        }
    }
}

impl Error for GraphError {}

impl From<MatrixError> for GraphError {
    fn from(value: MatrixError) -> Self {
        Self::Matrix(value)
    }
}

impl RoutingGraph {
    pub fn endpoints(&self) -> impl Iterator<Item = &EndpointDescriptor> {
        self.endpoints.values()
    }

    pub fn routes(&self) -> impl Iterator<Item = &Route> {
        self.routes.values()
    }

    pub fn endpoint(&self, id: &EndpointId) -> Option<&EndpointDescriptor> {
        self.endpoints.get(id)
    }

    pub fn add_endpoint(&mut self, endpoint: EndpointDescriptor) -> Result<(), GraphError> {
        if endpoint.channels == 0 {
            return Err(GraphError::ZeroChannels);
        }
        if endpoint.sample_rate < 8_000 || endpoint.sample_rate > 768_000 {
            return Err(GraphError::InvalidSampleRate(endpoint.sample_rate));
        }
        if endpoint.node.is_empty() || endpoint.node.len() > 255 {
            return Err(GraphError::InvalidIdentifier(endpoint.node));
        }
        if self.endpoints.contains_key(&endpoint.id) {
            return Err(GraphError::EndpointExists(endpoint.id));
        }
        self.endpoints.insert(endpoint.id.clone(), endpoint);
        Ok(())
    }

    pub fn remove_endpoint(&mut self, id: &EndpointId) -> Result<EndpointDescriptor, GraphError> {
        if self
            .routes
            .values()
            .any(|route| route.source == *id || route.sink == *id)
        {
            return Err(GraphError::EndpointInUse(id.clone()));
        }
        self.endpoints
            .remove(id)
            .ok_or_else(|| GraphError::EndpointNotFound(id.clone()))
    }

    pub fn add_route(&mut self, route: Route) -> Result<(), GraphError> {
        if self.routes.contains_key(&route.id) {
            return Err(GraphError::RouteExists(route.id));
        }
        let source = self
            .endpoints
            .get(&route.source)
            .ok_or_else(|| GraphError::EndpointNotFound(route.source.clone()))?;
        let sink = self
            .endpoints
            .get(&route.sink)
            .ok_or_else(|| GraphError::EndpointNotFound(route.sink.clone()))?;
        if source.direction != EndpointDirection::Source {
            return Err(GraphError::SourceDirectionRequired(source.id.clone()));
        }
        if sink.direction != EndpointDirection::Sink {
            return Err(GraphError::SinkDirectionRequired(sink.id.clone()));
        }
        if route.matrix.input_channels() != source.channels {
            return Err(GraphError::MatrixInputMismatch {
                expected: source.channels,
                actual: route.matrix.input_channels(),
            });
        }
        if route.matrix.output_channels() != sink.channels {
            return Err(GraphError::MatrixOutputMismatch {
                expected: sink.channels,
                actual: route.matrix.output_channels(),
            });
        }
        if !route.gain.is_finite() {
            return Err(GraphError::GainNotFinite);
        }
        if self.path_exists(&sink.node, &source.node) {
            return Err(GraphError::CycleDetected {
                source_node: source.node.clone(),
                sink_node: sink.node.clone(),
            });
        }
        self.routes.insert(route.id.clone(), route);
        Ok(())
    }

    pub fn remove_route(&mut self, id: &RouteId) -> Result<Route, GraphError> {
        self.routes
            .remove(id)
            .ok_or_else(|| GraphError::RouteNotFound(id.clone()))
    }

    fn path_exists(&self, from: &str, target: &str) -> bool {
        if !self.node_is_loopback(from) {
            return false;
        }
        let mut pending = vec![from];
        let mut visited = BTreeSet::new();
        while let Some(node) = pending.pop() {
            if node == target {
                return true;
            }
            if !visited.insert(node.to_owned()) {
                continue;
            }
            for route in self.routes.values() {
                let Some(source) = self.endpoints.get(&route.source) else {
                    continue;
                };
                if source.node != node {
                    continue;
                }
                if let Some(sink) = self.endpoints.get(&route.sink)
                    && self.node_is_loopback(&sink.node)
                {
                    pending.push(&sink.node);
                }
            }
        }
        false
    }

    fn node_is_loopback(&self, node: &str) -> bool {
        self.endpoints
            .values()
            .any(|endpoint| endpoint.node == node && endpoint.is_loopback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str, node: &str, direction: EndpointDirection) -> EndpointDescriptor {
        EndpointDescriptor {
            id: EndpointId::new(id).unwrap(),
            node: node.to_owned(),
            direction,
            channels: 2,
            sample_rate: 48_000,
            clock_domain: ClockDomain::new(node).unwrap(),
            is_loopback: true,
        }
    }

    fn route(id: &str, source: &str, sink: &str) -> Route {
        Route {
            id: RouteId::new(id).unwrap(),
            source: EndpointId::new(source).unwrap(),
            sink: EndpointId::new(sink).unwrap(),
            gain: 1.0,
            matrix: ChannelMatrix::identity(2).unwrap(),
        }
    }

    #[test]
    fn rejects_feedback_cycles() {
        let mut graph = RoutingGraph::default();
        for item in [
            endpoint("a.in", "a", EndpointDirection::Source),
            endpoint("a.out", "a", EndpointDirection::Sink),
            endpoint("b.in", "b", EndpointDirection::Source),
            endpoint("b.out", "b", EndpointDirection::Sink),
        ] {
            graph.add_endpoint(item).unwrap();
        }
        graph.add_route(route("a-to-b", "a.in", "b.out")).unwrap();
        assert!(matches!(
            graph.add_route(route("b-to-a", "b.in", "a.out")),
            Err(GraphError::CycleDetected { .. })
        ));
    }

    #[test]
    fn refuses_to_remove_an_endpoint_used_by_a_route() {
        let mut graph = RoutingGraph::default();
        graph
            .add_endpoint(endpoint("a.in", "a", EndpointDirection::Source))
            .unwrap();
        graph
            .add_endpoint(endpoint("b.out", "b", EndpointDirection::Sink))
            .unwrap();
        graph.add_route(route("route", "a.in", "b.out")).unwrap();
        assert!(matches!(
            graph.remove_endpoint(&EndpointId::new("a.in").unwrap()),
            Err(GraphError::EndpointInUse(_))
        ));
    }

    #[test]
    fn allows_input_to_output_on_a_non_loopback_device() {
        let mut graph = RoutingGraph::default();
        let mut input = endpoint("physical.in", "physical", EndpointDirection::Source);
        input.is_loopback = false;
        let mut output = endpoint("physical.out", "physical", EndpointDirection::Sink);
        output.is_loopback = false;
        graph.add_endpoint(input).unwrap();
        graph.add_endpoint(output).unwrap();
        graph
            .add_route(route("monitor", "physical.in", "physical.out"))
            .unwrap();
    }
}
