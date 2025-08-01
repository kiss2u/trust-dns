pub mod mut_message_client;

use std::{
    collections::HashMap,
    env,
    io::{BufRead, BufReader, Write, stdout},
    net::SocketAddr,
    panic::{UnwindSafe, catch_unwind},
    process::{Command, Stdio},
    str::FromStr,
    sync::*,
    thread,
    time::*,
};

use regex::Regex;
use tokio::runtime::Runtime;
use tracing::{info, warn};

#[cfg(feature = "__dnssec")]
use hickory_client::client::Client;
use hickory_client::{client::ClientHandle, proto::xfer::DnsResponse};
#[cfg(feature = "__dnssec")]
use hickory_proto::dnssec::Algorithm;
use hickory_proto::{
    ProtoError,
    op::ResponseCode,
    rr::{DNSClass, Name, RData, RecordType, rdata::A},
    xfer::Protocol,
};

#[derive(Debug, Default)]
pub struct SocketPort {
    v4: u16,
    v6: u16,
}

#[derive(Debug, Default)]
pub struct SocketPorts(HashMap<ServerProtocol, SocketPort>);

impl SocketPorts {
    /// This will overwrite the existing value
    pub fn put(&mut self, protocol: impl Into<ServerProtocol>, addr: SocketAddr) {
        let entry = self.0.entry(protocol.into()).or_default();

        if addr.is_ipv4() {
            entry.v4 = addr.port();
        } else {
            entry.v6 = addr.port();
        }
    }

    /// Assumes there is only one V4 addr for the IP based on the usage in the Server
    pub fn get_v4(&self, protocol: impl Into<ServerProtocol>) -> Option<u16> {
        self.0
            .get(&protocol.into())
            .iter()
            .find_map(|ports| if ports.v4 == 0 { None } else { Some(ports.v4) })
    }

    /// Assumes there is only one V4 addr for the IP based on the usage in the Server
    pub fn get_v6(&self, protocol: impl Into<ServerProtocol>) -> Option<u16> {
        self.0
            .get(&protocol.into())
            .iter()
            .find_map(|ports| if ports.v6 == 0 { None } else { Some(ports.v6) })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ServerProtocol {
    Dns(Protocol),
    #[cfg(feature = "metrics")]
    PrometheusMetrics,
}

impl From<Protocol> for ServerProtocol {
    fn from(proto: Protocol) -> Self {
        Self::Dns(proto)
    }
}

#[cfg(feature = "__dnssec")]
use self::mut_message_client::MutMessageHandle;

fn collect_and_print<R: BufRead>(read: &mut R, output: &mut String) {
    output.clear();
    read.read_line(output).expect("could not read stdio");

    if !output.is_empty() {
        // uncomment for debugging
        // println!("SRV: {}", output.trim_end());
    }
}

/// Spins up a Server and handles shutting it down after running the test
#[allow(dead_code)]
pub fn named_test_harness<F, R>(toml: &str, test: F)
where
    F: FnOnce(SocketPorts) -> R + UnwindSafe,
{
    let server_path = env::var("TDNS_WORKSPACE_ROOT").unwrap_or_else(|_| "..".to_owned());
    println!("using server src path: {server_path}");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hickory-dns"));
    command
        .stdout(Stdio::piped())
        .env(
            "RUST_LOG",
            "hickory_dns=debug,hickory_client=debug,hickory_proto=debug,hickory_resolver=debug,hickory_server=debug",
        )
        .arg("-d")
        .arg(format!(
            "--config={server_path}/tests/test-data/test_configs/{toml}"
        ))
        .arg(format!(
            "--zonedir={server_path}/tests/test-data/test_configs"
        ))
        .arg(format!("--port={}", 0));
    #[cfg(feature = "__tls")]
    command.arg(format!("--tls-port={}", 0));
    #[cfg(feature = "__https")]
    command.arg(format!("--https-port={}", 0));
    #[cfg(feature = "__quic")]
    command.arg(format!("--quic-port={}", 0));
    #[cfg(feature = "prometheus-metrics")]
    command.arg(format!("--prometheus-listen-address=127.0.0.1:{}", 0));

    println!("named cli options: {command:#?}");

    let mut named = command.spawn().expect("failed to start named");

    println!("server starting");

    let mut named_out = BufReader::new(named.stdout.take().expect("no stdout"));

    // forced thread killer
    let named = Arc::new(Mutex::new(named));
    let named_killer = Arc::clone(&named);
    let succeeded = Arc::new(atomic::AtomicBool::new(false));
    let succeeded_clone = succeeded.clone();
    let killer_join = thread::Builder::new()
        .name("thread_killer".to_string())
        .spawn(move || {
            let succeeded = succeeded_clone;

            let kill_named = || {
                info!("killing named");

                let mut named = named_killer.lock().unwrap();
                if let Err(error) = named.kill() {
                    warn!(?error, "warning: failed to kill named");
                    return;
                }
                if let Err(error) = named.wait() {
                    warn!(?error, "warning: failed to wait for named");
                }
            };

            for _ in 0..30 {
                thread::sleep(Duration::from_secs(1));
                if succeeded.load(atomic::Ordering::Relaxed) {
                    kill_named();
                    return;
                }
            }

            kill_named();

            println!("Thread Killer has been awoken, killing process");
            std::process::exit(-1);
        })
        .expect("could not start thread killer");

    // These will be collected from the server startup'
    let mut socket_ports = SocketPorts::default();

    // we should get the correct output before 1000 lines...
    let mut output = String::new();
    let mut found = false;
    let wait_for_start_until = Instant::now() + Duration::from_secs(60);

    // Search strings for the ports used during testing
    let addr_regex = Regex::new(
        r"listening for (UDP|TCP|TLS|HTTPS|QUIC|Prometheus metrics) on ((?:(?:0\.0\.0\.0)|(?:127\.0\.0\.1)|(?:\[::\])):\d+)",
    )
    .unwrap();

    while Instant::now() < wait_for_start_until {
        {
            if let Some(ret_code) = named
                .lock()
                .unwrap()
                .try_wait()
                .expect("failed to check status of named")
            {
                panic!("named has already exited with code: {ret_code}");
            }
        }

        collect_and_print(&mut named_out, &mut output);

        if let Some(addr) = addr_regex.captures(&output) {
            let proto = addr.get(1).expect("missing protocol").as_str();
            let socket_addr = addr.get(2).expect("missing socket addr").as_str();

            let socket_addr =
                SocketAddr::from_str(socket_addr).expect("could not parse socket_addr");

            match proto {
                "UDP" => socket_ports.put(Protocol::Udp, socket_addr),
                "TCP" => socket_ports.put(Protocol::Tcp, socket_addr),
                #[cfg(feature = "__tls")]
                "TLS" => socket_ports.put(Protocol::Tls, socket_addr),
                #[cfg(feature = "__https")]
                "HTTPS" => socket_ports.put(Protocol::Https, socket_addr),
                #[cfg(feature = "__quic")]
                "QUIC" => socket_ports.put(Protocol::Quic, socket_addr),
                #[cfg(feature = "metrics")]
                "Prometheus metrics" => {
                    socket_ports.put(ServerProtocol::PrometheusMetrics, socket_addr)
                }
                _ => panic!("unsupported protocol: {proto}"),
            }
        } else if output.contains("awaiting connections...") {
            found = true;
            break;
        }
    }

    stdout().flush().unwrap();
    assert!(found);
    println!("Test server started. ports: {socket_ports:?}",);

    // spawn a thread to capture stdout
    let succeeded_clone = succeeded.clone();
    thread::Builder::new()
        .name("named stdout".into())
        .spawn(move || {
            let succeeded = succeeded_clone;
            while !succeeded.load(atomic::Ordering::Relaxed) {
                collect_and_print(&mut named_out, &mut output);

                if let Some(_ret_code) = named
                    .lock()
                    .unwrap()
                    .try_wait()
                    .expect("failed to check status of named")
                {
                    // uncomment for debugging:
                    // println!("named exited with code: {}", _ret_code);
                }
            }
        })
        .expect("no thread available");

    println!("running test...");

    let result = catch_unwind(move || test(socket_ports));

    println!("test completed");
    succeeded.store(true, atomic::Ordering::Relaxed);
    killer_join.join().expect("join failed");

    assert!(result.is_ok(), "test failed");
}

pub fn query_message<C: ClientHandle>(
    io_loop: &mut Runtime,
    client: &mut C,
    name: Name,
    record_type: RecordType,
) -> Result<DnsResponse, ProtoError> {
    println!("sending request: {name} for: {record_type}");
    io_loop.block_on(client.query(name, DNSClass::IN, record_type))
}

// This only validates that a query to the server works, it shouldn't be used for more than this.
//  i.e. more complex checks live with the clients and authorities to validate deeper functionality
#[allow(dead_code)]
pub fn query_a<C: ClientHandle>(io_loop: &mut Runtime, client: &mut C) {
    let name = Name::from_str("www.example.com.").unwrap();
    let response = query_message(io_loop, client, name, RecordType::A).unwrap();
    let record = &response.answers()[0];

    if let RData::A(address) = record.data() {
        assert_eq!(address, &A::new(127, 0, 0, 1))
    } else {
        panic!("wrong RDATA")
    }
}

// This only validates that a query to the server works, it shouldn't be used for more than this.
//  i.e. more complex checks live with the clients and authorities to validate deeper functionality
#[allow(dead_code)]
pub fn query_a_refused<C: ClientHandle>(io_loop: &mut Runtime, client: &mut C) {
    let name = Name::from_str("www.example.com.").unwrap();
    let response = query_message(io_loop, client, name, RecordType::A).unwrap();

    assert_eq!(response.response_code(), ResponseCode::Refused);
}

// This only validates that a query to the server works, it shouldn't be used for more than this.
//  i.e. more complex checks live with the clients and authorities to validate deeper functionality
#[allow(dead_code)]
#[cfg(feature = "__dnssec")]
pub fn query_all_dnssec(io_loop: &mut Runtime, client: Client, algorithm: Algorithm) {
    use hickory_proto::{
        dnssec::{
            PublicKey,
            rdata::{DNSKEY, RRSIG},
        },
        rr::{Record, RecordData},
    };

    let name = Name::from_str("example.com.").unwrap();
    let mut client = MutMessageHandle::new(client);
    client.lookup_options.dnssec_ok = true;

    let response = query_message(io_loop, &mut client, name.clone(), RecordType::DNSKEY).unwrap();

    let dnskey = response
        .answers()
        .iter()
        .map(Record::data)
        .filter_map(DNSKEY::try_borrow)
        .find(|d| d.public_key().algorithm() == algorithm);
    assert!(dnskey.is_some(), "DNSKEY not found");

    let response = query_message(io_loop, &mut client, name, RecordType::DNSKEY).unwrap();

    let rrsig = response
        .answers()
        .iter()
        .map(Record::data)
        .filter_map(RRSIG::try_borrow)
        .filter(|rrsig| rrsig.input().algorithm == algorithm)
        .find(|rrsig| rrsig.input().type_covered == RecordType::DNSKEY);
    assert!(rrsig.is_some(), "Associated RRSIG not found");
}
