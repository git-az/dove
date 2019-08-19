/*
 * Copyright 2019, Ulf Lilleengen
 * License: Apache License 2.0 (see the file LICENSE or http://apache.org/licenses/LICENSE-2.0.html).
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::From;
use std::net::TcpListener;
use std::net::TcpStream;
use std::time::Duration;
use std::time::Instant;
use std::vec::Vec;

use crate::error::*;
use crate::framing::*;
use crate::transport::*;
use crate::types::*;

#[derive(Debug)]
pub enum Sasl {
    Server(Vec<SaslMechanism>),
    Client(SaslMechanism),
}

#[derive(Debug)]
pub struct ConnectionOptions<'a> {
    pub container_id: &'a str,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl<'a> ConnectionOptions<'a> {
    pub fn new(container_id: &'a str) -> ConnectionOptions {
        ConnectionOptions {
            container_id: container_id,
            username: None,
            password: None,
        }
    }
}

#[derive(Debug)]
pub struct ListenOptions<'a> {
    pub container_id: &'a str,
}

#[derive(Debug)]
pub struct Container {
    id: String,
}

#[derive(Debug)]
enum ConnectionState {
    Start,
    StartWait,
    HdrSent,
    Sasl,
    HdrExch,
    OpenRcvd,
    OpenSent,
    ClosePipe,
    Opened,
    CloseRcvd,
    CloseSent,
    End,
}

pub struct Link {}

pub struct Sender {}

pub struct Receiver {}

const AMQP_10_HEADER: ProtocolHeader = ProtocolHeader::AMQP(Version(1, 0, 0));
const SASL_10_HEADER: ProtocolHeader = ProtocolHeader::SASL(Version(1, 0, 0));

type Handle = usize;

#[derive(Debug)]
pub struct ConnectionDriver {
    connections: HashMap<Handle, Connection>,
    handles: Vec<Handle>,
    id_counter: usize,
    last_checked: Handle,
}

#[derive(Debug)]
pub struct Connection {
    pub container_id: String,
    pub hostname: String,
    pub channel_max: u16,
    pub idle_timeout: Duration,
    pub remote_idle_timeout: Duration,
    pub remote_container_id: String,
    pub remote_channel_max: u16,
    sasl: Option<Sasl>,
    sasl_username: Option<String>,
    sasl_password: Option<String>,
    state: ConnectionState,
    transport: Transport,
    opened: bool,
    closed: bool,
    close_condition: Option<ErrorCondition>,
    sessions: HashMap<ChannelId, Session>,
    remote_channel_map: HashMap<ChannelId, ChannelId>,
}

type ChannelId = u16;

#[derive(Debug)]
enum SessionState {
    Unmapped,
    BeginSent,
    BeginRcvd,
    Mapped,
    EndSent,
    EndRcvd,
    Discarding,
}

#[derive(Debug)]
pub struct Session {
    pub local_channel: ChannelId,
    remote_channel: Option<ChannelId>,
    state: SessionState,
    begun: bool,
    ended: bool,
}

pub fn connect(host: &str, port: u16, opts: ConnectionOptions) -> Result<Connection> {
    let stream = TcpStream::connect(format!("{}:{}", host, port))?;
    // TODO: SASL support
    let transport: Transport = Transport::new(stream, 1024)?;

    let mut connection = Connection::new(opts.container_id, host, transport);
    connection.sasl_username = opts.username;
    connection.sasl_password = opts.password;
    if connection.sasl_username.is_some() || connection.sasl_password.is_some() {
        connection.sasl = Some(Sasl::Client(SaslMechanism::Plain));
    }

    Ok(connection)
}

pub struct Listener {
    pub listener: TcpListener,
    pub container_id: String,
    pub sasl_mechanisms: Option<Vec<SaslMechanism>>,
}

pub fn listen(host: &str, port: u16, opts: ListenOptions) -> Result<Listener> {
    let listener = TcpListener::bind(format!("{}:{}", host, port))?;
    Ok(Listener {
        listener: listener,
        container_id: opts.container_id.to_string(),
        sasl_mechanisms: None,
    })
}

impl Listener {
    pub fn accept(&self) -> Result<Connection> {
        let (stream, addr) = self.listener.accept()?;
        let transport: Transport = Transport::new(stream, 1024)?;
        let mut connection = Connection::new(
            self.container_id.as_str(),
            addr.ip().to_string().as_str(),
            transport,
        );
        connection.state = ConnectionState::StartWait;
        Ok(connection)
    }
}

pub type EventBuffer = Vec<Event>;

#[derive(Debug)]
pub enum Event {
    ConnectionInit,
    RemoteOpen(Open),
    LocalOpen(Open),
    RemoteClose(Close),
    LocalClose(Option<ErrorCondition>),
    SessionInit(ChannelId),
    LocalBegin(ChannelId, Begin),
    RemoteBegin(ChannelId, Begin),
    /*
    LocalEnd(ChannelId, End),
    RemoteEnd(ChannelId, End),
    */
}

impl ConnectionDriver {
    pub fn new() -> ConnectionDriver {
        ConnectionDriver {
            connections: HashMap::new(),
            handles: Vec::new(),
            id_counter: 0,
            last_checked: 0,
        }
    }

    fn next_handle(self: &mut Self, current: Handle) -> Handle {
        (current + 1) % self.connections.len()
    }

    /// Register a new connection to be managed by this driver.
    /// # Examples
    /// use XXX::core::ConnectionDriver
    /// let connection = connect("localhost:5672")?;
    /// let driver = ConnectionDriver::new();
    /// let handle = driver.register(connection);
    pub fn register(self: &mut Self, connection: Connection) -> Handle {
        let handle = self.id_counter;
        self.connections.insert(handle, connection);
        self.handles.push(handle);
        self.id_counter += 1;
        handle
    }

    pub fn connection(self: &mut Self, handle: &Handle) -> Option<&mut Connection> {
        self.connections.get_mut(handle)
    }

    // Poll for events on one of the handles registered with this driver and push the events to the provided buffer.
    pub fn poll(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<Option<Handle>> {
        if self.handles.len() > 0 {
            let last: Handle = self.last_checked;
            loop {
                let next = self.next_handle(self.last_checked);

                let conn = self
                    .connections
                    .get_mut(&next)
                    .expect(format!("Handle {:?} missing!", next).as_str());
                let found = conn.poll(event_buffer);
                self.last_checked = next;
                match found {
                    Err(AmqpError::IoError(ref e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                    Ok(true) => return Ok(Some(next)),
                    _ => {}
                }
                if next == last {
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }
}

fn unwrap_frame(frame: Frame) -> Result<(ChannelId, Option<Performative>)> {
    match frame {
        Frame::AMQP(AmqpFrame {
            channel: channel,
            body: body,
        }) => {
            return Ok((channel as ChannelId, body));
        }
        _ => return Err(AmqpError::framing_error()),
    }
}

impl Connection {
    pub fn new(container_id: &str, hostname: &str, transport: Transport) -> Connection {
        Connection {
            container_id: container_id.to_string(),
            hostname: hostname.to_string(),
            idle_timeout: Duration::from_millis(5000),
            channel_max: std::u16::MAX,
            remote_container_id: String::new(),
            remote_channel_max: 0,
            remote_idle_timeout: Duration::from_millis(0),
            state: ConnectionState::Start,
            opened: false,
            closed: false,
            sessions: HashMap::new(),
            remote_channel_map: HashMap::new(),
            close_condition: None,
            transport: transport,
            sasl_username: None,
            sasl_password: None,
            sasl: None,
        }
    }

    pub fn open(self: &mut Self) {
        self.opened = true;
    }

    fn allocate_channel(self: &mut Self) -> Option<ChannelId> {
        for i in 0..self.channel_max {
            let chan = i as ChannelId;
            if !self.sessions.contains_key(&chan) {
                return Some(chan);
            }
        }
        None
    }

    pub fn create_session(self: &mut Self) -> &mut Session {
        self.session_internal(None)
    }

    pub fn get_session(self: &mut Self, channel_id: ChannelId) -> Option<&mut Session> {
        self.sessions.get_mut(&channel_id)
    }

    fn session_internal(self: &mut Self, channel_id: Option<ChannelId>) -> &mut Session {
        let chan = self.allocate_channel().unwrap();
        let s = Session {
            remote_channel: channel_id,
            local_channel: chan,
            begun: false,
            ended: false,
            state: SessionState::Unmapped,
        };
        self.sessions.insert(chan, s);
        channel_id.map(|c| self.remote_channel_map.insert(c, chan));
        self.sessions.get_mut(&chan).unwrap()
    }

    pub fn close(self: &mut Self, condition: Option<ErrorCondition>) {
        self.closed = true;
        self.close_condition = condition;
    }

    fn poll(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<bool> {
        let before = event_buffer.len();
        self.do_work(event_buffer)?;

        Ok(before != event_buffer.len())
    }

    fn check_header(self: &mut Self, header: ProtocolHeader, respond: bool) -> Result<()> {
        if self.sasl.is_none() {
            match header {
                AMQP_10_HEADER => {
                    if respond {
                        self.transport.write_protocol_header(&AMQP_10_HEADER)?;
                        self.transport.flush()?;
                    }
                    self.state = ConnectionState::HdrExch;
                }
                _ => {
                    self.transport.write_protocol_header(&AMQP_10_HEADER)?;
                    self.transport.flush()?;
                    self.transport.close()?;
                    self.state = ConnectionState::End;
                }
            }
            Ok(())
        } else {
            match header {
                SASL_10_HEADER => {
                    if respond {
                        self.transport.write_protocol_header(&SASL_10_HEADER)?;
                        self.transport.flush()?;
                    }
                    self.state = ConnectionState::Sasl;
                }
                _ => {
                    self.transport.write_protocol_header(&SASL_10_HEADER)?;
                    self.transport.flush()?;
                    self.transport.close()?;
                    self.state = ConnectionState::End;
                }
            }
            Ok(())
        }
    }

    fn do_work(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<()> {
        match self.state {
            ConnectionState::StartWait => {
                let header = self.transport.read_protocol_header()?;
                if header.is_some() {
                    let header = header.unwrap();
                    self.check_header(header, true)?;
                    event_buffer.push(Event::ConnectionInit);
                }
            }
            ConnectionState::Start => {
                if self.sasl.is_none() {
                    self.transport.write_protocol_header(&AMQP_10_HEADER)?;
                } else {
                    self.transport.write_protocol_header(&SASL_10_HEADER)?;
                }
                self.transport.flush()?;
                self.state = ConnectionState::HdrSent;
            }
            ConnectionState::HdrSent => {
                let header = self.transport.read_protocol_header()?;
                if header.is_some() {
                    let header = header.unwrap();
                    self.check_header(header, false)?;
                    event_buffer.push(Event::ConnectionInit);
                }
            }
            ConnectionState::Sasl => {
                println!("Let the SASL exchange begin!");
                match &self.sasl {
                    Some(Sasl::Client(mechanism)) => {
                        let frame = self.transport.read_frame()?;
                        match frame {
                            Frame::SASL(SaslFrame::SaslMechanisms(mechs)) => {
                                println!("Got mechs {:?}, we want: {:?}!", mechs, mechanism);
                                let mut found = false;
                                for supported_mech in mechs.iter() {
                                    if mechanism == supported_mech {
                                        println!("Found supported mechanism, proceed!");
                                        found = true;
                                    }
                                }
                                if !found {
                                    println!("Unable to find supported mechanism");
                                    self.transport.close()?;
                                    self.state = ConnectionState::End;
                                } else {
                                    let mut initial_response = None;
                                    if *mechanism == SaslMechanism::Plain {
                                        let mut data = Vec::new();
                                        data.extend_from_slice(
                                            self.sasl_username.clone().unwrap().as_bytes(),
                                        );
                                        data.push(0);
                                        data.extend_from_slice(
                                            self.sasl_username.clone().unwrap().as_bytes(),
                                        );
                                        data.push(0);
                                        data.extend_from_slice(
                                            self.sasl_password.clone().unwrap().as_bytes(),
                                        );
                                        initial_response = Some(data);
                                    }
                                    let init = Frame::SASL(SaslFrame::SaslInit(SaslInit {
                                        mechanism: mechanism.to_string(),
                                        initial_response: initial_response,
                                        hostname: None,
                                    }));
                                    self.transport.write_frame(&init)?;
                                    self.transport.flush()?;
                                }
                            }
                            Frame::SASL(SaslFrame::SaslOutcome(outcome)) => {
                                println!("Got outcome: {:?}", outcome);
                                if outcome.code == 0 {
                                    self.state = ConnectionState::HdrExch;
                                } else {
                                    self.transport.close()?;
                                    self.state = ConnectionState::End;
                                }
                            }
                            _ => println!("Got frame {:?}", frame),
                        }
                    }
                    Some(Sasl::Server(allowed_mechs)) => {}
                    _ => {}
                }
            }
            ConnectionState::HdrExch => {
                if self.opened {
                    self.local_open(event_buffer)?;
                    self.state = ConnectionState::OpenSent;
                } else {
                    let frame = self.transport.read_frame()?;
                    let (_, body) = unwrap_frame(frame)?;
                    if let Some(body) = body {
                        match body {
                            Performative::Open(open) => {
                                self.update_connection_info(&open);
                                event_buffer.push(Event::RemoteOpen(open));
                                self.state = ConnectionState::OpenRcvd;
                            }
                            _ => return Err(AmqpError::framing_error()),
                        }
                    }
                }
            }
            ConnectionState::OpenRcvd => {
                if self.opened {
                    self.local_open(event_buffer)?;
                    self.state = ConnectionState::Opened;
                }
            }
            ConnectionState::OpenSent => {
                if self.closed {
                    self.local_close(event_buffer)?;
                    self.state = ConnectionState::ClosePipe;
                } else {
                    let frame = self.transport.read_frame()?;
                    let (_, body) = unwrap_frame(frame)?;
                    if let Some(body) = body {
                        match body {
                            Performative::Open(open) => {
                                self.update_connection_info(&open);
                                event_buffer.push(Event::RemoteOpen(open));
                                self.state = ConnectionState::Opened;
                            }
                            Performative::Close(close) => {
                                event_buffer.push(Event::RemoteClose(close));
                                self.state = ConnectionState::ClosePipe;
                            }
                            _ => return Err(AmqpError::framing_error()),
                        }
                    }
                }
            }
            ConnectionState::Opened => {
                if self.closed {
                    self.local_close(event_buffer)?;
                    self.state = ConnectionState::CloseSent;
                } else {
                    self.dispatch_work(event_buffer)?;
                    self.keepalive(event_buffer)?;
                    let frame = self.transport.read_frame()?;
                    self.dispatch_frame(frame, event_buffer)?;
                }
            }
            ConnectionState::ClosePipe => {
                let frame = self.transport.read_frame()?;
                let (_, body) = unwrap_frame(frame)?;
                if let Some(body) = body {
                    match body {
                        Performative::Open(open) => {
                            event_buffer.push(Event::RemoteOpen(open));
                            self.state = ConnectionState::CloseSent;
                        }
                        _ => return Err(AmqpError::framing_error()),
                    }
                }
            }
            ConnectionState::CloseRcvd => {
                if self.closed {
                    self.local_close(event_buffer)?;
                    self.state = ConnectionState::End;
                }
            }
            ConnectionState::CloseSent => {
                let frame = self.transport.read_frame()?;
                let (_, body) = unwrap_frame(frame)?;

                if let Some(body) = body {
                    match body {
                        Performative::Close(close) => {
                            event_buffer.push(Event::RemoteClose(close));
                            self.state = ConnectionState::End;
                        }
                        _ => return Err(AmqpError::framing_error()),
                    }
                }
            }
            ConnectionState::End => {}
        }
        Ok(())
    }

    fn update_connection_info(self: &mut Self, open: &Open) {
        self.remote_container_id = open.container_id.clone();
        self.remote_idle_timeout = Duration::from_millis(open.idle_timeout.unwrap_or(0) as u64);
        self.remote_channel_max = open.channel_max.unwrap_or(65535);
    }

    // Dispatch to work performed by sub-endpoints
    fn dispatch_work(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<()> {
        for (channel_id, session) in self.sessions.iter_mut() {
            match session.state {
                SessionState::Unmapped => {
                    if session.begun {
                        let frame = session.local_begin(&mut self.transport, event_buffer)?;
                        session.state = SessionState::BeginSent;
                    }
                }
                SessionState::BeginRcvd => {
                    if session.begun {
                        let frame = session.local_begin(&mut self.transport, event_buffer)?;
                        session.state = SessionState::Mapped;
                    }
                }
                SessionState::BeginSent | SessionState::Mapped => {
                    session.dispatch_work(&mut self.transport, event_buffer)?;
                }
                _ => return Err(AmqpError::not_implemented()),
            }
        }
        Ok(())
    }

    fn keepalive(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<()> {
        // Sent out keepalives...
        let now = Instant::now();
        if self.remote_idle_timeout.as_millis() > 0 {
            if now - self.transport.last_sent() >= self.remote_idle_timeout {
                let frame = Frame::AMQP(AmqpFrame {
                    channel: 0,
                    body: None,
                });
                self.transport.write_frame(&frame)?;
                self.transport.flush()?;
            }
        }

        if self.idle_timeout.as_millis() > 0 {
            // Ensure our peer honors our keepalive
            if now - self.transport.last_received() > self.idle_timeout * 2 {
                self.close_condition = Some(ErrorCondition {
                    condition: condition::RESOURCE_LIMIT_EXCEEDED.to_string(),
                    description: "local-idle-timeout expired".to_string(),
                });
                self.local_close(event_buffer)?;
            }
        }
        Ok(())
    }

    // Dispatch frame to relevant endpoint
    fn dispatch_frame(self: &mut Self, frame: Frame, event_buffer: &mut EventBuffer) -> Result<()> {
        let (channel_id, body) = unwrap_frame(frame)?;

        if body.is_none() {
            return Ok(());
        }

        let body = body.unwrap();
        let mut consumed = self.process_frame(channel_id, &body, event_buffer)?;
        let local_channel_opt = self.remote_channel_map.get_mut(&channel_id);
        if let Some(local_channel) = local_channel_opt {
            let session = self.sessions.get_mut(&local_channel).unwrap();
            consumed |= session.process_frame(body, event_buffer)?;
        }
        if !consumed {
            Err(AmqpError::framing_error())
        } else {
            Ok(())
        }
    }

    // Handle frames for a connection
    fn process_frame(
        self: &mut Self,
        channel_id: ChannelId,
        body: &Performative,
        event_buffer: &mut EventBuffer,
    ) -> Result<bool> {
        Ok(match body {
            // TODO: Handle sessions, links etc...
            Performative::Begin(begin) => {
                let session = self.session_internal(Some(channel_id));
                session.state = SessionState::BeginRcvd;
                session.remote_channel = Some(channel_id);
                let local_channel = session.local_channel;
                //self.remote_channel_map .insert(channel_id, session.local_channel);
                event_buffer.push(Event::RemoteBegin(session.local_channel, begin.clone()));
                true
            }
            Performative::Close(close) => {
                if channel_id == 0 {
                    event_buffer.push(Event::RemoteClose(close.clone()));
                    self.state = ConnectionState::CloseRcvd;
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
    }

    fn local_open(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<()> {
        let mut args = Open::new(self.container_id.as_str());
        args.hostname = Some(self.hostname.clone());
        args.channel_max = Some(self.channel_max);
        args.idle_timeout = Some(self.idle_timeout.as_millis() as u32);

        let frame = Frame::AMQP(AmqpFrame {
            channel: 0,
            body: Some(Performative::Open(args)),
        });

        self.transport.write_frame(&frame)?;
        self.transport.flush()?;

        if let Frame::AMQP(AmqpFrame {
            channel: _,
            body: body,
        }) = frame
        {
            if let Some(Performative::Open(data)) = body {
                event_buffer.push(Event::LocalOpen(data));
            }
        }
        Ok(())
    }

    fn local_close(self: &mut Self, event_buffer: &mut EventBuffer) -> Result<()> {
        let frame = Frame::AMQP(AmqpFrame {
            channel: 0,
            body: Some(Performative::Close(Close {
                error: self.close_condition.clone(),
            })),
        });

        self.transport.write_frame(&frame)?;
        self.transport.flush()?;

        let condition = self.close_condition.clone();
        event_buffer.push(Event::LocalClose(condition));
        Ok(())
    }
}

impl Session {
    pub fn begin(self: &mut Self) {
        self.begun = true;
    }

    fn process_frame(
        self: &mut Self,
        performative: Performative,
        event_buffer: &mut EventBuffer,
    ) -> Result<bool> {
        Ok(match self.state {
            SessionState::Unmapped => match performative {
                Performative::Begin(begin) => {
                    self.remote_channel = begin.remote_channel;
                    event_buffer.push(Event::RemoteBegin(self.local_channel, begin));
                    self.state = SessionState::BeginRcvd;
                    true
                }
                _ => false,
            },
            SessionState::BeginSent => match performative {
                Performative::Begin(begin) => {
                    event_buffer.push(Event::RemoteBegin(self.local_channel, begin));
                    self.state = SessionState::Mapped;
                    true
                }
                _ => false,
            },
            _ => false,
        })
    }

    fn local_begin(
        self: &mut Self,
        transport: &mut Transport,
        event_buffer: &mut EventBuffer,
    ) -> Result<()> {
        let frame = Frame::AMQP(AmqpFrame {
            channel: self.local_channel as u16,
            body: Some(Performative::Begin(Begin {
                remote_channel: self.remote_channel,
                next_outgoing_id: 0,
                incoming_window: 10,
                outgoing_window: 10,
                handle_max: None,
                offered_capabilities: None,
                desired_capabilities: None,
                properties: None,
            })),
        });

        transport.write_frame(&frame)?;
        transport.flush()?;

        if let Frame::AMQP(AmqpFrame { channel: _, body }) = frame {
            if let Some(Performative::Begin(data)) = body {
                event_buffer.push(Event::LocalBegin(self.local_channel, data));
            }
        }
        Ok(())
    }

    fn dispatch_work(
        self: &mut Self,
        transport: &mut Transport,
        event_buffer: &mut EventBuffer,
    ) -> Result<()> {
        Ok(())
    }
}
