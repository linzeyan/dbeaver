//! The BigQuery Storage Read API, which is the whole reason this driver exists.
//!
//! **No server has answered any of this.** The messages below are transcribed
//! from `google/cloud/bigquery/storage/v1/{storage,stream,arrow}.proto`, and the
//! field numbers in them are the one thing in this crate that no test can check:
//! a wrong number does not fail to compile and does not fail to decode — prost
//! skips fields it does not know — it produces an empty result and no
//! complaint. That is stated here rather than buried, because it is the single
//! most likely way for this file to be wrong.
//!
//! **Why the messages are written by hand rather than generated.** Two reasons,
//! and the second is the important one.
//!
//! The first is that generating them means `tonic-build`, a build script and
//! `protoc` on every machine that compiles this workspace, to produce four
//! messages and two calls.
//!
//! The second is that generated code would be *wrong for this driver*. `prost`
//! renders a proto `bytes` field as `Vec<u8>` unless told otherwise, and
//! decoding into a `Vec<u8>` copies: prost has to allocate and memcpy, because a
//! `Vec` owns its allocation. Declared as `bytes::Bytes` — which is what
//! `#[prost(bytes = "bytes")]` below does — the same field is decoded with
//! `Buf::copy_to_bytes`, and tonic's decode buffer answers that by splitting the
//! buffer it already holds. So the Arrow IPC body arrives as a window into the
//! bytes tonic read off the socket, and `Buffer::from(Bytes)` then makes the
//! Arrow arrays windows into the same memory. Generated code would copy every
//! batch on the way in and nothing would say so. This is the same shape of
//! finding the Flight SQL driver records about `arrow_flight::decode`, arrived
//! at one layer lower.
//!
//! **What is asked of the session, and why so little.** `CreateReadSession`
//! takes an `ArrowSerializationOptions` whose only field is a compression codec,
//! and this driver sends the message with that field unset. That is not
//! laziness: `LZ4_FRAME` and `ZSTD` would make every batch arrive compressed,
//! and decompressing it is precisely the transcoding step this driver is
//! supposed not to have. Fewer bytes on the wire in exchange for a copy of every
//! one of them is the wrong trade for a client that hands its batches straight
//! to a grid.
//!
//! **One stream, not many.** `max_stream_count: 1` asks BigQuery to produce the
//! result as a single ordered stream. The API's reason for existing is the
//! opposite — a data-processing job takes as many streams as it has workers and
//! reads them in parallel — but a grid shows rows in an order and a client that
//! interleaved four streams would show them in none. A `SELECT … ORDER BY` whose
//! ordering was thrown away by the reader is worse than a slow one.

use arrow::array::{ArrayRef, RecordBatch};
use arrow::buffer::Buffer;
use arrow::datatypes::{Schema, SchemaRef};
use arrow::error::ArrowError;
use bytes::Bytes;
use prost::Message;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::Channel;

use crate::BigQueryError;

/// The gRPC endpoint. One global address, as the REST API has.
pub(crate) const ENDPOINT: &str = "https://bigquerystorage.googleapis.com";

const CREATE_READ_SESSION: &str =
    "/google.cloud.bigquery.storage.v1.BigQueryRead/CreateReadSession";
const READ_ROWS: &str = "/google.cloud.bigquery.storage.v1.BigQueryRead/ReadRows";

/// `DataFormat.ARROW`.
///
/// 2 and not 1: `AVRO` is 1 in this enum, and the two are close enough together
/// that getting it wrong would produce a session whose rows are Avro and a
/// decoder that reads them as Arrow.
const FORMAT_ARROW: i32 = 2;

/// How large a `ReadRowsResponse` this side will accept.
///
/// tonic's default is 4 MiB, which is a sensible ceiling for a request/response
/// API and the wrong one here: the Storage Read API's whole purpose is large
/// batches, and a result whose batches exceed the limit fails partway through
/// with a message about message size rather than anything about the query. 128
/// MiB is above anything the service is documented to send and still a bound
/// rather than none.
const MAX_RESPONSE: usize = 128 * 1024 * 1024;

// ---------------------------------------------------------------------------
// The protocol, transcribed
// ---------------------------------------------------------------------------

/// `google.cloud.bigquery.storage.v1.CreateReadSessionRequest`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct CreateReadSessionRequest {
    /// `projects/{project}` — the project that is *billed* for the read, which
    /// is not necessarily the project holding the table.
    #[prost(string, tag = "1")]
    pub parent: String,
    #[prost(message, optional, tag = "2")]
    pub read_session: Option<ReadSession>,
    #[prost(int32, tag = "3")]
    pub max_stream_count: i32,
}

/// `google.cloud.bigquery.storage.v1.ReadSession`, in the parts this driver
/// reads or writes.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadSession {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(enumeration = "i32", tag = "3")]
    pub data_format: i32,
    /// The schema of the rows about to arrive, as a serialized Arrow IPC schema
    /// message. Tag 5 because tag 4 is the Avro schema in the same `oneof`.
    #[prost(message, optional, tag = "5")]
    pub arrow_schema: Option<ArrowSchema>,
    /// `projects/{p}/datasets/{d}/tables/{t}`.
    #[prost(string, tag = "6")]
    pub table: String,
    #[prost(message, optional, tag = "8")]
    pub read_options: Option<TableReadOptions>,
    #[prost(message, repeated, tag = "10")]
    pub streams: Vec<ReadStream>,
    #[prost(int64, tag = "14")]
    pub estimated_row_count: i64,
}

/// `google.cloud.bigquery.storage.v1.ReadSession.TableReadOptions`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct TableReadOptions {
    #[prost(message, optional, tag = "3")]
    pub arrow_serialization_options: Option<ArrowSerializationOptions>,
}

/// `google.cloud.bigquery.storage.v1.ArrowSerializationOptions`.
///
/// Its only field is `buffer_compression`, and this driver leaves it at the
/// default — see the module comment. The message is still sent, present and
/// empty, because sending it is how a client states which serialization it is
/// asking about.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ArrowSerializationOptions {
    #[prost(enumeration = "i32", tag = "2")]
    pub buffer_compression: i32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadStream {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ArrowSchema {
    #[prost(bytes = "bytes", tag = "1")]
    pub serialized_schema: Bytes,
}

/// `google.cloud.bigquery.storage.v1.ArrowRecordBatch`.
///
/// `serialized_record_batch` is the field the whole driver turns on, and its
/// type here is not a detail: `bytes = "bytes"` is what makes prost decode it
/// with `Buf::copy_to_bytes` rather than by allocating and copying. See the
/// module comment.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ArrowRecordBatch {
    #[prost(bytes = "bytes", tag = "1")]
    pub serialized_record_batch: Bytes,
}

/// `google.cloud.bigquery.storage.v1.ReadRowsRequest`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadRowsRequest {
    #[prost(string, tag = "1")]
    pub read_stream: String,
    /// Where in the stream to start. Sent as 0 and never used: this driver reads
    /// a stream forward once. It is here because a stream that fails partway
    /// through can be resumed at the row it reached, and leaving the field out
    /// would make that look impossible rather than unimplemented.
    #[prost(int64, tag = "2")]
    pub offset: i64,
}

/// `google.cloud.bigquery.storage.v1.ReadRowsResponse`, in the parts this driver
/// reads.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadRowsResponse {
    /// Tag 4 of the `rows` oneof; tag 3 is `avro_rows`.
    #[prost(message, optional, tag = "4")]
    pub arrow_record_batch: Option<ArrowRecordBatch>,
    #[prost(int64, tag = "6")]
    pub row_count: i64,
    /// Tag 8 of the `schema` oneof, sent on the first response of a stream. The
    /// session already carried it, so this is read only to notice a stream that
    /// disagrees with its own session.
    #[prost(message, optional, tag = "8")]
    pub arrow_schema: Option<ArrowSchema>,
}

// ---------------------------------------------------------------------------
// The two calls
// ---------------------------------------------------------------------------

/// A gRPC client aimed at the Storage Read API.
///
/// A `Channel` and not a connection, as in the Flight SQL driver: tonic
/// multiplexes every call over one HTTP/2 connection, so a second call costs a
/// stream rather than a socket.
#[derive(Clone)]
pub(crate) struct Read {
    channel: Channel,
}

impl Read {
    /// Prepares the channel, with TLS, because there is no plaintext form of
    /// this endpoint.
    ///
    /// Lazily, which is a decision rather than a shortcut. A session that only
    /// browses the navigator never reads a row and never touches this service at
    /// all, so a TLS handshake with it at connection time is a connection dialog
    /// made slower for something that may never happen — and a second thing that
    /// can fail while the user is looking at a form about the first. Connecting
    /// on first use puts any failure where the read is, which is where it
    /// belongs.
    pub fn lazy() -> Result<Read, BigQueryError> {
        let channel = tonic::transport::Endpoint::from_static(ENDPOINT)
            .tls_config(tonic::transport::ClientTlsConfig::new().with_webpki_roots())
            .map_err(|e| BigQueryError::Transport(format!("{ENDPOINT}: {e}")))?
            .connect_lazy();
        Ok(Read { channel })
    }

    /// Asks BigQuery to prepare a table for reading as Arrow.
    ///
    /// The session that comes back carries the schema and the streams; nothing
    /// has been read yet, and the session expires on its own if nobody does.
    pub async fn create_session(
        &self,
        token: &str,
        project: &str,
        table: &str,
    ) -> Result<ReadSession, BigQueryError> {
        let request = CreateReadSessionRequest {
            parent: format!("projects/{project}"),
            read_session: Some(ReadSession {
                data_format: FORMAT_ARROW,
                table: table.to_string(),
                read_options: Some(TableReadOptions {
                    arrow_serialization_options: Some(ArrowSerializationOptions::default()),
                }),
                ..ReadSession::default()
            }),
            // One stream; see the module comment.
            max_stream_count: 1,
        };
        let mut grpc = self.grpc();
        grpc.ready()
            .await
            .map_err(|e| BigQueryError::Transport(crate::with_causes(&e, ENDPOINT)))?;
        let response = grpc
            .unary(
                self.request(request, token, "read_session.table", table)?,
                PathAndQuery::from_static(CREATE_READ_SESSION),
                tonic_prost::ProstCodec::default(),
            )
            .await
            .map_err(crate::server_said)?;
        Ok(response.into_inner())
    }

    /// Starts reading one stream of a session.
    pub async fn read_rows(
        &self,
        token: &str,
        stream: &str,
    ) -> Result<tonic::Streaming<ReadRowsResponse>, BigQueryError> {
        let request = ReadRowsRequest {
            read_stream: stream.to_string(),
            offset: 0,
        };
        let mut grpc = self.grpc();
        grpc.ready()
            .await
            .map_err(|e| BigQueryError::Transport(crate::with_causes(&e, ENDPOINT)))?;
        let response = grpc
            .server_streaming(
                self.request(request, token, "read_stream", stream)?,
                PathAndQuery::from_static(READ_ROWS),
                tonic_prost::ProstCodec::default(),
            )
            .await
            .map_err(crate::server_said)?;
        Ok(response.into_inner())
    }

    fn grpc(&self) -> tonic::client::Grpc<Channel> {
        tonic::client::Grpc::new(self.channel.clone()).max_decoding_message_size(MAX_RESPONSE)
    }

    /// One request, with the two headers Google's front end reads.
    ///
    /// `x-goog-request-params` is how the routing layer learns which region the
    /// resource is in before the message has been parsed. Google's own client
    /// libraries send it on every call and the API is documented to work without
    /// it; sending it costs a header and is the difference between a read routed
    /// to the region the table is in and one routed by a default.
    fn request<T>(
        &self,
        message: T,
        token: &str,
        parameter: &str,
        resource: &str,
    ) -> Result<tonic::Request<T>, BigQueryError> {
        let mut request = tonic::Request::new(message);
        let bearer = format!("Bearer {token}").parse().map_err(|_| {
            BigQueryError::Credentials("the access token is not a header value".to_string())
        })?;
        request.metadata_mut().insert("authorization", bearer);
        let params = format!(
            "{parameter}={}",
            percent_encoding::utf8_percent_encode(resource, crate::UNRESERVED)
        );
        if let Ok(value) = params.parse() {
            request
                .metadata_mut()
                .insert("x-goog-request-params", value);
        }
        Ok(request)
    }
}

// ---------------------------------------------------------------------------
// Arrow, decoded in place
// ---------------------------------------------------------------------------

/// One Arrow IPC message taken apart into its header and its body.
///
/// The encapsulated format, which is what `serialized_record_batch` and
/// `serialized_schema` both hold: a continuation marker, the length of the
/// metadata, the flatbuffer, and then the body. The writer pads the metadata so
/// that the body starts on an 8-byte boundary *relative to the message*, which
/// is what makes decoding in place possible at all — land the message itself on
/// an 8-byte address and every buffer in it is already where Arrow wants it.
///
/// Both framings are read. Arrow 0.15 introduced the `0xFFFFFFFF` continuation
/// marker and everything current writes it; a message without one begins
/// directly with the metadata length. Accepting both costs one branch and the
/// alternative is a decoder that fails on bytes that are valid Arrow.
fn split_message(message: &Bytes) -> Result<(&[u8], Range<usize>), ArrowError> {
    const CONTINUATION: u32 = 0xFFFF_FFFF;
    let word = |at: usize| -> Result<u32, ArrowError> {
        message
            .get(at..at + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| {
                ArrowError::IpcError(format!(
                    "an Arrow IPC message of {} bytes, which is too short to be one",
                    message.len()
                ))
            })
    };

    let (header_at, metadata_len) = match word(0)? {
        CONTINUATION => (8usize, word(4)? as usize),
        length => (4usize, length as usize),
    };
    let body_at = header_at + metadata_len;
    if body_at > message.len() {
        return Err(ArrowError::IpcError(format!(
            "this Arrow IPC message says its metadata is {metadata_len} bytes and it is \
             {} bytes long in total",
            message.len()
        )));
    }
    Ok((&message[header_at..body_at], body_at..message.len()))
}

/// The schema a session describes.
pub(crate) fn read_schema(serialized: &Bytes) -> Result<SchemaRef, BigQueryError> {
    let (header, _) = split_message(serialized)?;
    let message = arrow::ipc::root_as_message(header)
        .map_err(|e| ArrowError::IpcError(format!("undecodable Arrow IPC schema: {e}")))?;
    let ipc = message
        .header_as_schema()
        .ok_or_else(|| ArrowError::IpcError("a schema message that is not one".to_string()))?;
    Ok(Arc::new(arrow::ipc::convert::fb_to_schema(ipc)))
}

/// One `serialized_record_batch` as a batch, and where in memory it came from.
///
/// **This is the function the driver's central claim is about, and it is a free
/// function so that it can be checked without a server.** `message` is an owned
/// `Bytes`; the body is taken as a slice of it, which shares rather than copies,
/// and `Buffer::from` takes that slice whole. So every buffer of the returned
/// batch points inside `message` — except the fixed-width ones when the body did
/// not land on an 8-byte boundary, where `read_record_batch` reallocates to
/// align them, exactly as it does in the Flight SQL driver.
///
/// The returned range is the address range of the body the batch was decoded
/// from, which is what turns "no copy" from a claim into something a caller can
/// look at. `tests` below does exactly that with bytes Arrow's own IPC writer
/// produced; a real server would be checked the same way through
/// `Rows::wire_body`.
pub(crate) fn decode_batch(
    schema: &SchemaRef,
    dictionaries: &mut HashMap<i64, ArrayRef>,
    message: Bytes,
) -> Result<Option<(RecordBatch, Range<usize>)>, BigQueryError> {
    use arrow::ipc::MessageHeader;

    let (header, body_range) = split_message(&message)?;
    let ipc = arrow::ipc::root_as_message(header)
        .map_err(|e| ArrowError::IpcError(format!("undecodable Arrow IPC message: {e}")))?;
    let body = message.slice(body_range);
    let at = body.as_ptr() as usize;
    let range = at..at + body.len();

    match ipc.header_type() {
        MessageHeader::RecordBatch => {
            let batch = ipc.header_as_record_batch().ok_or_else(|| {
                ArrowError::IpcError("a record batch that is not one".to_string())
            })?;
            let decoded = arrow::ipc::reader::read_record_batch(
                &Buffer::from(body),
                batch,
                Arc::clone(schema),
                dictionaries,
                None,
                &ipc.version(),
            )?;
            Ok(Some((decoded, range)))
        }
        MessageHeader::DictionaryBatch => {
            // Not something the Storage Read API is documented to send, and
            // handled rather than refused: a dictionary message is how Arrow
            // encodes a low-cardinality column, and refusing one would turn a
            // service that started sending them into a driver that reads nothing.
            let dictionary = ipc.header_as_dictionary_batch().ok_or_else(|| {
                ArrowError::IpcError("a dictionary batch that is not one".to_string())
            })?;
            arrow::ipc::reader::read_dictionary(
                &Buffer::from(body),
                dictionary,
                schema,
                dictionaries,
                &ipc.version(),
            )?;
            Ok(None)
        }
        MessageHeader::Schema => Ok(None),
        other => Err(BigQueryError::Arrow(ArrowError::IpcError(format!(
            "unexpected Arrow IPC message in a read stream: {}",
            other.variant_name().unwrap_or("unknown")
        )))),
    }
}

/// An empty schema, for a statement whose result has no columns.
pub(crate) fn empty_schema() -> SchemaRef {
    Arc::new(Schema::empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field};

    /// A batch encoded by Arrow's own IPC writer, as one encapsulated message.
    ///
    /// **No server produced these bytes and this test does not pretend one did.**
    /// What it establishes is a property of `decode_batch`: given an Arrow IPC
    /// message in a `Bytes`, does the batch that comes out point into it. The
    /// wire shape is Arrow's, written by Arrow, which is the only part of the
    /// path that can be checked here — whether BigQuery sends exactly this is
    /// the part that needs a project.
    fn encoded() -> (SchemaRef, Bytes) {
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("label", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(Int64Array::from((0..512).collect::<Vec<i64>>())),
                Arc::new(StringArray::from(
                    (0..512)
                        .map(|n| format!("row-{n}"))
                        .collect::<Vec<String>>(),
                )),
            ],
        )
        .expect("a batch");

        // `StreamWriter` writes a schema message, then the batch, then an
        // end-of-stream marker. What `serialized_record_batch` carries is the
        // middle one on its own, so the other two are cut away here — a test
        // that fed the whole stream in would be checking something the field
        // never holds.
        let mut buffer = Vec::new();
        {
            let mut writer =
                arrow::ipc::writer::StreamWriter::try_new(&mut buffer, &schema).expect("a writer");
            writer.write(&batch).expect("a written batch");
            writer.finish().expect("a finished stream");
        }
        let whole = Bytes::from(buffer);
        // A schema message has no body, so the batch begins where the schema's
        // body would have.
        let (_, after_schema) = split_message(&whole).expect("the schema message");
        let rest = whole.slice(after_schema.start..);
        let (header, body) = split_message(&rest).expect("the batch message");
        let length = arrow::ipc::root_as_message(header)
            .expect("a message")
            .bodyLength() as usize;
        (schema, rest.slice(..body.start + length))
    }

    /// The claim this driver exists to make, checked over the one step that can
    /// be checked without a project: a batch decoded out of a message is the
    /// message's own bytes.
    ///
    /// The two halves are the same ones the Flight SQL driver's integration test
    /// makes against a live server. A body that landed 8-byte aligned must have
    /// every buffer inside the message; one that did not may have its
    /// fixed-width buffers realigned by Arrow, but never its characters — those
    /// need one-byte alignment, and nothing about where a message landed can
    /// justify moving them. A decoder that copied would fail both.
    #[test]
    fn a_batch_is_decoded_out_of_the_bytes_it_arrived_in() {
        let (schema, message) = encoded();
        let mut dictionaries = HashMap::new();
        let (batch, body) = decode_batch(&schema, &mut dictionaries, message.clone())
            .expect("a decode")
            .expect("a record batch");
        assert_eq!(batch.num_rows(), 512);

        let aligned = body.start % 8 == 0;
        for (column, field) in batch.columns().iter().zip(batch.schema().fields()) {
            let data = column.to_data();
            for (at, buffer) in data.buffers().iter().enumerate() {
                let inside = body.contains(&(buffer.as_ptr() as usize));
                let variable_width =
                    matches!(field.data_type(), DataType::Utf8) && at + 1 == data.buffers().len();
                assert!(
                    inside || (!aligned && !variable_width),
                    "{}'s buffer {at} was copied out of the message it arrived in",
                    field.name()
                );
            }
        }
    }

    /// Most of the batch's weight reaches the caller untouched, counted in
    /// bytes rather than in buffers — a `Utf8` column's characters are most of
    /// its weight and its offsets a twentieth of it, and one vote each would say
    /// the opposite of what happened.
    #[test]
    fn most_of_a_batch_is_never_moved() {
        let (schema, message) = encoded();
        let mut dictionaries = HashMap::new();
        let (batch, body) = decode_batch(&schema, &mut dictionaries, message)
            .expect("a decode")
            .expect("a record batch");

        let (mut in_place, mut moved) = (0usize, 0usize);
        for column in batch.columns() {
            for buffer in column.to_data().buffers() {
                if body.contains(&(buffer.as_ptr() as usize)) {
                    in_place += buffer.len();
                } else {
                    moved += buffer.len();
                }
            }
        }
        assert!(
            in_place > moved,
            "{in_place} bytes reached the caller in place and {moved} were copied"
        );
    }

    /// Both IPC framings, because a decoder that read only the current one would
    /// fail on bytes that are valid Arrow.
    #[test]
    fn an_ipc_message_is_split_whichever_framing_it_uses() {
        let mut with_marker = vec![0xff, 0xff, 0xff, 0xff, 4, 0, 0, 0];
        with_marker.extend_from_slice(b"METAbody");
        let bytes = Bytes::from(with_marker);
        let (header, body) = split_message(&bytes).expect("a split");
        assert_eq!(header, b"META");
        assert_eq!(&bytes[body], b"body");

        let mut legacy = vec![4, 0, 0, 0];
        legacy.extend_from_slice(b"METAbody");
        let bytes = Bytes::from(legacy);
        let (header, body) = split_message(&bytes).expect("a split");
        assert_eq!(header, b"META");
        assert_eq!(&bytes[body], b"body");
    }

    /// A message that is not one is refused rather than read past its end.
    #[test]
    fn a_message_that_cannot_be_one_is_refused() {
        assert!(split_message(&Bytes::from_static(b"\xff\xff")).is_err());
        // A metadata length longer than the message that carries it.
        let claim = Bytes::from_static(b"\xff\xff\xff\xff\xff\x00\x00\x00short");
        assert!(split_message(&claim).is_err());
    }

    /// The one number in this file that a wrong value would make silently
    /// useless: asking for Avro and decoding Arrow.
    #[test]
    fn the_format_asked_for_is_arrow_and_not_avro() {
        assert_eq!(FORMAT_ARROW, 2);
    }

    /// The request that goes out, checked through prost's own round trip. This
    /// says nothing about whether the tags match Google's — nothing here can —
    /// but it does say that the message this driver builds carries the three
    /// things it means to ask for.
    #[test]
    fn the_session_asks_for_arrow_with_no_compression_on_one_stream() {
        let request = CreateReadSessionRequest {
            parent: "projects/example".to_string(),
            read_session: Some(ReadSession {
                data_format: FORMAT_ARROW,
                table: "projects/example/datasets/d/tables/t".to_string(),
                read_options: Some(TableReadOptions {
                    arrow_serialization_options: Some(ArrowSerializationOptions::default()),
                }),
                ..ReadSession::default()
            }),
            max_stream_count: 1,
        };
        let encoded = request.encode_to_vec();
        let read_back = CreateReadSessionRequest::decode(&encoded[..]).expect("a request");
        let session = read_back.read_session.expect("a session");
        assert_eq!(session.data_format, FORMAT_ARROW);
        assert_eq!(read_back.max_stream_count, 1);
        // Present and empty: the options message is sent, and the compression
        // field inside it is left at the default, which is none.
        let options = session
            .read_options
            .expect("read options")
            .arrow_serialization_options
            .expect("arrow serialization options");
        assert_eq!(options.buffer_compression, 0);
    }
}
