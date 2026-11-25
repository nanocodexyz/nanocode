use std::{
    future::{Future, poll_fn},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::Arc,
    time::{Duration, Instant},
};

use nanocode_io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::{
    net::{Source, TcpStream},
    reactor::Direction,
};

const ATTEMPTS_DEFAULT: u32 = 2;
const ATTEMPTS_MAX: u32 = 5;
const CLASS_IN: u16 = 1;
const DATAGRAM_BYTES_MAX: usize = 65_535;
const FLAG_RECURSION_DESIRED: u16 = 0x0100;
const FLAG_RESPONSE: u16 = 0x8000;
const FLAG_TRUNCATED: u16 = 0x0200;
const HEADER_BYTES: usize = 12;
const HOSTS_PATH: &str = "/etc/hosts";
const LABEL_BYTES_MAX: usize = 63;
const NAME_BYTES_MAX: usize = 255;
const NAMESERVERS_MAX: usize = 3;
const NDOTS_DEFAULT: u32 = 1;
const NDOTS_MAX: u32 = 15;
const OPCODE_MASK: u16 = 0x7800;
const POINTER: u8 = 0xc0;
const PORT: u16 = 53;
const QUESTION_FIXED_BYTES: usize = 4;
const RCODE_MASK: u16 = 0x000f;
const RCODE_NAME_ERROR: u16 = 3;
const RCODE_NO_ERROR: u16 = 0;
const RECORD_FIXED_BYTES: usize = 10;
const RESOLV_CONF_PATH: &str = "/etc/resolv.conf";
const TIMEOUT_SECONDS_DEFAULT: u64 = 5;
const TIMEOUT_SECONDS_MAX: u64 = 30;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_CNAME: u16 = 5;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub attempts: u32,
    pub nameservers: Vec<SocketAddr>,
    pub ndots: u32,
    pub no_aaaa: bool,
    pub search: Vec<String>,
    pub tcp: bool,
    pub timeout: Duration,
}

pub struct System {
    config: Config,
    hosts: String,
    stamp: [Option<Stamp>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Stamp {
    changed: (i64, i64),
    device: u64,
    inode: u64,
    modified: (i64, i64),
    size: u64,
}

pub trait Wire {
    fn id(&mut self) -> io::Result<u16>;
    fn open(&mut self, server: SocketAddr, wait: Duration) -> io::Result<()>;
    fn receive(&mut self, buffer: &mut [u8]) -> impl Future<Output = io::Result<Option<usize>>>;
    fn send(&mut self, message: &[u8]) -> impl Future<Output = io::Result<()>>;
    fn stream(
        &mut self,
        server: SocketAddr,
        message: &[u8],
        wait: Duration,
    ) -> impl Future<Output = io::Result<Vec<u8>>>;
}

#[derive(Default)]
struct Sockets {
    deadline: Option<Instant>,
    socket: Option<Source<UdpSocket>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Answer {
    Failed,
    NoName,
    Records(Vec<IpAddr>),
}

enum Reply {
    Answer(Answer),
    Truncated,
}

struct Question {
    answer: Option<Answer>,
    id: u16,
    kind: u16,
    message: Vec<u8>,
}

struct Record {
    class: u16,
    data: std::ops::Range<usize>,
    kind: u16,
    owner: Vec<u8>,
}

impl Config {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut config = Self {
            attempts: ATTEMPTS_DEFAULT,
            nameservers: Vec::new(),
            ndots: NDOTS_DEFAULT,
            no_aaaa: false,
            search: Vec::new(),
            tcp: false,
            timeout: Duration::from_secs(TIMEOUT_SECONDS_DEFAULT),
        };
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with(['#', ';']) {
                continue;
            }
            let mut fields = line.split_ascii_whitespace();
            match fields.next() {
                Some("nameserver") => {
                    let address = fields.next().and_then(|field| field.parse::<IpAddr>().ok());
                    if let Some(address) = address
                        && config.nameservers.len() < NAMESERVERS_MAX
                    {
                        config.nameservers.push(SocketAddr::new(address, PORT));
                    }
                }
                Some("domain") => {
                    config.search = fields.next().map(str::to_owned).into_iter().collect();
                }
                Some("search") => config.search = fields.map(str::to_owned).collect(),
                Some("options") => {
                    for option in fields {
                        config.option(option);
                    }
                }
                _ => {}
            }
        }
        if config.nameservers.is_empty() {
            config
                .nameservers
                .push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT));
        }
        config
    }

    fn option(&mut self, option: &str) {
        let (name, value) = option.split_once(':').unwrap_or((option, ""));
        match name {
            "attempts" => {
                if let Ok(value) = value.parse::<u32>() {
                    self.attempts = value.clamp(1, ATTEMPTS_MAX);
                }
            }
            "ndots" => {
                if let Ok(value) = value.parse::<u32>() {
                    self.ndots = value.min(NDOTS_MAX);
                }
            }
            "no-aaaa" => self.no_aaaa = true,
            "timeout" => {
                if let Ok(value) = value.parse::<u64>() {
                    self.timeout = Duration::from_secs(value.clamp(1, TIMEOUT_SECONDS_MAX));
                }
            }
            "use-vc" => self.tcp = true,
            _ => {}
        }
    }

    #[must_use]
    pub fn system() -> Self {
        let mut config =
            Self::parse(&std::fs::read_to_string(RESOLV_CONF_PATH).unwrap_or_default());
        if let Ok(domains) = std::env::var("LOCALDOMAIN") {
            config.search = domains
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect();
        }
        if let Ok(options) = std::env::var("RES_OPTIONS") {
            for option in options.split_ascii_whitespace() {
                config.option(option);
            }
        }
        if config.search.is_empty()
            && let Ok(host) = nanocode_sys::unix::host_name()
            && let Some((_, domain)) = host.to_string_lossy().split_once('.')
            && !domain.is_empty()
        {
            config.search.push(domain.to_owned());
        }
        config
    }

    fn wait(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return self.timeout;
        }
        let servers = u32::try_from(self.nameservers.len())
            .expect("a configuration holds at most three name servers");
        (self.timeout * (1_u32 << attempt) / servers).max(Duration::from_secs(1))
    }
}

thread_local! {
    static LOADED: std::cell::Cell<Option<Arc<System>>> = const { std::cell::Cell::new(None) };
}

impl System {
    #[must_use]
    pub fn load() -> Arc<Self> {
        let stamp = [stamp(HOSTS_PATH), stamp(RESOLV_CONF_PATH)];
        if let Some(loaded) = LOADED.take() {
            LOADED.set(Some(loaded.clone()));
            if loaded.stamp == stamp {
                return loaded;
            }
        }
        let system = Arc::new(Self {
            hosts: std::fs::read_to_string(HOSTS_PATH).unwrap_or_default(),
            config: Config::system(),
            stamp,
        });
        LOADED.set(Some(system.clone()));
        system
    }
}

fn stamp(path: &str) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path).ok()?;
    Some(Stamp {
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        device: metadata.dev(),
        inode: metadata.ino(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        size: metadata.size(),
    })
}

impl Wire for Sockets {
    fn id(&mut self) -> io::Result<u16> {
        let mut bytes = [0; size_of::<u16>()];
        nanocode_sys::fill_random(&mut bytes)?;
        Ok(u16::from_ne_bytes(bytes))
    }

    fn open(&mut self, server: SocketAddr, wait: Duration) -> io::Result<()> {
        self.socket = None;
        let socket = UdpSocket::bind(SocketAddr::new(unspecified(server.ip()), 0))?;
        socket.connect(server)?;
        socket.set_nonblocking(true)?;
        self.socket = Some(Source::new(socket)?);
        self.deadline = Some(Instant::now() + wait);
        Ok(())
    }

    async fn receive(&mut self, buffer: &mut [u8]) -> io::Result<Option<usize>> {
        let (Some(socket), Some(deadline)) = (&self.socket, self.deadline) else {
            return Err(not_open());
        };
        let received =
            poll_fn(|context| socket.poll_io(context, Direction::Read, |inner| inner.recv(buffer)));
        crate::time::timeout_at(deadline, received)
            .await
            .map_or(Ok(None), |received| received.map(Some))
    }

    async fn send(&mut self, message: &[u8]) -> io::Result<()> {
        let Some(socket) = &self.socket else {
            return Err(not_open());
        };
        poll_fn(|context| socket.poll_io(context, Direction::Write, |inner| inner.send(message)))
            .await
            .map(drop)
    }

    async fn stream(
        &mut self,
        server: SocketAddr,
        message: &[u8],
        wait: Duration,
    ) -> io::Result<Vec<u8>> {
        let length = u16::try_from(message.len())
            .map_err(|_| invalid("the query does not fit a TCP message"))?;
        let mut framed = Vec::with_capacity(size_of::<u16>() + message.len());
        framed.extend_from_slice(&length.to_be_bytes());
        framed.extend_from_slice(message);
        let exchanged = async {
            let mut stream = TcpStream::connect_one(server).await?;
            stream.write_all(&framed).await?;
            let mut length = [0; size_of::<u16>()];
            stream.read_exact(&mut length).await?;
            let mut reply = vec![0; usize::from(u16::from_be_bytes(length))];
            stream.read_exact(&mut reply).await?;
            Ok(reply)
        };
        crate::time::timeout(wait, exchanged)
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the name server did not answer",
                ))
            })
    }
}

pub async fn resolve(system: &System, name: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    if let Ok(address) = name.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(address, port)]);
    }
    let listed = from_hosts(&system.hosts, name.strip_suffix('.').unwrap_or(name));
    let addresses = if listed.is_empty() {
        lookup(&system.config, &system.hosts, name, &mut Sockets::default()).await?
    } else {
        listed
    };
    let routed = |v6: bool| {
        addresses
            .iter()
            .find(|address| address.is_ipv6() == v6)
            .is_some_and(|address| {
                UdpSocket::bind(SocketAddr::new(unspecified(*address), 0))
                    .and_then(|socket| socket.connect(SocketAddr::new(*address, port)))
                    .is_ok()
            })
    };
    let (v6, v4) = (routed(true), routed(false));
    let mut ordered: Vec<SocketAddr> = addresses
        .into_iter()
        .map(|address| SocketAddr::new(address, port))
        .collect();
    ordered.sort_by_key(|address| match (address.is_ipv6(), v6, v4) {
        (true, true, _) | (false, false, true) => 0,
        (false, true, true) => 1,
        _ => 2,
    });
    Ok(ordered)
}

pub async fn lookup(
    config: &Config,
    hosts: &str,
    name: &str,
    wire: &mut impl Wire,
) -> io::Result<Vec<IpAddr>> {
    let (name, absolute) = name
        .strip_suffix('.')
        .map_or((name, false), |name| (name, true));
    if encode(name).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid host name",
        ));
    }
    let listed = from_hosts(hosts, name);
    if !listed.is_empty() {
        return Ok(listed);
    }
    let lowered = name.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") {
        return Ok(vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]);
    }
    let mut failure = None;
    let mut unreachable = Vec::new();
    for candidate in candidates(config, name, absolute) {
        match query(config, &candidate, wire, &mut unreachable).await {
            Ok(Some(addresses)) => return Ok(addresses),
            Ok(None) => {}
            Err(error)
                if config
                    .nameservers
                    .iter()
                    .all(|server| unreachable.contains(server)) =>
            {
                return Err(error);
            }
            Err(error) => failure = Some(error),
        }
    }
    Err(failure.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "failed to lookup address information: Name or service not known",
        )
    }))
}

#[must_use]
pub fn from_hosts(text: &str, name: &str) -> Vec<IpAddr> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let mut fields = line.split_ascii_whitespace();
        let Some(address) = fields.next().and_then(|field| field.parse::<IpAddr>().ok()) else {
            continue;
        };
        if fields.any(|alias| alias.eq_ignore_ascii_case(name)) && !found.contains(&address) {
            found.push(address);
        }
    }
    found
}

#[must_use]
pub fn candidates(config: &Config, name: &str, absolute: bool) -> Vec<Vec<u8>> {
    let searched = config
        .search
        .iter()
        .map(|domain| format!("{name}.{}", domain.trim_end_matches('.')));
    let dots = name.bytes().filter(|byte| *byte == b'.').count();
    let names: Vec<String> = if absolute {
        vec![name.to_owned()]
    } else if u32::try_from(dots).is_ok_and(|dots| dots < config.ndots) {
        searched.chain(std::iter::once(name.to_owned())).collect()
    } else {
        std::iter::once(name.to_owned()).chain(searched).collect()
    };
    names.iter().filter_map(|name| encode(name)).collect()
}

async fn query(
    config: &Config,
    name: &[u8],
    wire: &mut impl Wire,
    unreachable: &mut Vec<SocketAddr>,
) -> io::Result<Option<Vec<IpAddr>>> {
    let kinds: &[u16] = if config.no_aaaa {
        &[TYPE_A]
    } else {
        &[TYPE_AAAA, TYPE_A]
    };
    let mut questions = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let id = wire.id()?;
        questions.push(Question {
            answer: None,
            id,
            kind: *kind,
            message: message(id, name, *kind),
        });
    }
    let mut failure = None;
    for attempt in 0..config.attempts {
        for server in &config.nameservers {
            if unreachable.contains(server) {
                continue;
            }
            for question in &mut questions {
                question.answer = None;
            }
            let wait = config.wait(attempt);
            if let Err(error) = exchange(
                config.tcp,
                *server,
                wait,
                name,
                &mut questions,
                wire,
                unreachable,
            )
            .await
            {
                failure = Some(error);
                continue;
            }
            match answered(&questions) {
                Ok(found) => return Ok(found),
                Err(error) => failure = Some(error),
            }
        }
    }
    Err(failure.expect("a configuration holds at least one name server and one attempt"))
}

fn answered(questions: &[Question]) -> io::Result<Option<Vec<IpAddr>>> {
    let addresses: Vec<IpAddr> = questions
        .iter()
        .filter_map(|question| match &question.answer {
            Some(Answer::Records(addresses)) => Some(addresses.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    if !addresses.is_empty() {
        return Ok(Some(addresses));
    }
    if questions
        .iter()
        .any(|question| question.answer == Some(Answer::NoName))
        || questions
            .iter()
            .all(|question| matches!(question.answer, Some(Answer::Records(_))))
    {
        return Ok(None);
    }
    Err(
        if questions
            .iter()
            .any(|question| question.answer == Some(Answer::Failed))
        {
            io::Error::other("the name server failed to answer")
        } else {
            io::Error::new(io::ErrorKind::TimedOut, "the name server did not answer")
        },
    )
}

async fn exchange(
    tcp: bool,
    server: SocketAddr,
    wait: Duration,
    name: &[u8],
    questions: &mut [Question],
    wire: &mut impl Wire,
    unreachable: &mut Vec<SocketAddr>,
) -> io::Result<()> {
    if tcp {
        for question in questions {
            question.answer = Some(streamed(server, wait, name, question, wire).await?);
        }
        return Ok(());
    }
    if let Err(error) = wire.open(server, wait) {
        unreachable.push(server);
        return Err(error);
    }
    for question in questions.iter() {
        wire.send(&question.message).await?;
    }
    let mut buffer = vec![0; DATAGRAM_BYTES_MAX];
    while questions.iter().any(|question| question.answer.is_none()) {
        let Some(length) = wire.receive(&mut buffer).await? else {
            break;
        };
        let reply = &buffer[..length];
        let Some((question, decoded)) = questions
            .iter_mut()
            .filter(|question| {
                question.answer.is_none() && reply.get(..2) == Some(&question.id.to_be_bytes()[..])
            })
            .find_map(|question| {
                let kind = question.kind;
                decode(reply, name, kind).map(|decoded| (question, decoded))
            })
        else {
            continue;
        };
        question.answer = Some(match decoded {
            Reply::Answer(answer) => answer,
            Reply::Truncated => streamed(server, wait, name, question, wire).await?,
        });
    }
    Ok(())
}

async fn streamed(
    server: SocketAddr,
    wait: Duration,
    name: &[u8],
    question: &Question,
    wire: &mut impl Wire,
) -> io::Result<Answer> {
    let reply = wire.stream(server, &question.message, wait).await?;
    if reply.get(..2) != Some(&question.id.to_be_bytes()[..]) {
        return Err(invalid("the name server answered another query"));
    }
    match decode(&reply, name, question.kind) {
        Some(Reply::Answer(answer)) => Ok(answer),
        Some(Reply::Truncated) | None => Err(invalid("the name server sent a malformed answer")),
    }
}

fn decode(reply: &[u8], name: &[u8], kind: u16) -> Option<Reply> {
    let word = |at: usize| {
        reply
            .get(at..at + size_of::<u16>())
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    };
    let flags = word(2)?;
    if flags & FLAG_RESPONSE == 0 || flags & OPCODE_MASK != 0 || word(4)? != 1 {
        return None;
    }
    let mut owner = Vec::with_capacity(NAME_BYTES_MAX);
    let mut at = read_name(reply, HEADER_BYTES, &mut owner)?;
    if owner != name || word(at)? != kind || word(at + 2)? != CLASS_IN {
        return None;
    }
    at += QUESTION_FIXED_BYTES;
    if flags & FLAG_TRUNCATED != 0 {
        return Some(Reply::Truncated);
    }
    match flags & RCODE_MASK {
        RCODE_NO_ERROR => {}
        RCODE_NAME_ERROR => return Some(Reply::Answer(Answer::NoName)),
        _ => return Some(Reply::Answer(Answer::Failed)),
    }
    let count = word(6)?;
    let mut records = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let mut owner = Vec::with_capacity(NAME_BYTES_MAX);
        at = read_name(reply, at, &mut owner)?;
        let start = at + RECORD_FIXED_BYTES;
        let end = start + usize::from(word(at + 8)?);
        reply.get(start..end)?;
        records.push(Record {
            class: word(at + 2)?,
            data: start..end,
            kind: word(at)?,
            owner,
        });
        at = end;
    }
    let mut chain = vec![name.to_vec()];
    while let Some(record) = records.iter().find(|record| {
        record.kind == TYPE_CNAME && record.class == CLASS_IN && Some(&record.owner) == chain.last()
    }) {
        let mut target = Vec::with_capacity(NAME_BYTES_MAX);
        read_name(reply, record.data.start, &mut target)?;
        if chain.contains(&target) {
            return None;
        }
        chain.push(target);
    }
    Some(Reply::Answer(Answer::Records(addresses(
        reply, &records, &chain, kind,
    ))))
}

fn addresses(reply: &[u8], records: &[Record], chain: &[Vec<u8>], kind: u16) -> Vec<IpAddr> {
    records
        .iter()
        .filter(|record| {
            record.kind == kind && record.class == CLASS_IN && chain.contains(&record.owner)
        })
        .filter_map(|record| {
            let data = &reply[record.data.clone()];
            if kind == TYPE_A {
                <[u8; 4]>::try_from(data)
                    .ok()
                    .map(|bytes| IpAddr::V4(bytes.into()))
            } else {
                <[u8; 16]>::try_from(data)
                    .ok()
                    .map(|bytes| IpAddr::V6(bytes.into()))
            }
        })
        .collect()
}

fn read_name(message: &[u8], start: usize, name: &mut Vec<u8>) -> Option<usize> {
    name.clear();
    let mut at = start;
    let mut floor = start;
    let mut end = None;
    loop {
        let length = *message.get(at)?;
        match length & POINTER {
            0 if length == 0 => {
                name.push(0);
                return Some(end.unwrap_or(at + 1));
            }
            0 => {
                let label = message.get(at + 1..at + 1 + usize::from(length))?;
                name.push(length);
                name.extend(label.iter().map(u8::to_ascii_lowercase));
                if name.len() >= NAME_BYTES_MAX {
                    return None;
                }
                at += 1 + usize::from(length);
            }
            POINTER => {
                let target = usize::from(u16::from_be_bytes([
                    length & !POINTER,
                    *message.get(at + 1)?,
                ]));
                end.get_or_insert(at + 2);
                if target >= floor {
                    return None;
                }
                floor = target;
                at = target;
            }
            _ => return None,
        }
    }
}

fn encode(name: &str) -> Option<Vec<u8>> {
    let mut wire = Vec::with_capacity(name.len() + 2);
    for label in name.split('.') {
        let length = u8::try_from(label.len()).ok()?;
        if length == 0 || usize::from(length) > LABEL_BYTES_MAX {
            return None;
        }
        wire.push(length);
        wire.extend(label.bytes().map(|byte| byte.to_ascii_lowercase()));
    }
    wire.push(0);
    (wire.len() <= NAME_BYTES_MAX).then_some(wire)
}

fn message(id: u16, name: &[u8], kind: u16) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_BYTES + name.len() + QUESTION_FIXED_BYTES);
    for word in [id, FLAG_RECURSION_DESIRED, 1, 0, 0, 0] {
        message.extend_from_slice(&word.to_be_bytes());
    }
    message.extend_from_slice(name);
    message.extend_from_slice(&kind.to_be_bytes());
    message.extend_from_slice(&CLASS_IN.to_be_bytes());
    message
}

const fn unspecified(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

fn not_open() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "no name server is open")
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
