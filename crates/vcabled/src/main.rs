use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use vcable_core::{
    ChannelMatrix, ClockDomain, EndpointDescriptor, EndpointDirection, EndpointId, Route, RouteId,
    RoutingGraph,
};
use vcable_coreaudio::{
    AudioDevice, AudioRoute, AudioRouter, create_virtual_device, delete_virtual_device,
    list_devices,
};
use vcable_protocol::{Message, Request, Response, read_message, write_message};

fn main() {
    if let Err(error) = run() {
        eprintln!("vcabled: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let options = Options::parse(env::args().skip(1))?;
    if options.socket.exists() {
        return Err(format!(
            "socket already exists: {}; remove it only after verifying no daemon is running",
            options.socket.display()
        )
        .into());
    }
    let mut state = DaemonState::load(&options.state)?;
    state.refresh_endpoints()?;
    state.restore_routes()?;
    state.restart_router()?;

    let listener = UnixListener::bind(&options.socket)?;
    fs::set_permissions(&options.socket, fs::Permissions::from_mode(0o600))?;
    let _socket_guard = SocketGuard(options.socket.clone());
    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => match serve_connection(&mut stream, &mut state) {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => eprintln!("vcabled: client error: {error}"),
            },
            Err(error) => eprintln!("vcabled: accept error: {error}"),
        }
    }
    Ok(())
}

fn serve_connection(
    stream: &mut UnixStream,
    state: &mut DaemonState,
) -> Result<bool, Box<dyn Error>> {
    let request = read_message::<_, Request>(stream)?;
    let shutdown = request == Request::Shutdown;
    let response = match state.handle(request) {
        Ok(message) => Response::Ok(message),
        Err(error) => Response::Error {
            code: error.code().to_owned(),
            message: error.to_string(),
        },
    };
    write_message(stream, &response)?;
    Ok(shutdown)
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Debug)]
struct Options {
    socket: PathBuf,
    state: PathBuf,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, Box<dyn Error>> {
        let mut socket = None;
        let mut state = None;
        while let Some(argument) = args.next() {
            let destination = match argument.as_str() {
                "--socket" => &mut socket,
                "--state" => &mut state,
                "--help" | "-h" => {
                    println!("usage: vcabled --socket PATH --state PATH");
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown argument: {argument}").into()),
            };
            *destination = Some(PathBuf::from(
                args.next()
                    .ok_or_else(|| format!("{argument} requires a path"))?,
            ));
        }
        Ok(Self {
            socket: socket.ok_or("--socket is required")?,
            state: state.ok_or("--state is required")?,
        })
    }
}

#[derive(Clone)]
struct SavedRoute {
    id: String,
    source: String,
    sink: String,
    gain_millidb: i32,
    matrix_millionths: Vec<i32>,
}

struct DaemonState {
    graph: RoutingGraph,
    routes: BTreeMap<String, SavedRoute>,
    endpoint_devices: BTreeMap<String, AudioDevice>,
    router: Option<AudioRouter>,
    state_path: PathBuf,
}

impl DaemonState {
    fn load(path: &Path) -> Result<Self, Box<dyn Error>> {
        let mut routes = BTreeMap::new();
        if path.exists() {
            let file = File::open(path)?;
            for (line_number, line) in BufReader::new(file).lines().enumerate() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                let request = Request::decode(&line)
                    .map_err(|error| format!("{}:{}: {error}", path.display(), line_number + 1))?;
                let Request::Connect {
                    id,
                    source,
                    sink,
                    gain_millidb,
                    matrix_millionths,
                } = request
                else {
                    return Err(format!(
                        "{}:{}: state contains a non-route command",
                        path.display(),
                        line_number + 1
                    )
                    .into());
                };
                if routes.contains_key(&id) {
                    return Err(format!(
                        "{}:{}: duplicate route {id}",
                        path.display(),
                        line_number + 1
                    )
                    .into());
                }
                routes.insert(
                    id.clone(),
                    SavedRoute {
                        id,
                        source,
                        sink,
                        gain_millidb,
                        matrix_millionths,
                    },
                );
            }
        }
        Ok(Self {
            graph: RoutingGraph::default(),
            routes,
            endpoint_devices: BTreeMap::new(),
            router: None,
            state_path: path.to_owned(),
        })
    }

    fn handle(&mut self, request: Request) -> Result<String, DaemonError> {
        match request {
            Request::Ping => Ok("pong".to_owned()),
            Request::Status => Ok(self.status()),
            Request::Shutdown => {
                self.router = None;
                Ok("shutting down".to_owned())
            }
            Request::CreateDevice {
                id,
                name,
                input_channels,
                output_channels,
                sample_rate,
            } => {
                validate_device_id(&id)?;
                create_virtual_device(&id, &name, input_channels, output_channels, sample_rate)?;
                self.refresh_endpoints()?;
                self.restore_routes()?;
                Ok(format!("created {id}"))
            }
            Request::DeleteDevice { id } => {
                validate_device_id(&id)?;
                let node = format!("dev.vcable.device.{id}");
                let is_referenced = self.routes.values().any(|route| {
                    [&route.source, &route.sink].into_iter().any(|endpoint| {
                        self.endpoint_devices
                            .get(endpoint)
                            .is_some_and(|device| device.uid == node)
                    })
                });
                if is_referenced {
                    return Err(DaemonError::DeviceInUse(id));
                }
                delete_virtual_device(&id)?;
                self.refresh_endpoints()?;
                self.restore_routes()?;
                Ok(format!("deleted {id}"))
            }
            Request::Connect {
                id,
                source,
                sink,
                gain_millidb,
                matrix_millionths,
            } => {
                let saved = SavedRoute {
                    id: id.clone(),
                    source,
                    sink,
                    gain_millidb,
                    matrix_millionths,
                };
                let route = self.build_route(&saved)?;
                self.graph.add_route(route)?;
                if self.routes.insert(id.clone(), saved).is_some() {
                    self.restore_routes()?;
                    return Err(DaemonError::RouteExists(id));
                }
                if let Err(error) = self.save_routes() {
                    self.routes.remove(&id);
                    self.restore_routes()?;
                    return Err(error);
                }
                if let Err(error) = self.restart_router() {
                    self.routes.remove(&id);
                    self.save_routes()?;
                    self.refresh_endpoints()?;
                    self.restore_routes()?;
                    self.restart_router()?;
                    return Err(error);
                }
                Ok(format!("connected {id}"))
            }
            Request::Disconnect { id } => {
                let previous = self
                    .routes
                    .remove(&id)
                    .ok_or_else(|| DaemonError::RouteNotFound(id.clone()))?;
                if let Err(error) = self.save_routes() {
                    self.routes.insert(id.clone(), previous);
                    return Err(error);
                }
                self.refresh_endpoints()?;
                self.restore_routes()?;
                if let Err(error) = self.restart_router() {
                    self.routes.insert(id.clone(), previous);
                    self.save_routes()?;
                    self.refresh_endpoints()?;
                    self.restore_routes()?;
                    self.restart_router()?;
                    return Err(error);
                }
                Ok(format!("disconnected {id}"))
            }
        }
    }

    fn refresh_endpoints(&mut self) -> Result<(), DaemonError> {
        let mut graph = RoutingGraph::default();
        let mut endpoint_devices = BTreeMap::new();
        for device in list_devices()? {
            let clock = ClockDomain::new(format!("coreaudio:{}", device.object_id))?;
            if device.input_channels > 0 {
                let id = endpoint_id(&device.uid, "input");
                graph.add_endpoint(EndpointDescriptor {
                    id: EndpointId::new(id.clone())?,
                    node: device.uid.clone(),
                    direction: EndpointDirection::Source,
                    channels: device.input_channels as usize,
                    sample_rate: device.sample_rate,
                    clock_domain: clock.clone(),
                    is_loopback: device.is_virtual,
                })?;
                endpoint_devices.insert(id, device.clone());
            }
            if device.output_channels > 0 {
                let id = endpoint_id(&device.uid, "output");
                graph.add_endpoint(EndpointDescriptor {
                    id: EndpointId::new(id.clone())?,
                    node: device.uid.clone(),
                    direction: EndpointDirection::Sink,
                    channels: device.output_channels as usize,
                    sample_rate: device.sample_rate,
                    clock_domain: clock,
                    is_loopback: device.is_virtual,
                })?;
                endpoint_devices.insert(id, device);
            }
        }
        self.graph = graph;
        self.endpoint_devices = endpoint_devices;
        Ok(())
    }

    fn restore_routes(&mut self) -> Result<(), DaemonError> {
        let routes = self.routes.values().cloned().collect::<Vec<_>>();
        for saved in routes {
            self.graph.add_route(self.build_route(&saved)?)?;
        }
        Ok(())
    }

    fn build_route(&self, saved: &SavedRoute) -> Result<Route, DaemonError> {
        let source_id = EndpointId::new(saved.source.clone())?;
        let sink_id = EndpointId::new(saved.sink.clone())?;
        let source = self
            .graph
            .endpoint(&source_id)
            .ok_or_else(|| DaemonError::EndpointNotFound(saved.source.clone()))?;
        let sink = self
            .graph
            .endpoint(&sink_id)
            .ok_or_else(|| DaemonError::EndpointNotFound(saved.sink.clone()))?;
        let coefficients = if saved.matrix_millionths.is_empty() {
            if source.channels != sink.channels {
                return Err(DaemonError::MatrixRequired {
                    inputs: source.channels,
                    outputs: sink.channels,
                });
            }
            ChannelMatrix::identity(source.channels)?
        } else {
            ChannelMatrix::new(
                sink.channels,
                source.channels,
                saved
                    .matrix_millionths
                    .iter()
                    .map(|value| *value as f32 / 1_000_000.0)
                    .collect(),
            )?
        };
        let gain_db = saved.gain_millidb as f32 / 1_000.0;
        Ok(Route {
            id: RouteId::new(saved.id.clone())?,
            source: source_id,
            sink: sink_id,
            gain: 10.0_f32.powf(gain_db / 20.0),
            matrix: coefficients,
        })
    }

    fn status(&self) -> String {
        let mut lines = Vec::new();
        for endpoint in self.graph.endpoints() {
            lines.push(format!(
                "endpoint\t{}\t{:?}\t{}ch\t{}Hz\t{}",
                endpoint.id,
                endpoint.direction,
                endpoint.channels,
                endpoint.sample_rate,
                endpoint.clock_domain.as_str()
            ));
        }
        for route in self.graph.routes() {
            lines.push(format!(
                "route\t{}\t{}\t{}\t{:.6}",
                route.id, route.source, route.sink, route.gain
            ));
        }
        if let Some(router) = &self.router {
            let metrics = router.metrics();
            lines.push(format!(
                "metrics\tunderruns={}\toverruns={}\tformat_errors={}",
                metrics.underruns, metrics.overruns, metrics.format_errors
            ));
        }
        lines.join("\n")
    }

    fn restart_router(&mut self) -> Result<(), DaemonError> {
        self.router = None;
        let routes = self
            .graph
            .routes()
            .map(|route| {
                let source = self
                    .endpoint_devices
                    .get(route.source.as_str())
                    .ok_or_else(|| DaemonError::EndpointNotFound(route.source.to_string()))?;
                let sink = self
                    .endpoint_devices
                    .get(route.sink.as_str())
                    .ok_or_else(|| DaemonError::EndpointNotFound(route.sink.to_string()))?;
                Ok(AudioRoute {
                    source_device_id: source.object_id,
                    sink_device_id: sink.object_id,
                    source_channels: source.input_channels,
                    sink_channels: sink.output_channels,
                    source_sample_rate: source.sample_rate,
                    sink_sample_rate: sink.sample_rate,
                    matrix: route.matrix.coefficients().to_vec(),
                    gain: route.gain,
                })
            })
            .collect::<Result<Vec<_>, DaemonError>>()?;
        if !routes.is_empty() {
            self.router = Some(AudioRouter::start(&routes)?);
        }
        Ok(())
    }

    fn save_routes(&self) -> Result<(), DaemonError> {
        let parent = self
            .state_path
            .parent()
            .ok_or_else(|| DaemonError::State("state path has no parent directory".to_owned()))?;
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::create_dir_all(parent)?;
        let temp = self.state_path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temp)?;
        for route in self.routes.values() {
            let request = Request::Connect {
                id: route.id.clone(),
                source: route.source.clone(),
                sink: route.sink.clone(),
                gain_millidb: route.gain_millidb,
                matrix_millionths: route.matrix_millionths.clone(),
            };
            writeln!(file, "{}", request.encode())?;
        }
        file.sync_all()?;
        fs::rename(temp, &self.state_path)?;
        Ok(())
    }
}

fn endpoint_id(uid: &str, direction: &str) -> String {
    let mut encoded = String::from("audio:");
    for byte in uid.bytes() {
        if byte.is_ascii_alphanumeric() || b"._:-".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            use fmt::Write as _;
            let _ = write!(encoded, "_{byte:02x}");
        }
    }
    encoded.push(':');
    encoded.push_str(direction);
    encoded
}

fn validate_device_id(id: &str) -> Result<(), DaemonError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(DaemonError::InvalidDeviceId(id.to_owned()));
    }
    Ok(())
}

#[derive(Debug)]
enum DaemonError {
    CoreAudio(vcable_coreaudio::CoreAudioError),
    Graph(vcable_core::GraphError),
    Matrix(vcable_core::MatrixError),
    Io(std::io::Error),
    InvalidDeviceId(String),
    DeviceInUse(String),
    RouteExists(String),
    RouteNotFound(String),
    EndpointNotFound(String),
    MatrixRequired { inputs: usize, outputs: usize },
    State(String),
}

impl DaemonError {
    fn code(&self) -> &'static str {
        match self {
            Self::CoreAudio(_) => "core_audio",
            Self::Graph(_) => "invalid_graph",
            Self::Matrix(_) | Self::MatrixRequired { .. } => "invalid_matrix",
            Self::Io(_) | Self::State(_) => "state_io",
            Self::InvalidDeviceId(_) => "invalid_device_id",
            Self::DeviceInUse(_) => "device_in_use",
            Self::RouteExists(_) => "route_exists",
            Self::RouteNotFound(_) => "route_not_found",
            Self::EndpointNotFound(_) => "endpoint_not_found",
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoreAudio(error) => error.fmt(f),
            Self::Graph(error) => error.fmt(f),
            Self::Matrix(error) => error.fmt(f),
            Self::Io(error) => error.fmt(f),
            Self::InvalidDeviceId(id) => write!(f, "invalid device id: {id}"),
            Self::DeviceInUse(id) => write!(f, "device is referenced by a route: {id}"),
            Self::RouteExists(id) => write!(f, "route already exists: {id}"),
            Self::RouteNotFound(id) => write!(f, "route not found: {id}"),
            Self::EndpointNotFound(id) => write!(f, "endpoint not found: {id}"),
            Self::MatrixRequired { inputs, outputs } => write!(
                f,
                "an explicit {outputs}x{inputs} channel matrix is required"
            ),
            Self::State(message) => message.fmt(f),
        }
    }
}

impl Error for DaemonError {}

impl From<vcable_coreaudio::CoreAudioError> for DaemonError {
    fn from(value: vcable_coreaudio::CoreAudioError) -> Self {
        Self::CoreAudio(value)
    }
}

impl From<vcable_core::GraphError> for DaemonError {
    fn from(value: vcable_core::GraphError) -> Self {
        Self::Graph(value)
    }
}

impl From<vcable_core::MatrixError> for DaemonError {
    fn from(value: vcable_core::MatrixError) -> Self {
        Self::Matrix(value)
    }
}

impl From<std::io::Error> for DaemonError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
