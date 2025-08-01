// Copyright 2015-2018 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::{self, Display};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::str::FromStr;
use core::task::{Context, Poll};
use std::net::SocketAddr;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures_util::{
    future::{BoxFuture, FutureExt},
    stream::Stream,
};
use h3::client::SendRequest;
use h3_quinn::OpenStreams;
use http::header::{self, CONTENT_LENGTH};
use quinn::{Endpoint, EndpointConfig, TransportConfig};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::error::ProtoError;
use crate::http::Version;
use crate::quic::connect_quic;
use crate::rustls::client_config;
use crate::udp::UdpSocket;
use crate::xfer::{DnsRequest, DnsRequestSender, DnsResponse, DnsResponseStream};

use super::ALPN_H3;

/// A DNS client connection for DNS-over-HTTP/3
#[derive(Clone)]
#[must_use = "futures do nothing unless polled"]
pub struct H3ClientStream {
    // Corresponds to the dns-name of the HTTP/3 server
    server_name: Arc<str>,
    name_server: SocketAddr,
    path: Arc<str>,
    send_request: SendRequest<OpenStreams, Bytes>,
    shutdown_tx: mpsc::Sender<()>,
    is_shutdown: bool,
}

impl H3ClientStream {
    /// Builder for H3ClientStream
    pub fn builder() -> H3ClientStreamBuilder {
        H3ClientStreamBuilder {
            crypto_config: None,
            transport_config: Arc::new(super::transport()),
            bind_addr: None,
            disable_grease: false,
        }
    }

    async fn inner_send(
        mut h3: SendRequest<OpenStreams, Bytes>,
        message: Bytes,
        name_server_name: Arc<str>,
        query_path: Arc<str>,
    ) -> Result<DnsResponse, ProtoError> {
        // build up the http request
        let request = crate::http::request::new(
            Version::Http3,
            &name_server_name,
            &query_path,
            message.remaining(),
        );

        let request =
            request.map_err(|err| ProtoError::from(format!("bad http request: {err}")))?;

        debug!("request: {:#?}", request);

        // Send the request
        let mut stream = h3
            .send_request(request)
            .await
            .map_err(|err| ProtoError::from(format!("h3 send_request error: {err}")))?;

        stream
            .send_data(message)
            .await
            .map_err(|e| ProtoError::from(format!("h3 send_data error: {e}")))?;

        stream
            .finish()
            .await
            .map_err(|err| ProtoError::from(format!("received a stream error: {err}")))?;

        let response = stream
            .recv_response()
            .await
            .map_err(|err| ProtoError::from(format!("h3 recv_response error: {err}")))?;

        debug!("got response: {:#?}", response);

        // get the length of packet
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .map(|v| v.to_str())
            .transpose()
            .map_err(|e| ProtoError::from(format!("bad headers received: {e}")))?
            .map(usize::from_str)
            .transpose()
            .map_err(|e| ProtoError::from(format!("bad headers received: {e}")))?;

        // TODO: what is a good max here?
        // clamp(512, 4096) says make sure it is at least 512 bytes, and min 4096 says it is at most 4k
        // just a little protection from malicious actors.
        let mut response_bytes =
            BytesMut::with_capacity(content_length.unwrap_or(512).clamp(512, 4_096));

        while let Some(partial_bytes) = stream
            .recv_data()
            .await
            .map_err(|e| ProtoError::from(format!("h3 recv_data error: {e}")))?
        {
            debug!("got bytes: {}", partial_bytes.remaining());
            response_bytes.put(partial_bytes);

            // assert the length
            if let Some(content_length) = content_length {
                if response_bytes.len() >= content_length {
                    break;
                }
            }
        }

        // assert the length
        if let Some(content_length) = content_length {
            if response_bytes.len() != content_length {
                // TODO: make explicit error type
                return Err(ProtoError::from(format!(
                    "expected byte length: {}, got: {}",
                    content_length,
                    response_bytes.len()
                )));
            }
        }

        // Was it a successful request?
        if !response.status().is_success() {
            let error_string = String::from_utf8_lossy(response_bytes.as_ref());

            // TODO: make explicit error type
            return Err(ProtoError::from(format!(
                "http unsuccessful code: {}, message: {}",
                response.status(),
                error_string
            )));
        } else {
            // verify content type
            {
                // in the case that the ContentType is not specified, we assume it's the standard DNS format
                let content_type = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .map(|h| {
                        h.to_str().map_err(|err| {
                            // TODO: make explicit error type
                            ProtoError::from(format!("ContentType header not a string: {err}"))
                        })
                    })
                    .unwrap_or(Ok(crate::http::MIME_APPLICATION_DNS))?;

                if content_type != crate::http::MIME_APPLICATION_DNS {
                    return Err(ProtoError::from(format!(
                        "ContentType unsupported (must be '{}'): '{}'",
                        crate::http::MIME_APPLICATION_DNS,
                        content_type
                    )));
                }
            }
        };

        // and finally convert the bytes into a DNS message
        DnsResponse::from_buffer(response_bytes.to_vec())
    }
}

impl DnsRequestSender for H3ClientStream {
    /// This indicates that the HTTP message was successfully sent, and we now have the response.RecvStream
    ///
    /// If the request fails, this will return the error, and it should be assumed that the Stream portion of
    ///   this will have no date.
    ///
    /// ```text
    /// RFC 8484              DNS Queries over HTTPS (DoH)          October 2018
    ///
    ///
    /// 4.2.  The HTTP Response
    ///
    ///    The only response type defined in this document is "application/dns-
    ///    message", but it is possible that other response formats will be
    ///    defined in the future.  A DoH server MUST be able to process
    ///    "application/dns-message" request messages.
    ///
    ///    Different response media types will provide more or less information
    ///    from a DNS response.  For example, one response type might include
    ///    information from the DNS header bytes while another might omit it.
    ///    The amount and type of information that a media type gives are solely
    ///    up to the format, which is not defined in this protocol.
    ///
    ///    Each DNS request-response pair is mapped to one HTTP exchange.  The
    ///    responses may be processed and transported in any order using HTTP's
    ///    multi-streaming functionality (see Section 5 of [RFC7540]).
    ///
    ///    Section 5.1 discusses the relationship between DNS and HTTP response
    ///    caching.
    ///
    /// 4.2.1.  Handling DNS and HTTP Errors
    ///
    ///    DNS response codes indicate either success or failure for the DNS
    ///    query.  A successful HTTP response with a 2xx status code (see
    ///    Section 6.3 of [RFC7231]) is used for any valid DNS response,
    ///    regardless of the DNS response code.  For example, a successful 2xx
    ///    HTTP status code is used even with a DNS message whose DNS response
    ///    code indicates failure, such as SERVFAIL or NXDOMAIN.
    ///
    ///    HTTP responses with non-successful HTTP status codes do not contain
    ///    replies to the original DNS question in the HTTP request.  DoH
    ///    clients need to use the same semantic processing of non-successful
    ///    HTTP status codes as other HTTP clients.  This might mean that the
    ///    DoH client retries the query with the same DoH server, such as if
    ///    there are authorization failures (HTTP status code 401; see
    ///    Section 3.1 of [RFC7235]).  It could also mean that the DoH client
    ///    retries with a different DoH server, such as for unsupported media
    ///    types (HTTP status code 415; see Section 6.5.13 of [RFC7231]), or
    ///    where the server cannot generate a representation suitable for the
    ///    client (HTTP status code 406; see Section 6.5.6 of [RFC7231]), and so
    ///    on.
    /// ```
    fn send_message(&mut self, mut request: DnsRequest) -> DnsResponseStream {
        if self.is_shutdown {
            panic!("can not send messages after stream is shutdown")
        }

        // per the RFC, a zero id allows for the HTTP packet to be cached better
        request.set_id(0);

        let bytes = match request.to_vec() {
            Ok(bytes) => bytes,
            Err(err) => return err.into(),
        };

        Box::pin(Self::inner_send(
            self.send_request.clone(),
            Bytes::from(bytes),
            self.server_name.clone(),
            self.path.clone(),
        ))
        .into()
    }

    fn shutdown(&mut self) {
        self.is_shutdown = true;
    }

    fn is_shutdown(&self) -> bool {
        self.is_shutdown
    }
}

impl Stream for H3ClientStream {
    type Item = Result<(), ProtoError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.is_shutdown {
            return Poll::Ready(None);
        }

        // just checking if the connection is ok
        if self.shutdown_tx.is_closed() {
            return Poll::Ready(Some(Err(ProtoError::from(
                "h3 connection is already shutdown",
            ))));
        }

        Poll::Ready(Some(Ok(())))
    }
}

impl Display for H3ClientStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        write!(formatter, "H3({},{})", self.name_server, self.server_name)
    }
}

/// A H3 connection builder for DNS-over-HTTP/3
#[derive(Clone)]
pub struct H3ClientStreamBuilder {
    crypto_config: Option<rustls::ClientConfig>,
    transport_config: Arc<TransportConfig>,
    bind_addr: Option<SocketAddr>,
    disable_grease: bool,
}

impl H3ClientStreamBuilder {
    /// Constructs a new H3ClientStreamBuilder with the associated ClientConfig
    pub fn crypto_config(mut self, crypto_config: rustls::ClientConfig) -> Self {
        self.crypto_config = Some(crypto_config);
        self
    }

    /// Sets the address to connect from.
    pub fn bind_addr(mut self, bind_addr: SocketAddr) -> Self {
        self.bind_addr = Some(bind_addr);
        self
    }

    /// Sets whether to disable GREASE
    pub fn disable_grease(mut self, disable_grease: bool) -> Self {
        self.disable_grease = disable_grease;
        self
    }

    /// Creates a new H3Stream to the specified name_server
    ///
    /// # Arguments
    ///
    /// * `name_server` - IP and Port for the remote DNS resolver
    /// * `server_name` - The DNS name associated with a certificate
    pub fn build(
        self,
        name_server: SocketAddr,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> H3ClientConnect {
        H3ClientConnect(Box::pin(self.connect(name_server, server_name, path)) as _)
    }

    /// Creates a new H3Stream with existing connection
    pub fn build_with_future(
        self,
        socket: Arc<dyn quinn::AsyncUdpSocket>,
        name_server: SocketAddr,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> H3ClientConnect {
        H3ClientConnect(
            Box::pin(self.connect_with_future(socket, name_server, server_name, path)) as _,
        )
    }

    async fn connect_with_future(
        self,
        socket: Arc<dyn quinn::AsyncUdpSocket>,
        name_server: SocketAddr,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> Result<H3ClientStream, ProtoError> {
        let endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        self.connect_inner(endpoint, name_server, server_name, path)
            .await
    }

    async fn connect(
        self,
        name_server: SocketAddr,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> Result<H3ClientStream, ProtoError> {
        let connect = if let Some(bind_addr) = self.bind_addr {
            <tokio::net::UdpSocket as UdpSocket>::connect_with_bind(name_server, bind_addr)
        } else {
            <tokio::net::UdpSocket as UdpSocket>::connect(name_server)
        };

        let socket = connect.await?;
        let socket = socket.into_std()?;
        let endpoint = Endpoint::new(
            EndpointConfig::default(),
            None,
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        self.connect_inner(endpoint, name_server, server_name, path)
            .await
    }

    async fn connect_inner(
        self,
        endpoint: Endpoint,
        name_server: SocketAddr,
        server_name: Arc<str>,
        path: Arc<str>,
    ) -> Result<H3ClientStream, ProtoError> {
        let quic_connection = connect_quic(
            name_server,
            server_name.clone(),
            ALPN_H3,
            match self.crypto_config {
                Some(crypto_config) => crypto_config,
                None => client_config()?,
            },
            self.transport_config,
            endpoint,
        )
        .await?;

        let h3_connection = h3_quinn::Connection::new(quic_connection);
        let (mut driver, send_request) = h3::client::builder()
            .send_grease(!self.disable_grease)
            .build(h3_connection)
            .await
            .map_err(|e| ProtoError::from(format!("h3 connection failed: {e}")))?;

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // TODO: hand this back for others to run rather than spawning here?
        debug!("h3 connection is ready: {}", name_server);
        tokio::spawn(async move {
            tokio::select! {
                error = poll_fn(|cx| driver.poll_close(cx)) => {
                    // `poll_close()` strangely unconditionally returns a `ConnectionError`
                    if !error.is_h3_no_error() {
                        warn!(%error, "h3 connection failed to close")
                    }
                }
                _ = shutdown_rx.recv() => {
                    debug!("h3 connection is shutting down: {}", name_server);
                }
            }
        });

        Ok(H3ClientStream {
            server_name,
            name_server,
            path,
            send_request,
            shutdown_tx,
            is_shutdown: false,
        })
    }
}

/// A future that resolves to an H3ClientStream
pub struct H3ClientConnect(BoxFuture<'static, Result<H3ClientStream, ProtoError>>);

impl Future for H3ClientConnect {
    type Output = Result<H3ClientStream, ProtoError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.poll_unpin(cx)
    }
}

/// A future that resolves to
pub struct H3ClientResponse(BoxFuture<'static, Result<DnsResponse, ProtoError>>);

impl Future for H3ClientResponse {
    type Output = Result<DnsResponse, ProtoError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx).map_err(ProtoError::from)
    }
}

#[cfg(all(
    test,
    any(feature = "rustls-platform-verifier", feature = "webpki-roots")
))]
mod tests {
    use alloc::string::ToString;
    use core::str::FromStr;
    use std::net::SocketAddr;
    use std::println;

    use rustls::KeyLogFile;
    use test_support::subscribe;
    use tokio::runtime::Runtime;
    use tokio::task::JoinSet;

    use crate::op::{Edns, Message, Query};
    use crate::rr::{Name, RecordType};
    use crate::xfer::{DnsRequestOptions, FirstAnswer};

    use super::*;

    #[tokio::test]
    async fn test_h3_google() {
        subscribe();

        let google = SocketAddr::from(([8, 8, 8, 8], 443));
        let mut request = Message::query();
        let query = Query::query(Name::from_str("www.example.com.").unwrap(), RecordType::A);
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let mut client_config = client_config().unwrap();
        client_config.key_log = Arc::new(KeyLogFile::new());

        let mut h3 = H3ClientStream::builder()
            .crypto_config(client_config)
            .build(google, Arc::from("dns.google"), Arc::from("/dns-query"))
            .await
            .expect("h3 connect failed");

        let response = h3
            .send_message(request)
            .first_answer()
            .await
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_a().is_some())
        );

        //
        // assert that the connection works for a second query
        let mut request = Message::query();
        let query = Query::query(
            Name::from_str("www.example.com.").unwrap(),
            RecordType::AAAA,
        );
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let response = h3
            .send_message(request.clone())
            .first_answer()
            .await
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_aaaa().is_some())
        );
    }

    #[tokio::test]
    async fn test_h3_google_with_pure_ip_address_server() {
        subscribe();

        let google = SocketAddr::from(([8, 8, 8, 8], 443));
        let mut request = Message::query();
        let query = Query::query(Name::from_str("www.example.com.").unwrap(), RecordType::A);
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let mut client_config = client_config().unwrap();
        client_config.key_log = Arc::new(KeyLogFile::new());

        let mut h3 = H3ClientStream::builder()
            .crypto_config(client_config)
            .build(
                google,
                Arc::from(google.ip().to_string()),
                Arc::from("/dns-query"),
            )
            .await
            .expect("h3 connect failed");

        let response = h3
            .send_message(request)
            .first_answer()
            .await
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_a().is_some())
        );

        //
        // assert that the connection works for a second query
        let mut request = Message::query();
        let query = Query::query(
            Name::from_str("www.example.com.").unwrap(),
            RecordType::AAAA,
        );
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let response = h3
            .send_message(request.clone())
            .first_answer()
            .await
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_aaaa().is_some())
        );
    }

    #[test]
    fn test_h3_cloudflare() {
        subscribe();

        let cloudflare = SocketAddr::from(([1, 1, 1, 1], 443));
        let mut request = Message::query();
        let query = Query::query(Name::from_str("www.example.com.").unwrap(), RecordType::A);
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let mut client_config = client_config().unwrap();
        client_config.key_log = Arc::new(KeyLogFile::new());

        let connect = H3ClientStream::builder()
            .crypto_config(client_config)
            // Currently CF is using a broken GREASE implementation, see <https://github.com/hyperium/h3/issues/206>.
            .disable_grease(true)
            .build(
                cloudflare,
                Arc::from("cloudflare-dns.com"),
                Arc::from("/dns-query"),
            );

        // tokio runtime stuff...
        let runtime = Runtime::new().expect("could not start runtime");
        let mut h3 = runtime.block_on(connect).expect("h3 connect failed");

        let response = runtime
            .block_on(h3.send_message(request).first_answer())
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_a().is_some())
        );

        //
        // assert that the connection works for a second query
        let mut request = Message::query();
        let query = Query::query(
            Name::from_str("www.example.com.").unwrap(),
            RecordType::AAAA,
        );
        request.add_query(query);
        request.set_recursion_desired(true);
        let mut edns = Edns::new();
        edns.set_version(0);
        edns.set_max_payload(1232);
        *request.extensions_mut() = Some(edns);

        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let response = runtime
            .block_on(h3.send_message(request).first_answer())
            .expect("send_message failed");

        assert!(
            response
                .answers()
                .iter()
                .any(|record| record.data().as_aaaa().is_some())
        );
    }

    #[tokio::test]
    #[allow(clippy::print_stdout)]
    async fn test_h3_client_stream_clonable() {
        subscribe();

        // use google
        let google = SocketAddr::from(([8, 8, 8, 8], 443));

        let mut client_config = client_config().unwrap();
        client_config.key_log = Arc::new(KeyLogFile::new());

        let h3 = H3ClientStream::builder()
            .crypto_config(client_config)
            .build(google, Arc::from("dns.google"), Arc::from("/dns-query"))
            .await
            .expect("h3 connect failed");

        // prepare request
        let mut request = Message::query();
        let query = Query::query(
            Name::from_str("www.example.com.").unwrap(),
            RecordType::AAAA,
        );
        request.add_query(query);
        let request = DnsRequest::new(request, DnsRequestOptions::default());

        let mut join_set = JoinSet::new();

        for i in 0..50 {
            let mut h3 = h3.clone();
            let request = request.clone();

            join_set.spawn(async move {
                let start = std::time::Instant::now();
                h3.send_message(request)
                    .first_answer()
                    .await
                    .expect("send_message failed");
                println!("request[{i}] completed: {:?}", start.elapsed());
            });
        }

        let total = join_set.len();
        let mut idx = 0usize;
        while join_set.join_next().await.is_some() {
            println!("join_set completed {idx}/{total}");
            idx += 1;
        }
    }
}
