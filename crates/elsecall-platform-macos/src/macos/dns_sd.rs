//! What the local network advertises, through the system's DNS Service Discovery
//! (Bonjour) client library: the adapter of `nearby_macs::LocalNetwork`.
//!
//! Every operation shares one connection to the system's daemon, so a network
//! with many devices costs one descriptor. macOS Local Network privacy applies: a
//! process it denies gets `local_network_denied`.
use elsecall_add_mac::{
    lan_address::is_lan,
    nearby_macs::{Advertisement, LocalNetwork, Offer},
};
use elsecall_contract::failure::{Failure, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString, c_char, c_void},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::{Duration, Instant},
};

type ServiceRef = *mut c_void;
type ErrorType = i32;

const NO_ERROR: ErrorType = 0;
/// `kDNSServiceErr_PolicyDenied`: Local Network privacy denied this process.
const POLICY_DENIED: ErrorType = -65570;
const FLAG_MORE_COMING: u32 = 0x1;
const FLAG_ADD: u32 = 0x2;
const FLAG_SHARE_CONNECTION: u32 = 0x4000;
const PROTOCOL_IPV4: u32 = 0x01;
const PROTOCOL_IPV6: u32 = 0x02;
/// `kDNSServiceInterfaceIndexAny`: every interface except peer-to-peer ones.
const INTERFACE_ANY: u32 = 0;
const TYPE_TXT: u16 = 16;
const CLASS_IN: u16 = 1;
const DOMAIN: &CStr = c"local.";
/// `kDNSServiceMaxDomainName`: the buffer `DNSServiceConstructFullName` fills.
const MAX_DOMAIN_NAME: usize = 1009;
/// How long resolving names and reading models may take after browsing. A device
/// that does not answer in time is left out rather than holding up the others.
const RESOLVE_WINDOW: Duration = Duration::from_secs(1);
/// How long looking up the resolved hosts' addresses may take.
const ADDRESS_WINDOW: Duration = Duration::from_secs(1);
const SSH: &CStr = c"_ssh._tcp";
const COMPANION_LINK: &CStr = c"_companion-link._tcp";
const DEVICE_INFO: &CStr = c"_device-info._tcp";

type BrowseReply = unsafe extern "C" fn(
    ServiceRef,
    u32,
    u32,
    ErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveReply = unsafe extern "C" fn(
    ServiceRef,
    u32,
    u32,
    ErrorType,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);
type AddressReply = unsafe extern "C" fn(
    ServiceRef,
    u32,
    u32,
    ErrorType,
    *const c_char,
    *const libc::sockaddr,
    u32,
    *mut c_void,
);
type QueryReply = unsafe extern "C" fn(
    ServiceRef,
    u32,
    u32,
    ErrorType,
    *const c_char,
    u16,
    u16,
    u16,
    *const c_void,
    u32,
    *mut c_void,
);

// The DNS-SD client library is part of libSystem on macOS.
unsafe extern "C" {
    fn DNSServiceCreateConnection(service: *mut ServiceRef) -> ErrorType;
    fn DNSServiceBrowse(
        service: *mut ServiceRef,
        flags: u32,
        interface: u32,
        kind: *const c_char,
        domain: *const c_char,
        reply: BrowseReply,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceResolve(
        service: *mut ServiceRef,
        flags: u32,
        interface: u32,
        name: *const c_char,
        kind: *const c_char,
        domain: *const c_char,
        reply: ResolveReply,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceGetAddrInfo(
        service: *mut ServiceRef,
        flags: u32,
        interface: u32,
        protocol: u32,
        host: *const c_char,
        reply: AddressReply,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceQueryRecord(
        service: *mut ServiceRef,
        flags: u32,
        interface: u32,
        name: *const c_char,
        kind: u16,
        class: u16,
        reply: QueryReply,
        context: *mut c_void,
    ) -> ErrorType;
    fn DNSServiceConstructFullName(
        full: *mut c_char,
        name: *const c_char,
        kind: *const c_char,
        domain: *const c_char,
    ) -> ErrorType;
    fn DNSServiceRefSockFD(service: ServiceRef) -> i32;
    fn DNSServiceProcessResult(service: ServiceRef) -> ErrorType;
    fn DNSServiceRefDeallocate(service: ServiceRef);
}

/// Which advertised service an operation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Ssh,
    CompanionLink,
}

impl Kind {
    fn dns_sd_type(self) -> &'static CStr {
        match self {
            Kind::Ssh => SSH,
            Kind::CompanionLink => COMPANION_LINK,
        }
    }
}

pub struct Bonjour;

impl LocalNetwork for Bonjour {
    fn advertisements(&self, browse: Duration) -> Result<Vec<Advertisement>> {
        let mut found: BTreeMap<(Kind, String), u32> = BTreeMap::new();
        {
            let mut session = Session::open()?;
            for kind in [Kind::Ssh, Kind::CompanionLink] {
                let context = Browsed {
                    kind,
                    found: Vec::new(),
                    denied: false,
                };
                session
                    .start(context, |reference, context| unsafe {
                        DNSServiceBrowse(
                            reference,
                            FLAG_SHARE_CONNECTION,
                            0,
                            kind.dns_sd_type().as_ptr(),
                            DOMAIN.as_ptr(),
                            browsed,
                            context,
                        )
                    })
                    .map_err(browse_failure)?;
            }
            session.run_until(Instant::now() + browse, |_| false)?;
            if session.contexts::<Browsed>().any(|c| c.denied) {
                return Err(local_network_denied());
            }
            for context in session.contexts::<Browsed>() {
                for (name, interface) in &context.found {
                    found
                        .entry((context.kind, name.clone()))
                        .or_insert(*interface);
                }
            }
        }

        let (resolved, models) = {
            let mut session = Session::open()?;
            start_resolving(&mut session, &found);
            start_reading_models(&mut session, &found);
            session.run_until(Instant::now() + RESOLVE_WINDOW, |session| {
                session.contexts::<Resolved>().all(|r| r.target.is_some())
                    && session.contexts::<Described>().all(|d| d.model.is_some())
            })?;
            (resolutions(&session), models(&session))
        };
        let hosts: BTreeSet<&str> = resolved.values().map(|t| t.host.as_str()).collect();
        let addresses = addresses(&hosts, Instant::now() + ADDRESS_WINDOW)?;

        Ok(found
            .keys()
            .filter_map(|(kind, name)| {
                let target = resolved.get(&(*kind, name.clone()))?;
                Some(Advertisement {
                    offer: match kind {
                        Kind::Ssh => Offer::RemoteLogin { port: target.port },
                        Kind::CompanionLink => Offer::Continuity,
                    },
                    name: name.clone(),
                    addresses: addresses.get(&target.host).cloned().unwrap_or_default(),
                    host: target.host.clone(),
                    model: models.get(name).cloned(),
                })
            })
            .collect())
    }
}

fn local_network_denied() -> Failure {
    Failure::new(
        "local_network_denied",
        "macOS Local Network privacy denied this process the local network.",
        "Run the command from Terminal or over SSH, or allow the app that runs it in System Settings > Privacy & Security > Local Network.",
    )
}

fn browse_failure(error: ErrorType) -> Failure {
    if error == POLICY_DENIED {
        return local_network_denied();
    }
    bonjour_unavailable(error)
}

fn bonjour_unavailable(error: ErrorType) -> Failure {
    Failure::new(
        "bonjour_unavailable",
        format!("The system's Bonjour service did not answer (DNS-SD error {error})."),
        "Retry in a moment; add the Mac by host name or address if it persists.",
    )
}

/// Starts resolving every browsed service to its host and port. One the daemon
/// refuses is left out, like one that does not answer.
fn start_resolving(session: &mut Session, found: &BTreeMap<(Kind, String), u32>) {
    for ((kind, name), interface) in found {
        let Ok(c_name) = CString::new(name.as_str()) else {
            continue;
        };
        let context = Resolved {
            key: (*kind, name.clone()),
            target: None,
        };
        let _ = session.start(context, |reference, context| unsafe {
            DNSServiceResolve(
                reference,
                FLAG_SHARE_CONNECTION,
                *interface,
                c_name.as_ptr(),
                kind.dns_sd_type().as_ptr(),
                DOMAIN.as_ptr(),
                resolved,
                context,
            )
        });
    }
}

/// Where a browsed service runs.
struct Target {
    host: String,
    port: u16,
}

fn resolutions(session: &Session) -> BTreeMap<(Kind, String), Target> {
    session
        .contexts::<Resolved>()
        .filter_map(|r| {
            let (host, port) = r.target.clone()?;
            Some((r.key.clone(), Target { host, port }))
        })
        .collect()
}

/// Every host's local network addresses, IPv4 first: the same kind members
/// report, so the two compare.
///
/// They are looked up on every interface, not the one the service was browsed
/// on: a host sees its own services on loopback too, and there it has only
/// addresses no other machine can use, so the Mac running this would have none.
fn addresses(hosts: &BTreeSet<&str>, deadline: Instant) -> Result<BTreeMap<String, Vec<IpAddr>>> {
    let mut session = Session::open()?;
    for host in hosts {
        let Ok(c_host) = CString::new(*host) else {
            continue;
        };
        let context = Addressed {
            host: (*host).to_owned(),
            addresses: Vec::new(),
            settled: false,
        };
        let _ = session.start(context, |reference, context| unsafe {
            DNSServiceGetAddrInfo(
                reference,
                FLAG_SHARE_CONNECTION,
                INTERFACE_ANY,
                PROTOCOL_IPV4 | PROTOCOL_IPV6,
                c_host.as_ptr(),
                addressed,
                context,
            )
        });
    }
    session.run_until(deadline, |session| {
        session.contexts::<Addressed>().all(|a| a.settled)
    })?;
    Ok(session
        .contexts::<Addressed>()
        .map(|a| {
            let mut addresses = a.addresses.clone();
            addresses.sort_by_key(|address| (address.is_ipv6(), *address));
            addresses.dedup();
            (a.host.clone(), addresses)
        })
        .collect())
}

/// Starts reading the model each advertised name reports in its
/// `_device-info._tcp` record.
fn start_reading_models(session: &mut Session, found: &BTreeMap<(Kind, String), u32>) {
    let mut names: BTreeMap<&str, u32> = BTreeMap::new();
    for ((_, name), interface) in found {
        names.entry(name.as_str()).or_insert(*interface);
    }
    for (name, interface) in names {
        let Some(full) = full_name(name, DEVICE_INFO) else {
            continue;
        };
        let context = Described {
            name: name.to_owned(),
            model: None,
        };
        let _ = session.start(context, |reference, context| unsafe {
            DNSServiceQueryRecord(
                reference,
                FLAG_SHARE_CONNECTION,
                interface,
                full.as_ptr(),
                TYPE_TXT,
                CLASS_IN,
                described,
                context,
            )
        });
    }
}

fn models(session: &Session) -> BTreeMap<String, String> {
    session
        .contexts::<Described>()
        .filter_map(|d| Some((d.name.clone(), d.model.clone()?)))
        .collect()
}

fn full_name(name: &str, kind: &CStr) -> Option<CString> {
    let name = CString::new(name).ok()?;
    let mut buffer = vec![0 as c_char; MAX_DOMAIN_NAME];
    // SAFETY: the buffer has the size dns_sd.h requires for a full name.
    let error = unsafe {
        DNSServiceConstructFullName(
            buffer.as_mut_ptr(),
            name.as_ptr(),
            kind.as_ptr(),
            DOMAIN.as_ptr(),
        )
    };
    if error != NO_ERROR {
        return None;
    }
    // SAFETY: on success the buffer holds a NUL-terminated name.
    Some(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_owned())
}

/// The value of `key` in a DNS TXT record's length-prefixed `key=value` strings.
/// Keys compare without case, as RFC 6763 specifies.
pub fn txt_value(record: &[u8], key: &str) -> Option<String> {
    let mut rest = record;
    while let Some((&length, tail)) = rest.split_first() {
        let length = usize::from(length);
        let entry = tail.get(..length)?;
        rest = &tail[length..];
        let Some(split) = entry.iter().position(|b| *b == b'=') else {
            continue;
        };
        if entry[..split].eq_ignore_ascii_case(key.as_bytes()) {
            return String::from_utf8(entry[split + 1..].to_vec()).ok();
        }
    }
    None
}

struct Browsed {
    kind: Kind,
    found: Vec<(String, u32)>,
    denied: bool,
}
struct Resolved {
    key: (Kind, String),
    target: Option<(String, u16)>,
}
struct Addressed {
    host: String,
    /// Only `is_lan` addresses.
    addresses: Vec<IpAddr>,
    /// The daemon delivered what it had at once (no "more coming").
    settled: bool,
}
struct Described {
    name: String,
    model: Option<String>,
}

fn text(pointer: *const c_char) -> Option<String> {
    // SAFETY: the daemon passes NUL-terminated strings valid during the callback.
    (!pointer.is_null()).then(|| {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

// SAFETY for every callback: `context` is the pointer `Session::start` registered
// for this operation, valid until the session deallocates the operation, and only
// the callback dereferences it while `DNSServiceProcessResult` runs.
unsafe extern "C" fn browsed(
    _: ServiceRef,
    flags: u32,
    interface: u32,
    error: ErrorType,
    name: *const c_char,
    _: *const c_char,
    _: *const c_char,
    context: *mut c_void,
) {
    let context = unsafe { &mut *(context as *mut Browsed) };
    context.denied |= error == POLICY_DENIED;
    if error == NO_ERROR
        && flags & FLAG_ADD != 0
        && let Some(name) = text(name)
    {
        context.found.push((name, interface));
    }
}

unsafe extern "C" fn resolved(
    _: ServiceRef,
    _: u32,
    _: u32,
    error: ErrorType,
    _: *const c_char,
    host: *const c_char,
    port: u16,
    _: u16,
    _: *const u8,
    context: *mut c_void,
) {
    let context = unsafe { &mut *(context as *mut Resolved) };
    if error == NO_ERROR
        && let Some(host) = text(host)
    {
        context.target = Some((host, u16::from_be(port)));
    }
}

unsafe extern "C" fn addressed(
    _: ServiceRef,
    flags: u32,
    _: u32,
    error: ErrorType,
    _: *const c_char,
    address: *const libc::sockaddr,
    _: u32,
    context: *mut c_void,
) {
    let context = unsafe { &mut *(context as *mut Addressed) };
    if error == NO_ERROR
        && flags & FLAG_ADD != 0
        && !address.is_null()
        && let Some(address) = unsafe { ip_address(address) }
        && is_lan(address)
    {
        context.addresses.push(address);
    }
    if flags & FLAG_MORE_COMING == 0 && !context.addresses.is_empty() {
        context.settled = true;
    }
}

unsafe extern "C" fn described(
    _: ServiceRef,
    _: u32,
    _: u32,
    error: ErrorType,
    _: *const c_char,
    _: u16,
    _: u16,
    length: u16,
    data: *const c_void,
    _: u32,
    context: *mut c_void,
) {
    let context = unsafe { &mut *(context as *mut Described) };
    if error == NO_ERROR && !data.is_null() {
        // SAFETY: the daemon passes `length` bytes of record data.
        let record = unsafe { std::slice::from_raw_parts(data as *const u8, usize::from(length)) };
        if let Some(model) = txt_value(record, "model") {
            context.model = Some(model);
        }
    }
}

/// SAFETY: `address` points to a socket address of the family it names.
unsafe fn ip_address(address: *const libc::sockaddr) -> Option<IpAddr> {
    match i32::from(unsafe { (*address).sa_family }) {
        libc::AF_INET => {
            let v4 = unsafe { &*(address as *const libc::sockaddr_in) };
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            let v6 = unsafe { &*(address as *const libc::sockaddr_in6) };
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

/// A context an operation's callback fills, owned through a raw pointer so the
/// pointer the daemon holds stays the only way it is reached while in flight.
struct Context {
    pointer: *mut c_void,
    kind: std::any::TypeId,
    drop: unsafe fn(*mut c_void),
}

/// SAFETY: `pointer` came from `Box::<T>::into_raw` and is dropped once.
unsafe fn drop_boxed<T>(pointer: *mut c_void) {
    drop(unsafe { Box::from_raw(pointer as *mut T) });
}

/// One connection to the DNS-SD daemon and the operations sharing it. Dropping
/// it deallocates every operation, then the connection, then the contexts.
struct Session {
    connection: ServiceRef,
    operations: Vec<ServiceRef>,
    contexts: Vec<Context>,
}

impl Session {
    fn open() -> Result<Self> {
        let mut connection: ServiceRef = std::ptr::null_mut();
        // SAFETY: the daemon fills `connection` on success.
        let error = unsafe { DNSServiceCreateConnection(&mut connection) };
        if error != NO_ERROR || connection.is_null() {
            return Err(browse_failure(error));
        }
        Ok(Self {
            connection,
            operations: Vec::new(),
            contexts: Vec::new(),
        })
    }

    /// Starts an operation on the shared connection whose callback receives
    /// `context`; the daemon's error when it refuses.
    fn start<T: 'static>(
        &mut self,
        context: T,
        begin: impl FnOnce(*mut ServiceRef, *mut c_void) -> ErrorType,
    ) -> std::result::Result<(), ErrorType> {
        let pointer = Box::into_raw(Box::new(context)) as *mut c_void;
        // With kDNSServiceFlagsShareConnection, the reference starts as the
        // shared connection and becomes the new operation's own.
        let mut reference = self.connection;
        let error = begin(&mut reference, pointer);
        if error != NO_ERROR {
            // SAFETY: the daemon did not keep the pointer.
            unsafe { drop_boxed::<T>(pointer) };
            return Err(error);
        }
        self.operations.push(reference);
        self.contexts.push(Context {
            pointer,
            kind: std::any::TypeId::of::<T>(),
            drop: drop_boxed::<T>,
        });
        Ok(())
    }

    fn contexts<T: 'static>(&self) -> impl Iterator<Item = &T> {
        self.contexts
            .iter()
            .filter(|c| c.kind == std::any::TypeId::of::<T>())
            // SAFETY: the type matches, and no callback runs while this borrow lives.
            .map(|c| unsafe { &*(c.pointer as *const T) })
    }

    /// Delivers the daemon's answers until `done` holds or `deadline` passes.
    fn run_until(&mut self, deadline: Instant, done: impl Fn(&Session) -> bool) -> Result<()> {
        // SAFETY: the connection is open until the session is dropped.
        let fd = unsafe { DNSServiceRefSockFD(self.connection) };
        while !self.operations.is_empty() && !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = i32::try_from(left.as_millis().max(1)).unwrap_or(i32::MAX);
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(bonjour_unavailable(0));
            }
            if ready > 0 {
                // SAFETY: the connection is open; callbacks reach live contexts.
                let error = unsafe { DNSServiceProcessResult(self.connection) };
                if error != NO_ERROR {
                    return Err(bonjour_unavailable(error));
                }
            }
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: operations before the connection they share, then the contexts
        // that no callback can reach any more.
        unsafe {
            for operation in self.operations.drain(..) {
                DNSServiceRefDeallocate(operation);
            }
            DNSServiceRefDeallocate(self.connection);
            for context in self.contexts.drain(..) {
                (context.drop)(context.pointer);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_values_are_read_from_length_prefixed_strings() {
        let record = b"\x0eosxvers=26.6.2\x0dMODEL=Mac16,8\x06ecolor";
        assert_eq!(txt_value(record, "model").as_deref(), Some("Mac16,8"));
        assert_eq!(txt_value(record, "osxvers").as_deref(), Some("26.6.2"));
        assert_eq!(txt_value(record, "ecolor"), None);
        assert_eq!(txt_value(record, "missing"), None);
        // A truncated string ends the record instead of reading past it.
        assert_eq!(txt_value(b"\x20model=Mac", "model"), None);
    }
}
