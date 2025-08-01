// Copyright 2015-2023 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// https://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Basic protocol message for DNS

use alloc::{boxed::Box, fmt, vec::Vec};
use core::{iter, mem, ops::Deref};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[cfg(any(feature = "std", feature = "no-std-rand"))]
use crate::random;
use crate::{
    error::*,
    op::{Edns, Header, MessageType, OpCode, Query, ResponseCode},
    rr::{Record, RecordType},
    serialize::binary::{BinDecodable, BinDecoder, BinEncodable, BinEncoder, EncodeMode},
    xfer::DnsResponse,
};

/// The basic request and response data structure, used for all DNS protocols.
///
/// [RFC 1035, DOMAIN NAMES - IMPLEMENTATION AND SPECIFICATION, November 1987](https://tools.ietf.org/html/rfc1035)
///
/// ```text
/// 4.1. Format
///
/// All communications inside of the domain protocol are carried in a single
/// format called a message.  The top level format of message is divided
/// into 5 sections (some of which are empty in certain cases) shown below:
///
///     +--------------------------+
///     |        Header            |
///     +--------------------------+
///     |  Question / Zone         | the question for the name server
///     +--------------------------+
///     |   Answer  / Prerequisite | RRs answering the question
///     +--------------------------+
///     | Authority / Update       | RRs pointing toward an authority
///     +--------------------------+
///     |      Additional          | RRs holding additional information
///     +--------------------------+
///
/// The header section is always present.  The header includes fields that
/// specify which of the remaining sections are present, and also specify
/// whether the message is a query or a response, a standard query or some
/// other opcode, etc.
///
/// The names of the sections after the header are derived from their use in
/// standard queries.  The question section contains fields that describe a
/// question to a name server.  These fields are a query type (QTYPE), a
/// query class (QCLASS), and a query domain name (QNAME).  The last three
/// sections have the same format: a possibly empty list of concatenated
/// resource records (RRs).  The answer section contains RRs that answer the
/// question; the authority section contains RRs that point toward an
/// authoritative name server; the additional records section contains RRs
/// which relate to the query, but are not strictly answers for the
/// question.
/// ```
///
/// By default Message is a Query. Use the Message::as_update() to create and update, or
///  Message::new_update()
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
pub struct Message {
    header: Header,
    queries: Vec<Query>,
    answers: Vec<Record>,
    authorities: Vec<Record>,
    additionals: Vec<Record>,
    signature: MessageSignature,
    edns: Option<Edns>,
}

impl Message {
    /// Returns a new "empty" Message
    #[cfg(any(feature = "std", feature = "no-std-rand"))]
    pub fn query() -> Self {
        Self::new(random(), MessageType::Query, OpCode::Query)
    }

    /// Returns a Message constructed with error details to return to a client
    ///
    /// # Arguments
    ///
    /// * `id` - message id should match the request message id
    /// * `op_code` - operation of the request
    /// * `response_code` - the error code for the response
    pub fn error_msg(id: u16, op_code: OpCode, response_code: ResponseCode) -> Self {
        let mut message = Self::response(id, op_code);
        message.set_response_code(response_code);
        message
    }

    /// Returns a new `Message` with `MessageType::Response` and the given header contents
    pub fn response(id: u16, op_code: OpCode) -> Self {
        Self::new(id, MessageType::Response, op_code)
    }

    /// Create a new [`Message`] with the given header contents
    pub fn new(id: u16, message_type: MessageType, op_code: OpCode) -> Self {
        Self {
            header: Header::new(id, message_type, op_code),
            queries: Vec::new(),
            answers: Vec::new(),
            authorities: Vec::new(),
            additionals: Vec::new(),
            signature: MessageSignature::default(),
            edns: None,
        }
    }

    /// Truncates a Message, this blindly removes all response fields and sets truncated to `true`
    pub fn truncate(&self) -> Self {
        // copy header
        let mut header = self.header;
        header.set_truncated(true);
        header
            .set_additional_count(0)
            .set_answer_count(0)
            .set_authority_count(0);

        let mut msg = Self::new(0, MessageType::Query, OpCode::Query);
        msg.header = header;

        // drops additional/answer/nameservers/signature
        // adds query/OPT
        msg.add_queries(self.queries().iter().cloned());
        if let Some(edns) = self.extensions().clone() {
            msg.set_edns(edns);
        }

        // TODO, perhaps just quickly add a few response records here? that we know would fit?
        msg
    }

    /// Sets the [`Header`]
    pub fn set_header(&mut self, header: Header) -> &mut Self {
        self.header = header;
        self
    }

    /// See [`Header::set_id()`]
    pub fn set_id(&mut self, id: u16) -> &mut Self {
        self.header.set_id(id);
        self
    }

    /// See [`Header::set_op_code()`]
    pub fn set_op_code(&mut self, op_code: OpCode) -> &mut Self {
        self.header.set_op_code(op_code);
        self
    }

    /// See [`Header::set_authoritative()`]
    pub fn set_authoritative(&mut self, authoritative: bool) -> &mut Self {
        self.header.set_authoritative(authoritative);
        self
    }

    /// See [`Header::set_truncated()`]
    pub fn set_truncated(&mut self, truncated: bool) -> &mut Self {
        self.header.set_truncated(truncated);
        self
    }

    /// See [`Header::set_recursion_desired()`]
    pub fn set_recursion_desired(&mut self, recursion_desired: bool) -> &mut Self {
        self.header.set_recursion_desired(recursion_desired);
        self
    }

    /// See [`Header::set_recursion_available()`]
    pub fn set_recursion_available(&mut self, recursion_available: bool) -> &mut Self {
        self.header.set_recursion_available(recursion_available);
        self
    }

    /// See [`Header::set_authentic_data()`]
    pub fn set_authentic_data(&mut self, authentic_data: bool) -> &mut Self {
        self.header.set_authentic_data(authentic_data);
        self
    }

    /// See [`Header::set_checking_disabled()`]
    pub fn set_checking_disabled(&mut self, checking_disabled: bool) -> &mut Self {
        self.header.set_checking_disabled(checking_disabled);
        self
    }

    /// See [`Header::set_response_code()`]
    pub fn set_response_code(&mut self, response_code: ResponseCode) -> &mut Self {
        self.header.set_response_code(response_code);
        self
    }

    /// See [`Header::set_query_count()`]
    ///
    /// this count will be ignored during serialization,
    /// where the length of the associated records will be used instead.
    pub fn set_query_count(&mut self, query_count: u16) -> &mut Self {
        self.header.set_query_count(query_count);
        self
    }

    /// See [`Header::set_answer_count()`]
    ///
    /// this count will be ignored during serialization,
    /// where the length of the associated records will be used instead.
    pub fn set_answer_count(&mut self, answer_count: u16) -> &mut Self {
        self.header.set_answer_count(answer_count);
        self
    }

    /// See [`Header::set_authority_count()`]
    ///
    /// this count will be ignored during serialization,
    /// where the length of the associated records will be used instead.
    pub fn set_authority_count(&mut self, authority_count: u16) -> &mut Self {
        self.header.set_authority_count(authority_count);
        self
    }

    /// See [`Header::set_additional_count()`]
    ///
    /// this count will be ignored during serialization,
    /// where the length of the associated records will be used instead.
    pub fn set_additional_count(&mut self, additional_count: u16) -> &mut Self {
        self.header.set_additional_count(additional_count);
        self
    }

    /// Add a query to the Message, either the query response from the server, or the request Query.
    pub fn add_query(&mut self, query: Query) -> &mut Self {
        self.queries.push(query);
        self
    }

    /// Adds an iterator over a set of Queries to be added to the message
    pub fn add_queries<Q, I>(&mut self, queries: Q) -> &mut Self
    where
        Q: IntoIterator<Item = Query, IntoIter = I>,
        I: Iterator<Item = Query>,
    {
        for query in queries {
            self.add_query(query);
        }

        self
    }

    /// Add a record to the Answer section.
    pub fn add_answer(&mut self, record: Record) -> &mut Self {
        self.answers.push(record);
        self
    }

    /// Add all the records from the iterator to the Answer section of the message.
    pub fn add_answers<R, I>(&mut self, records: R) -> &mut Self
    where
        R: IntoIterator<Item = Record, IntoIter = I>,
        I: Iterator<Item = Record>,
    {
        for record in records {
            self.add_answer(record);
        }

        self
    }

    /// Sets the Answer section to the specified set of records.
    ///
    /// # Panics
    ///
    /// Will panic if the Answer section is already non-empty.
    pub fn insert_answers(&mut self, records: Vec<Record>) {
        assert!(self.answers.is_empty());
        self.answers = records;
    }

    /// Add a record to the Authority section.
    pub fn add_authority(&mut self, record: Record) -> &mut Self {
        self.authorities.push(record);
        self
    }

    /// Add all the records from the Iterator to the Authority section of the message.
    pub fn add_authorities<R, I>(&mut self, records: R) -> &mut Self
    where
        R: IntoIterator<Item = Record, IntoIter = I>,
        I: Iterator<Item = Record>,
    {
        for record in records {
            self.add_authority(record);
        }

        self
    }

    /// Sets the Authority section to the specified set of records.
    ///
    /// # Panics
    ///
    /// Will panic if the Authority section is already non-empty.
    pub fn insert_authorities(&mut self, records: Vec<Record>) {
        assert!(self.authorities.is_empty());
        self.authorities = records;
    }

    /// Add a record to the Additional section.
    pub fn add_additional(&mut self, record: Record) -> &mut Self {
        self.additionals.push(record);
        self
    }

    /// Add all the records from the iterator to the Additional section of the message.
    pub fn add_additionals<R, I>(&mut self, records: R) -> &mut Self
    where
        R: IntoIterator<Item = Record, IntoIter = I>,
        I: Iterator<Item = Record>,
    {
        for record in records {
            self.add_additional(record);
        }

        self
    }

    /// Sets the Additional to the specified set of records.
    ///
    /// # Panics
    ///
    /// Will panic if additional records are already associated to the message.
    pub fn insert_additionals(&mut self, records: Vec<Record>) {
        assert!(self.additionals.is_empty());
        self.additionals = records;
    }

    /// Add the EDNS OPT pseudo-RR to the Message
    pub fn set_edns(&mut self, edns: Edns) -> &mut Self {
        self.edns = Some(edns);
        self
    }

    /// Set the signature record for the message.
    ///
    /// This must be used only after all records have been associated. Generally this will be
    /// handled by the client and not need to be used directly
    ///
    /// # Panics
    ///
    /// If the `MessageSignature` specifies a `Record` and the record type is not correct. For
    /// example, providing a `MessageSignature::Tsig` variant with a `Record` with a type other than
    /// `RecordType::TSIG` will panic.
    #[cfg(feature = "__dnssec")]
    pub fn set_signature(&mut self, sig: MessageSignature) -> &mut Self {
        match &sig {
            MessageSignature::Tsig(rec) => assert_eq!(RecordType::TSIG, rec.record_type()),
            MessageSignature::Sig0(rec) => assert_eq!(RecordType::SIG, rec.record_type()),
            _ => {}
        }
        self.signature = sig;
        self
    }

    /// Returns a clone of the `Message` with the message type set to `Response`.
    pub fn to_response(&self) -> Self {
        let mut header = self.header;
        header.set_message_type(MessageType::Response);
        Self {
            header,
            queries: self.queries.clone(),
            answers: self.answers.clone(),
            authorities: self.authorities.clone(),
            additionals: self.additionals.clone(),
            signature: self.signature.clone(),
            edns: self.edns.clone(),
        }
    }

    /// Gets the header of the Message
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// See [`Header::id()`]
    pub fn id(&self) -> u16 {
        self.header.id()
    }

    /// See [`Header::message_type()`]
    pub fn message_type(&self) -> MessageType {
        self.header.message_type()
    }

    /// See [`Header::op_code()`]
    pub fn op_code(&self) -> OpCode {
        self.header.op_code()
    }

    /// See [`Header::authoritative()`]
    pub fn authoritative(&self) -> bool {
        self.header.authoritative()
    }

    /// See [`Header::truncated()`]
    pub fn truncated(&self) -> bool {
        self.header.truncated()
    }

    /// See [`Header::recursion_desired()`]
    pub fn recursion_desired(&self) -> bool {
        self.header.recursion_desired()
    }

    /// See [`Header::recursion_available()`]
    pub fn recursion_available(&self) -> bool {
        self.header.recursion_available()
    }

    /// See [`Header::authentic_data()`]
    pub fn authentic_data(&self) -> bool {
        self.header.authentic_data()
    }

    /// See [`Header::checking_disabled()`]
    pub fn checking_disabled(&self) -> bool {
        self.header.checking_disabled()
    }

    /// # Return value
    ///
    /// The `ResponseCode`, if this is an EDNS message then this will join the section from the OPT
    ///  record to create the EDNS `ResponseCode`
    pub fn response_code(&self) -> ResponseCode {
        self.header.response_code()
    }

    /// ```text
    /// Question        Carries the query name and other query parameters.
    /// ```
    pub fn queries(&self) -> &[Query] {
        &self.queries
    }

    /// Provides mutable access to `queries`
    pub fn queries_mut(&mut self) -> &mut Vec<Query> {
        &mut self.queries
    }

    /// Removes all the answers from the Message
    pub fn take_queries(&mut self) -> Vec<Query> {
        mem::take(&mut self.queries)
    }

    /// ```text
    /// Answer          Carries RRs which directly answer the query.
    /// ```
    pub fn answers(&self) -> &[Record] {
        &self.answers
    }

    /// Provides mutable access to `answers`
    pub fn answers_mut(&mut self) -> &mut Vec<Record> {
        &mut self.answers
    }

    /// Removes the Answer section records from the message
    pub fn take_answers(&mut self) -> Vec<Record> {
        mem::take(&mut self.answers)
    }

    /// ```text
    /// Authority       Carries RRs which describe other authoritative servers.
    ///                 May optionally carry the SOA RR for the authoritative
    ///                 data in the answer section.
    /// ```
    pub fn authorities(&self) -> &[Record] {
        &self.authorities
    }

    /// Provides mutable access to `authorities`
    pub fn authorities_mut(&mut self) -> &mut Vec<Record> {
        &mut self.authorities
    }

    /// Remove the Authority section records from the message
    pub fn take_authorities(&mut self) -> Vec<Record> {
        mem::take(&mut self.authorities)
    }

    /// ```text
    /// Additional      Carries RRs which may be helpful in using the RRs in the
    ///                 other sections.
    /// ```
    pub fn additionals(&self) -> &[Record] {
        &self.additionals
    }

    /// Provides mutable access to `additionals`
    pub fn additionals_mut(&mut self) -> &mut Vec<Record> {
        &mut self.additionals
    }

    /// Remove the Additional section records from the message
    pub fn take_additionals(&mut self) -> Vec<Record> {
        mem::take(&mut self.additionals)
    }

    /// All sections chained
    pub fn all_sections(&self) -> impl Iterator<Item = &Record> {
        self.answers
            .iter()
            .chain(self.authorities.iter())
            .chain(self.additionals.iter())
    }

    /// [RFC 6891, EDNS(0) Extensions, April 2013](https://tools.ietf.org/html/rfc6891#section-6.1.1)
    ///
    /// ```text
    /// 6.1.1.  Basic Elements
    ///
    ///  An OPT pseudo-RR (sometimes called a meta-RR) MAY be added to the
    ///  additional data section of a request.
    ///
    ///  The OPT RR has RR type 41.
    ///
    ///  If an OPT record is present in a received request, compliant
    ///  responders MUST include an OPT record in their respective responses.
    ///
    ///  An OPT record does not carry any DNS data.  It is used only to
    ///  contain control information pertaining to the question-and-answer
    ///  sequence of a specific transaction.  OPT RRs MUST NOT be cached,
    ///  forwarded, or stored in or loaded from Zone Files.
    ///
    ///  The OPT RR MAY be placed anywhere within the additional data section.
    ///  When an OPT RR is included within any DNS message, it MUST be the
    ///  only OPT RR in that message.  If a query message with more than one
    ///  OPT RR is received, a FORMERR (RCODE=1) MUST be returned.  The
    ///  placement flexibility for the OPT RR does not override the need for
    ///  the TSIG or SIG(0) RRs to be the last in the additional section
    ///  whenever they are present.
    /// ```
    /// # Return value
    ///
    /// Optionally returns a reference to EDNS OPT pseudo-RR
    pub fn extensions(&self) -> &Option<Edns> {
        &self.edns
    }

    /// Returns mutable reference of EDNS OPT pseudo-RR
    pub fn extensions_mut(&mut self) -> &mut Option<Edns> {
        &mut self.edns
    }

    /// # Return value
    ///
    /// the max payload value as it's defined in the EDNS OPT pseudo-RR.
    pub fn max_payload(&self) -> u16 {
        let max_size = self.edns.as_ref().map_or(512, Edns::max_payload);
        if max_size < 512 { 512 } else { max_size }
    }

    /// # Return value
    ///
    /// the version as defined in the EDNS record
    pub fn version(&self) -> u8 {
        self.edns.as_ref().map_or(0, Edns::version)
    }

    /// # Return value
    ///
    /// the signature over the message, if any
    pub fn signature(&self) -> &MessageSignature {
        &self.signature
    }

    /// Remove signatures from the Message
    pub fn take_signature(&mut self) -> MessageSignature {
        mem::take(&mut self.signature)
    }

    // TODO: only necessary in tests, should it be removed?
    /// this is necessary to match the counts in the header from the record sections
    ///  this happens implicitly on write_to, so no need to call before write_to
    #[cfg(test)]
    pub fn update_counts(&mut self) -> &mut Self {
        self.header = update_header_counts(
            &self.header,
            self.truncated(),
            HeaderCounts {
                query_count: self.queries.len(),
                answer_count: self.answers.len(),
                authority_count: self.authorities.len(),
                additional_count: self.additionals.len(),
            },
        );
        self
    }

    /// Attempts to read the specified number of `Query`s
    pub fn read_queries(decoder: &mut BinDecoder<'_>, count: usize) -> ProtoResult<Vec<Query>> {
        let mut queries = Vec::with_capacity(count);
        for _ in 0..count {
            queries.push(Query::read(decoder)?);
        }
        Ok(queries)
    }

    /// Attempts to read the specified number of records
    ///
    /// # Returns
    ///
    /// This returns a tuple of first standard Records, then a possibly associated Edns, and then
    /// finally a `MessageSignature` if applicable.
    ///
    /// `MessageSignature::Tsig` and `MessageSignature::Sig0` records are only valid when
    /// found in the additional data section. Further, they must always be the last record
    /// in that section, and are mutually exclusive. It is not possible to have multiple TSIG
    /// or SIG(0) records.
    ///
    /// RFC 2931 §3.1 says:
    ///  "Note: requests and responses can either have a single TSIG or one SIG(0) but not both a
    ///   TSIG and a SIG(0)."
    /// RFC 8945 §5.1 says:
    ///  "This TSIG record MUST be the only TSIG RR in the message and MUST be the last record in
    ///   the additional data section."
    #[cfg_attr(not(feature = "__dnssec"), allow(unused_mut))]
    pub fn read_records(
        decoder: &mut BinDecoder<'_>,
        count: usize,
        is_additional: bool,
    ) -> ProtoResult<(Vec<Record>, Option<Edns>, MessageSignature)> {
        let mut records: Vec<Record> = Vec::with_capacity(count);
        let mut edns: Option<Edns> = None;
        let mut sig = MessageSignature::default();

        for _ in 0..count {
            let record = Record::read(decoder)?;

            // There must be no additional records after a TSIG/SIG(0) record.
            if sig != MessageSignature::Unsigned {
                return Err("TSIG or SIG(0) record must be final resource record".into());
            }

            // OPT, SIG and TSIG records are only allowed in the additional section.
            if !is_additional
                && matches!(
                    record.record_type(),
                    RecordType::OPT | RecordType::SIG | RecordType::TSIG
                )
            {
                return Err(format!(
                    "record type {} only allowed in additional section",
                    record.record_type()
                )
                .into());
            } else if !is_additional {
                records.push(record);
                continue;
            }

            match record.record_type() {
                #[cfg(feature = "__dnssec")]
                RecordType::SIG => sig = MessageSignature::Sig0(record),
                #[cfg(feature = "__dnssec")]
                RecordType::TSIG => sig = MessageSignature::Tsig(record),
                RecordType::OPT => {
                    if edns.is_some() {
                        return Err("more than one edns record present".into());
                    }
                    edns = Some((&record).into());
                }
                _ => {
                    records.push(record);
                }
            }
        }

        Ok((records, edns, sig))
    }

    /// Decodes a message from the buffer.
    pub fn from_vec(buffer: &[u8]) -> ProtoResult<Self> {
        let mut decoder = BinDecoder::new(buffer);
        Self::read(&mut decoder)
    }

    /// Encodes the Message into a buffer
    pub fn to_vec(&self) -> Result<Vec<u8>, ProtoError> {
        // TODO: this feels like the right place to verify the max packet size of the message,
        //  will need to update the header for truncation and the lengths if we send less than the
        //  full response. This needs to conform with the EDNS settings of the server...
        let mut buffer = Vec::with_capacity(512);
        {
            let mut encoder = BinEncoder::new(&mut buffer);
            self.emit(&mut encoder)?;
        }

        Ok(buffer)
    }

    /// Finalize the message prior to sending.
    ///
    /// Subsequent to calling this, the Message should not change.
    pub fn finalize(
        &mut self,
        finalizer: &dyn MessageSigner,
        inception_time: u32,
    ) -> ProtoResult<Option<MessageVerifier>> {
        debug!("finalizing message: {:?}", self);

        #[cfg_attr(not(feature = "__dnssec"), allow(unused_variables))]
        let (signature, verifier) = finalizer.sign_message(self, inception_time)?;

        #[cfg(feature = "__dnssec")]
        {
            self.set_signature(signature);
        }

        Ok(verifier)
    }

    /// Consumes `Message` and returns into components
    pub fn into_parts(self) -> MessageParts {
        self.into()
    }
}

impl From<MessageParts> for Message {
    fn from(msg: MessageParts) -> Self {
        let MessageParts {
            header,
            queries,
            answers,
            authorities,
            additionals,
            signature,
            edns,
        } = msg;
        Self {
            header,
            queries,
            answers,
            authorities,
            additionals,
            signature,
            edns,
        }
    }
}

impl Deref for Message {
    type Target = Header;

    fn deref(&self) -> &Self::Target {
        &self.header
    }
}

/// Consumes `Message` giving public access to fields in `Message` so they can be
/// destructured and taken by value
/// ```rust
/// use hickory_proto::op::{Message, MessageParts};
///
/// let msg = Message::query();
/// let MessageParts { queries, .. } = msg.into_parts();
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageParts {
    /// message header
    pub header: Header,
    /// message queries
    pub queries: Vec<Query>,
    /// message answers
    pub answers: Vec<Record>,
    /// message authorities
    pub authorities: Vec<Record>,
    /// message additional records
    pub additionals: Vec<Record>,
    /// message signature
    pub signature: MessageSignature,
    /// optional edns records
    pub edns: Option<Edns>,
}

impl From<Message> for MessageParts {
    fn from(msg: Message) -> Self {
        let Message {
            header,
            queries,
            answers,
            authorities,
            additionals,
            signature,
            edns,
        } = msg;
        Self {
            header,
            queries,
            answers,
            authorities,
            additionals,
            signature,
            edns,
        }
    }
}

/// Tracks the counts of the records in the Message.
///
/// This is only used internally during serialization.
#[derive(Clone, Copy, Debug)]
pub struct HeaderCounts {
    /// The number of queries in the Message
    pub query_count: usize,
    /// The number of answer records in the Message
    pub answer_count: usize,
    /// The number of authority records in the Message
    pub authority_count: usize,
    /// The number of additional records in the Message
    pub additional_count: usize,
}

/// Returns a new Header with accurate counts for each Message section
pub fn update_header_counts(
    current_header: &Header,
    is_truncated: bool,
    counts: HeaderCounts,
) -> Header {
    assert!(counts.query_count <= u16::MAX as usize);
    assert!(counts.answer_count <= u16::MAX as usize);
    assert!(counts.authority_count <= u16::MAX as usize);
    assert!(counts.additional_count <= u16::MAX as usize);

    // TODO: should the function just take by value?
    let mut header = *current_header;
    header
        .set_query_count(counts.query_count as u16)
        .set_answer_count(counts.answer_count as u16)
        .set_authority_count(counts.authority_count as u16)
        .set_additional_count(counts.additional_count as u16)
        .set_truncated(is_truncated);

    header
}

/// Alias for a function verifying if a message is properly signed
pub type MessageVerifier = Box<dyn FnMut(&[u8]) -> ProtoResult<DnsResponse> + Send>;

/// A trait for adding a final `MessageSignature` to a Message before it is sent.
pub trait MessageSigner: Send + Sync + 'static {
    /// Finalize the provided `Message`, computing a `MessageSignature`, and optionally
    /// providing a `MessageVerifier` for response messages.
    ///
    /// # Arguments
    ///
    /// * `message` - the message to finalize
    /// * `current_time` - the current system time.
    ///
    /// # Return
    ///
    /// A `MessageSignature` to append to the end of the additional data, and optionally
    /// a `MessageVerifier` to use to verify responses provoked by the message.
    fn sign_message(
        &self,
        message: &Message,
        current_time: u32,
    ) -> ProtoResult<(MessageSignature, Option<MessageVerifier>)>;

    /// Return whether the message requires a signature before being sent.
    /// By default, returns true for AXFR and IXFR queries, and Update and Notify messages
    fn should_sign_message(&self, message: &Message) -> bool {
        [OpCode::Update, OpCode::Notify].contains(&message.op_code())
            || message
                .queries()
                .iter()
                .any(|q| [RecordType::AXFR, RecordType::IXFR].contains(&q.query_type()))
    }
}

/// A trait for producing a `MessageSignature` for responses
pub trait ResponseSigner: Send + Sync {
    /// sign produces a `MessageSignature` for the provided encoded, unsigned, response message.
    fn sign(self: Box<Self>, response: &[u8]) -> Result<MessageSignature, ProtoError>;
}

/// Returns the count written and a boolean if it was truncated
pub fn count_was_truncated(result: ProtoResult<usize>) -> ProtoResult<(usize, bool)> {
    match result {
        Ok(count) => Ok((count, false)),
        Err(e) => match e.kind() {
            ProtoErrorKind::NotAllRecordsWritten { count } => Ok((*count, true)),
            _ => Err(e),
        },
    }
}

/// A trait that defines types which can be emitted as a set, with the associated count returned.
pub trait EmitAndCount {
    /// Emit self to the encoder and return the count of items
    fn emit(&mut self, encoder: &mut BinEncoder<'_>) -> ProtoResult<usize>;
}

impl<'e, I: Iterator<Item = &'e E>, E: 'e + BinEncodable> EmitAndCount for I {
    fn emit(&mut self, encoder: &mut BinEncoder<'_>) -> ProtoResult<usize> {
        encoder.emit_all(self)
    }
}

/// Emits the different sections of a message properly
///
/// # Return
///
/// In the case of a successful emit, the final header (updated counts, etc) is returned for help with logging, etc.
#[allow(clippy::too_many_arguments)]
pub fn emit_message_parts<Q, A, N, D>(
    header: &Header,
    queries: &mut Q,
    answers: &mut A,
    authorities: &mut N,
    additionals: &mut D,
    edns: Option<&Edns>,
    signature: &MessageSignature,
    encoder: &mut BinEncoder<'_>,
) -> ProtoResult<Header>
where
    Q: EmitAndCount,
    A: EmitAndCount,
    N: EmitAndCount,
    D: EmitAndCount,
{
    let include_signature = encoder.mode() != EncodeMode::Signing;
    let place = encoder.place::<Header>()?;

    let query_count = queries.emit(encoder)?;
    // TODO: need to do something on max records
    //  return offset of last emitted record.
    let answer_count = count_was_truncated(answers.emit(encoder))?;
    let authority_count = count_was_truncated(authorities.emit(encoder))?;
    let mut additional_count = count_was_truncated(additionals.emit(encoder))?;

    if let Some(mut edns) = edns.cloned() {
        // need to commit the error code
        edns.set_rcode_high(header.response_code().high());

        let count = count_was_truncated(encoder.emit_all(iter::once(&Record::from(&edns))))?;
        additional_count.0 += count.0;
        additional_count.1 |= count.1;
    } else if header.response_code().high() > 0 {
        warn!(
            "response code: {} for request: {} requires EDNS but none available",
            header.response_code(),
            header.id()
        );
    }

    // this is a little hacky, but if we are Verifying a signature, i.e. the original Message
    //  then the SIG0 or TSIG record should not be encoded and the edns record (if it exists) is
    //  already part of the additionals section.
    if include_signature {
        let count = match signature {
            #[cfg(feature = "__dnssec")]
            MessageSignature::Sig0(rec) | MessageSignature::Tsig(rec) => {
                count_was_truncated(encoder.emit_all(iter::once(rec)))?
            }
            MessageSignature::Unsigned => (0, false),
        };
        additional_count.0 += count.0;
        additional_count.1 |= count.1;
    }

    let counts = HeaderCounts {
        query_count,
        answer_count: answer_count.0,
        authority_count: authority_count.0,
        additional_count: additional_count.0,
    };
    let was_truncated =
        header.truncated() || answer_count.1 || authority_count.1 || additional_count.1;

    let final_header = update_header_counts(header, was_truncated, counts);
    place.replace(encoder, final_header)?;
    Ok(final_header)
}

impl BinEncodable for Message {
    fn emit(&self, encoder: &mut BinEncoder<'_>) -> ProtoResult<()> {
        emit_message_parts(
            &self.header,
            &mut self.queries.iter(),
            &mut self.answers.iter(),
            &mut self.authorities.iter(),
            &mut self.additionals.iter(),
            self.edns.as_ref(),
            &self.signature,
            encoder,
        )?;

        Ok(())
    }
}

impl<'r> BinDecodable<'r> for Message {
    fn read(decoder: &mut BinDecoder<'r>) -> ProtoResult<Self> {
        let mut header = Header::read(decoder)?;

        // TODO: return just header, and in the case of the rest of message getting an error.
        //  this could improve error detection while decoding.

        // get the questions
        let count = header.query_count() as usize;
        let mut queries = Vec::with_capacity(count);
        for _ in 0..count {
            queries.push(Query::read(decoder)?);
        }

        // get all counts before header moves
        let answer_count = header.answer_count() as usize;
        let authority_count = header.authority_count() as usize;
        let additional_count = header.additional_count() as usize;

        let (answers, _, _) = Self::read_records(decoder, answer_count, false)?;
        let (authorities, _, _) = Self::read_records(decoder, authority_count, false)?;
        let (additionals, edns, signature) = Self::read_records(decoder, additional_count, true)?;

        // need to grab error code from EDNS (which might have a higher value)
        if let Some(edns) = &edns {
            let high_response_code = edns.rcode_high();
            header.merge_response_code(high_response_code);
        }

        Ok(Self {
            header,
            queries,
            answers,
            authorities,
            additionals,
            signature,
            edns,
        })
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let write_query = |slice, f: &mut fmt::Formatter<'_>| -> Result<(), fmt::Error> {
            for d in slice {
                writeln!(f, ";; {d}")?;
            }

            Ok(())
        };

        let write_slice = |slice, f: &mut fmt::Formatter<'_>| -> Result<(), fmt::Error> {
            for d in slice {
                writeln!(f, "{d}")?;
            }

            Ok(())
        };

        writeln!(f, "; header {header}", header = self.header())?;

        if let Some(edns) = self.extensions() {
            writeln!(f, "; edns {edns}")?;
        }

        writeln!(f, "; query")?;
        write_query(self.queries(), f)?;

        if self.header().message_type() == MessageType::Response
            || self.header().op_code() == OpCode::Update
        {
            writeln!(f, "; answers {}", self.answer_count())?;
            write_slice(self.answers(), f)?;
            writeln!(f, "; authorities {}", self.authority_count())?;
            write_slice(self.authorities(), f)?;
            writeln!(f, "; additionals {}", self.additional_count())?;
            write_slice(self.additionals(), f)?;
        }

        Ok(())
    }
}

/// Indicates how a [Message] is signed.
///
/// Per RFC, the choice of RFC 2931 SIG(0), or RFC 8945 TSIG is mutually exclusive:
/// only one or the other may be used. See [`Message::read_records()`] for more
/// information.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
#[cfg_attr(not(feature = "__dnssec"), allow(missing_copy_implementations))]
pub enum MessageSignature {
    /// The message is not signed, or the dnssec crate feature is not enabled.
    #[default]
    Unsigned,
    /// The message has an RFC 2931 SIG(0) signature [Record].
    #[cfg(feature = "__dnssec")]
    Sig0(Record),
    /// The message has an RFC 8945 TSIG signature [Record].
    #[cfg(feature = "__dnssec")]
    Tsig(Record),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "__dnssec")]
    use crate::rr::RecordType;
    use crate::rr::rdata::A;
    #[cfg(feature = "std")]
    use crate::rr::rdata::OPT;
    #[cfg(feature = "std")]
    use crate::rr::rdata::opt::{ClientSubnet, EdnsCode, EdnsOption};
    use crate::rr::{Name, RData};
    #[cfg(feature = "std")]
    use crate::std::net::IpAddr;
    #[cfg(feature = "std")]
    use crate::std::string::ToString;

    #[test]
    fn test_emit_and_read_header() {
        let mut message = Message::response(10, OpCode::Update);
        message
            .set_authoritative(true)
            .set_truncated(false)
            .set_recursion_desired(true)
            .set_recursion_available(true)
            .set_response_code(ResponseCode::ServFail);

        test_emit_and_read(message);
    }

    #[test]
    fn test_emit_and_read_query() {
        let mut message = Message::response(10, OpCode::Update);
        message
            .set_authoritative(true)
            .set_truncated(true)
            .set_recursion_desired(true)
            .set_recursion_available(true)
            .set_response_code(ResponseCode::ServFail)
            .add_query(Query::new())
            .update_counts(); // we're not testing the query parsing, just message

        test_emit_and_read(message);
    }

    #[test]
    fn test_emit_and_read_records() {
        let mut message = Message::response(10, OpCode::Update);
        message
            .set_authoritative(true)
            .set_truncated(true)
            .set_recursion_desired(true)
            .set_recursion_available(true)
            .set_authentic_data(true)
            .set_checking_disabled(true)
            .set_response_code(ResponseCode::ServFail);

        message.add_answer(Record::stub());
        message.add_authority(Record::stub());
        message.add_additional(Record::stub());
        message.update_counts();

        test_emit_and_read(message);
    }

    #[cfg(test)]
    fn test_emit_and_read(message: Message) {
        let mut byte_vec: Vec<u8> = Vec::with_capacity(512);
        {
            let mut encoder = BinEncoder::new(&mut byte_vec);
            message.emit(&mut encoder).unwrap();
        }

        let mut decoder = BinDecoder::new(&byte_vec);
        let got = Message::read(&mut decoder).unwrap();

        assert_eq!(got, message);
    }

    #[test]
    fn test_header_counts_correction_after_emit_read() {
        let mut message = Message::response(10, OpCode::Update);
        message
            .set_authoritative(true)
            .set_truncated(true)
            .set_recursion_desired(true)
            .set_recursion_available(true)
            .set_authentic_data(true)
            .set_checking_disabled(true)
            .set_response_code(ResponseCode::ServFail);

        message.add_answer(Record::stub());
        message.add_authority(Record::stub());
        message.add_additional(Record::stub());

        // at here, we don't call update_counts and we even set wrong count,
        // because we are trying to test whether the counts in the header
        // are correct after the message is emitted and read.
        message.set_query_count(1);
        message.set_answer_count(5);
        message.set_authority_count(5);
        // message.set_additional_count(1);

        let got = get_message_after_emitting_and_reading(message);

        // make comparison
        assert_eq!(got.query_count(), 0);
        assert_eq!(got.answer_count(), 1);
        assert_eq!(got.authority_count(), 1);
        assert_eq!(got.additional_count(), 1);
    }

    #[cfg(test)]
    fn get_message_after_emitting_and_reading(message: Message) -> Message {
        let mut byte_vec: Vec<u8> = Vec::with_capacity(512);
        {
            let mut encoder = BinEncoder::new(&mut byte_vec);
            message.emit(&mut encoder).unwrap();
        }

        let mut decoder = BinDecoder::new(&byte_vec);

        Message::read(&mut decoder).unwrap()
    }

    #[test]
    fn test_legit_message() {
        #[rustfmt::skip]
        let buf: Vec<u8> = vec![
            0x10, 0x00, 0x81,
            0x80, // id = 4096, response, op=query, recursion_desired, recursion_available, no_error
            0x00, 0x01, 0x00, 0x01, // 1 query, 1 answer,
            0x00, 0x00, 0x00, 0x00, // 0 nameservers, 0 additional record
            0x03, b'w', b'w', b'w', // query --- www.example.com
            0x07, b'e', b'x', b'a', //
            b'm', b'p', b'l', b'e', //
            0x03, b'c', b'o', b'm', //
            0x00,                   // 0 = endname
            0x00, 0x01, 0x00, 0x01, // RecordType = A, Class = IN
            0xC0, 0x0C,             // name pointer to www.example.com
            0x00, 0x01, 0x00, 0x01, // RecordType = A, Class = IN
            0x00, 0x00, 0x00, 0x02, // TTL = 2 seconds
            0x00, 0x04,             // record length = 4 (ipv4 address)
            0x5D, 0xB8, 0xD7, 0x0E, // address = 93.184.215.14
        ];

        let mut decoder = BinDecoder::new(&buf);
        let message = Message::read(&mut decoder).unwrap();

        assert_eq!(message.id(), 4_096);

        let mut buf: Vec<u8> = Vec::with_capacity(512);
        {
            let mut encoder = BinEncoder::new(&mut buf);
            message.emit(&mut encoder).unwrap();
        }

        let mut decoder = BinDecoder::new(&buf);
        let message = Message::read(&mut decoder).unwrap();

        assert_eq!(message.id(), 4_096);
    }

    #[test]
    fn rdata_zero_roundtrip() {
        let buf = &[
            160, 160, 0, 13, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 1, 0,
        ];

        assert!(Message::from_bytes(buf).is_err());
    }

    #[test]
    fn nsec_deserialization() {
        const CRASHING_MESSAGE: &[u8] = &[
            0, 0, 132, 0, 0, 0, 0, 1, 0, 0, 0, 1, 36, 49, 101, 48, 101, 101, 51, 100, 51, 45, 100,
            52, 50, 52, 45, 52, 102, 55, 56, 45, 57, 101, 52, 99, 45, 99, 51, 56, 51, 51, 55, 55,
            56, 48, 102, 50, 98, 5, 108, 111, 99, 97, 108, 0, 0, 1, 128, 1, 0, 0, 0, 120, 0, 4,
            192, 168, 1, 17, 36, 49, 101, 48, 101, 101, 51, 100, 51, 45, 100, 52, 50, 52, 45, 52,
            102, 55, 56, 45, 57, 101, 52, 99, 45, 99, 51, 56, 51, 51, 55, 55, 56, 48, 102, 50, 98,
            5, 108, 111, 99, 97, 108, 0, 0, 47, 128, 1, 0, 0, 0, 120, 0, 5, 192, 70, 0, 1, 64,
        ];

        Message::from_vec(CRASHING_MESSAGE).expect("failed to parse message");
    }

    #[test]
    fn prior_to_pointer() {
        const MESSAGE: &[u8] = include_bytes!("../../tests/test-data/fuzz-prior-to-pointer.rdata");
        let message = Message::from_bytes(MESSAGE).expect("failed to parse message");
        let encoded = message.to_bytes().unwrap();
        Message::from_bytes(&encoded).expect("failed to parse encoded message");
    }

    #[test]
    fn test_read_records_unsigned() {
        let records = vec![
            Record::from_rdata(
                Name::from_labels(vec!["example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            Record::from_rdata(
                Name::from_labels(vec!["www", "example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
        ];
        let result = encode_and_read_records(records.clone(), false);
        let (output_records, edns, signature) = result.unwrap();
        assert_eq!(output_records.len(), records.len());
        assert!(edns.is_none());
        assert_eq!(signature, MessageSignature::Unsigned);
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_read_records_edns() {
        let records = vec![
            Record::from_rdata(
                Name::from_labels(vec!["example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            Record::from_rdata(
                Name::new(),
                0,
                RData::OPT(OPT::new(vec![(
                    EdnsCode::Subnet,
                    EdnsOption::Subnet(ClientSubnet::new(IpAddr::from([127, 0, 0, 1]), 0, 24)),
                )])),
            ),
        ];
        let result = encode_and_read_records(records, true);
        let (output_records, edns, signature) = result.unwrap();
        assert_eq!(output_records.len(), 1); // Only the A record, OPT becomes EDNS
        assert!(edns.is_some());
        assert_eq!(signature, MessageSignature::Unsigned);
    }

    #[cfg(feature = "__dnssec")]
    #[test]
    fn test_read_records_tsig() {
        let records = vec![
            Record::from_rdata(
                Name::from_labels(vec!["example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            Record::from_rdata(
                Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                0,
                RData::Update0(RecordType::TSIG),
            ),
        ];
        let result = encode_and_read_records(records, true);
        let (output_records, edns, signature) = result.unwrap();
        assert_eq!(output_records.len(), 1); // Only the A record, TSIG becomes signature
        assert!(edns.is_none());
        assert!(matches!(signature, MessageSignature::Tsig(_)));
    }

    #[cfg(feature = "__dnssec")]
    #[test]
    fn test_read_records_sig0() {
        let records = vec![
            Record::from_rdata(
                Name::from_labels(vec!["example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            Record::from_rdata(
                Name::from_labels(vec!["sig", "example", "com"]).unwrap(),
                0,
                RData::Update0(RecordType::SIG),
            ),
        ];
        let result = encode_and_read_records(records, true);
        assert!(result.is_ok());
        let (output_records, edns, signature) = result.unwrap();
        assert_eq!(output_records.len(), 1); // Only the A record, SIG0 becomes signature
        assert!(edns.is_none());
        assert!(matches!(signature, MessageSignature::Sig0(_)));
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_edns_tsig() {
        let records = vec![
            Record::from_rdata(
                Name::from_labels(vec!["example", "com"]).unwrap(),
                300,
                RData::A(A::new(127, 0, 0, 1)),
            ),
            Record::from_rdata(
                Name::new(),
                0,
                RData::OPT(OPT::new(vec![(
                    EdnsCode::Subnet,
                    EdnsOption::Subnet(ClientSubnet::new(IpAddr::from([127, 0, 0, 1]), 0, 24)),
                )])),
            ),
            Record::from_rdata(
                Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                0,
                RData::Update0(RecordType::TSIG),
            ),
        ];

        let result = encode_and_read_records(records, true);
        assert!(result.is_ok());
        let (output_records, edns, signature) = result.unwrap();
        assert_eq!(output_records.len(), 1); // Only the A record
        assert!(edns.is_some());
        assert!(matches!(signature, MessageSignature::Tsig(_)));
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_read_records_unsigned_multiple_edns() {
        let opt_record = Record::from_rdata(
            Name::new(),
            0,
            RData::OPT(OPT::new(vec![(
                EdnsCode::Subnet,
                EdnsOption::Subnet(ClientSubnet::new(IpAddr::from([127, 0, 0, 1]), 0, 24)),
            )])),
        );
        let error = encode_and_read_records(
            vec![
                opt_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                opt_record.clone(),
            ],
            true,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("more than one edns record present")
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_read_records_opt_not_additional() {
        let opt_record = Record::from_rdata(
            Name::new(),
            0,
            RData::OPT(OPT::new(vec![(
                EdnsCode::Subnet,
                EdnsOption::Subnet(ClientSubnet::new(IpAddr::from([127, 0, 0, 1]), 0, 24)),
            )])),
        );
        let err = encode_and_read_records(
            vec![
                opt_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
            ],
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("record type OPT only allowed in additional section")
        );
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_signed_multiple_edns() {
        let opt_record = Record::from_rdata(
            Name::new(),
            0,
            RData::OPT(OPT::new(vec![(
                EdnsCode::Subnet,
                EdnsOption::Subnet(ClientSubnet::new(IpAddr::from([127, 0, 0, 1]), 0, 24)),
            )])),
        );
        let error = encode_and_read_records(
            vec![
                opt_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                opt_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::TSIG),
                ),
            ],
            true,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("more than one edns record present")
        );
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_tsig_not_additional() {
        let err = encode_and_read_records(
            vec![
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                Record::from_rdata(
                    Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::TSIG),
                ),
            ],
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("record type TSIG only allowed in additional section")
        );
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_sig0_not_additional() {
        let err = encode_and_read_records(
            vec![
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                Record::from_rdata(
                    Name::from_labels(vec!["sig0", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::SIG),
                ),
            ],
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("record type SIG only allowed in additional section")
        );
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_tsig_not_last() {
        let a_record = Record::from_rdata(
            Name::from_labels(vec!["example", "com"]).unwrap(),
            300,
            RData::A(A::new(127, 0, 0, 1)),
        );
        let error = encode_and_read_records(
            vec![
                a_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::TSIG),
                ),
                a_record.clone(),
            ],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("TSIG or SIG(0) record must be final"));
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_sig0_not_last() {
        let a_record = Record::from_rdata(
            Name::from_labels(vec!["example", "com"]).unwrap(),
            300,
            RData::A(A::new(127, 0, 0, 1)),
        );
        let error = encode_and_read_records(
            vec![
                a_record.clone(),
                Record::from_rdata(
                    Name::from_labels(vec!["sig0", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::SIG),
                ),
                a_record.clone(),
            ],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("TSIG or SIG(0) record must be final"));
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_both_sig0_tsig() {
        let error = encode_and_read_records(
            vec![
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                Record::from_rdata(
                    Name::from_labels(vec!["sig0", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::SIG),
                ),
                Record::from_rdata(
                    Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
                    0,
                    RData::Update0(RecordType::TSIG),
                ),
            ],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("TSIG or SIG(0) record must be final"));
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_multiple_tsig() {
        let tsig_record = Record::from_rdata(
            Name::from_labels(vec!["tsig", "example", "com"]).unwrap(),
            0,
            RData::Update0(RecordType::TSIG),
        );
        let error = encode_and_read_records(
            vec![
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                tsig_record.clone(),
                tsig_record.clone(),
            ],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("TSIG or SIG(0) record must be final"));
    }

    #[cfg(all(feature = "std", feature = "__dnssec"))]
    #[test]
    fn test_read_records_multiple_sig0() {
        let sig0_record = Record::from_rdata(
            Name::from_labels(vec!["sig0", "example", "com"]).unwrap(),
            0,
            RData::Update0(RecordType::SIG),
        );
        let error = encode_and_read_records(
            vec![
                Record::from_rdata(
                    Name::from_labels(vec!["example", "com"]).unwrap(),
                    300,
                    RData::A(A::new(127, 0, 0, 1)),
                ),
                sig0_record.clone(),
                sig0_record.clone(),
            ],
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("TSIG or SIG(0) record must be final"));
    }

    fn encode_and_read_records(
        records: Vec<Record>,
        is_additional: bool,
    ) -> ProtoResult<(Vec<Record>, Option<Edns>, MessageSignature)> {
        let mut bytes = Vec::new();
        let mut encoder = BinEncoder::new(&mut bytes);
        encoder.emit_all(records.iter())?;
        Message::read_records(&mut BinDecoder::new(&bytes), records.len(), is_additional)
    }
}
