//! Versioned semantic CDC protocol shared by pg_eddy's logical-message
//! producer and output plugin.

use thiserror::Error;

pub const PROTOCOL_MAGIC: [u8; 4] = *b"PEDY";
pub const PROTOCOL_MAJOR: u16 = 1;
pub const LOGICAL_MESSAGE_PREFIX: &str = "pg_eddy/v1";

const HEADER_LEN: usize = 12;
const PLACEHOLDER_EVENT_LSN: u64 = 0;
const MAX_FIELD_BYTES: usize = 1024 * 1024;
const MAX_FRAME_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_LABELS: usize = 4096;

const KIND_BEGIN: u8 = 0x01;
const KIND_COMMIT: u8 = 0x02;
const KIND_NODE_INSERT: u8 = 0x10;
const KIND_NODE_UPDATE: u8 = 0x11;
const KIND_NODE_DELETE: u8 = 0x12;
const KIND_EDGE_INSERT: u8 = 0x20;
const KIND_EDGE_UPDATE: u8 = 0x21;
const KIND_EDGE_DELETE: u8 = 0x22;
const KIND_GRAPH_RESET: u8 = 0x30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub node_id: i64,
    pub labels: Vec<String>,
    pub properties: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub rel_id: i64,
    pub rel_type: String,
    pub source_node_id: i64,
    pub target_node_id: i64,
    pub properties: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    NodeInsert { new: NodeRow },
    NodeUpdate { old: NodeRow, new: NodeRow },
    NodeDelete { old: NodeRow },
    EdgeInsert { new: EdgeRow },
    EdgeUpdate { old: EdgeRow, new: EdgeRow },
    EdgeDelete { old: EdgeRow },
    GraphReset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Begin {
        xid: u32,
    },
    Commit {
        xid: u32,
        commit_lsn: u64,
        end_lsn: u64,
    },
    Mutation {
        event_lsn: u64,
        mutation: Mutation,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtocolError {
    #[error("CDC frame is shorter than the {HEADER_LEN}-byte header")]
    HeaderTruncated,

    #[error("invalid CDC frame magic")]
    InvalidMagic,

    #[error("unsupported CDC protocol major version {0}")]
    UnsupportedMajor(u16),

    #[error("unsupported CDC frame flags 0x{0:02x}")]
    UnsupportedFlags(u8),

    #[error("unknown CDC frame kind 0x{0:02x}")]
    UnknownKind(u8),

    #[error("CDC payload length mismatch: declared {declared}, actual {actual}")]
    PayloadLengthMismatch { declared: usize, actual: usize },

    #[error("CDC payload is truncated at byte {offset}: need {needed} bytes, have {remaining}")]
    PayloadTruncated {
        offset: usize,
        needed: usize,
        remaining: usize,
    },

    #[error("CDC payload has {0} trailing bytes")]
    TrailingPayload(usize),

    #[error("CDC {field} is too large: {length} bytes/items, maximum {maximum}")]
    FieldTooLarge {
        field: &'static str,
        length: usize,
        maximum: usize,
    },

    #[error("CDC {field} is not valid UTF-8")]
    InvalidUtf8 { field: &'static str },

    #[error("CDC properties are not valid JSON: {0}")]
    InvalidJson(String),

    #[error("CDC properties must be a JSON object")]
    PropertiesNotObject,

    #[error("logical-message envelope must contain one mutation with event LSN zero")]
    InvalidLogicalMessageEnvelope,
}

pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    let mut payload = Vec::new();
    let kind = match frame {
        Frame::Begin { xid } => {
            write_u32(&mut payload, *xid);
            KIND_BEGIN
        }
        Frame::Commit {
            xid,
            commit_lsn,
            end_lsn,
        } => {
            write_u32(&mut payload, *xid);
            write_u64(&mut payload, *commit_lsn);
            write_u64(&mut payload, *end_lsn);
            KIND_COMMIT
        }
        Frame::Mutation {
            event_lsn,
            mutation,
        } => {
            write_u64(&mut payload, *event_lsn);
            encode_mutation_payload(&mut payload, mutation)?;
            mutation_kind(mutation)
        }
    };

    if payload.len() > MAX_FRAME_PAYLOAD_BYTES {
        return Err(ProtocolError::FieldTooLarge {
            field: "frame payload",
            length: payload.len(),
            maximum: MAX_FRAME_PAYLOAD_BYTES,
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| ProtocolError::FieldTooLarge {
        field: "frame payload",
        length: payload.len(),
        maximum: u32::MAX as usize,
    })?;

    let mut encoded = Vec::with_capacity(HEADER_LEN + payload.len());
    encoded.extend_from_slice(&PROTOCOL_MAGIC);
    encoded.extend_from_slice(&PROTOCOL_MAJOR.to_be_bytes());
    encoded.push(kind);
    encoded.push(0);
    encoded.extend_from_slice(&payload_len.to_be_bytes());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

pub fn decode_frame(encoded: &[u8]) -> Result<Frame, ProtocolError> {
    if encoded.len() < HEADER_LEN {
        return Err(ProtocolError::HeaderTruncated);
    }
    if encoded[..4] != PROTOCOL_MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }

    let major = u16::from_be_bytes([encoded[4], encoded[5]]);
    if major != PROTOCOL_MAJOR {
        return Err(ProtocolError::UnsupportedMajor(major));
    }
    let kind = encoded[6];
    let flags = encoded[7];
    if flags != 0 {
        return Err(ProtocolError::UnsupportedFlags(flags));
    }

    let declared = u32::from_be_bytes([encoded[8], encoded[9], encoded[10], encoded[11]]) as usize;
    let actual = encoded.len() - HEADER_LEN;
    if declared != actual {
        return Err(ProtocolError::PayloadLengthMismatch { declared, actual });
    }
    if actual > MAX_FRAME_PAYLOAD_BYTES {
        return Err(ProtocolError::FieldTooLarge {
            field: "frame payload",
            length: actual,
            maximum: MAX_FRAME_PAYLOAD_BYTES,
        });
    }

    let mut reader = Reader::new(&encoded[HEADER_LEN..]);
    let frame = match kind {
        KIND_BEGIN => Frame::Begin { xid: reader.u32()? },
        KIND_COMMIT => Frame::Commit {
            xid: reader.u32()?,
            commit_lsn: reader.u64()?,
            end_lsn: reader.u64()?,
        },
        KIND_NODE_INSERT
        | KIND_NODE_UPDATE
        | KIND_NODE_DELETE
        | KIND_EDGE_INSERT
        | KIND_EDGE_UPDATE
        | KIND_EDGE_DELETE
        | KIND_GRAPH_RESET => Frame::Mutation {
            event_lsn: reader.u64()?,
            mutation: decode_mutation_payload(kind, &mut reader)?,
        },
        other => return Err(ProtocolError::UnknownKind(other)),
    };
    reader.finish()?;
    Ok(frame)
}

/// Encode one transactional logical-message payload. The zero event LSN is a
/// reserved placeholder replaced by the output plugin's message callback LSN.
pub fn encode_logical_message(mutation: &Mutation) -> Result<Vec<u8>, ProtocolError> {
    encode_frame(&Frame::Mutation {
        event_lsn: PLACEHOLDER_EVENT_LSN,
        mutation: mutation.clone(),
    })
}

pub fn decode_logical_message(encoded: &[u8]) -> Result<Mutation, ProtocolError> {
    match decode_frame(encoded)? {
        Frame::Mutation {
            event_lsn: PLACEHOLDER_EVENT_LSN,
            mutation,
        } => Ok(mutation),
        _ => Err(ProtocolError::InvalidLogicalMessageEnvelope),
    }
}

fn mutation_kind(mutation: &Mutation) -> u8 {
    match mutation {
        Mutation::NodeInsert { .. } => KIND_NODE_INSERT,
        Mutation::NodeUpdate { .. } => KIND_NODE_UPDATE,
        Mutation::NodeDelete { .. } => KIND_NODE_DELETE,
        Mutation::EdgeInsert { .. } => KIND_EDGE_INSERT,
        Mutation::EdgeUpdate { .. } => KIND_EDGE_UPDATE,
        Mutation::EdgeDelete { .. } => KIND_EDGE_DELETE,
        Mutation::GraphReset => KIND_GRAPH_RESET,
    }
}

fn encode_mutation_payload(
    payload: &mut Vec<u8>,
    mutation: &Mutation,
) -> Result<(), ProtocolError> {
    match mutation {
        Mutation::NodeInsert { new } => encode_node(payload, new),
        Mutation::NodeUpdate { old, new } => {
            encode_node(payload, old)?;
            encode_node(payload, new)
        }
        Mutation::NodeDelete { old } => encode_node(payload, old),
        Mutation::EdgeInsert { new } => encode_edge(payload, new),
        Mutation::EdgeUpdate { old, new } => {
            encode_edge(payload, old)?;
            encode_edge(payload, new)
        }
        Mutation::EdgeDelete { old } => encode_edge(payload, old),
        Mutation::GraphReset => Ok(()),
    }
}

fn decode_mutation_payload(kind: u8, reader: &mut Reader<'_>) -> Result<Mutation, ProtocolError> {
    match kind {
        KIND_NODE_INSERT => Ok(Mutation::NodeInsert {
            new: reader.node()?,
        }),
        KIND_NODE_UPDATE => Ok(Mutation::NodeUpdate {
            old: reader.node()?,
            new: reader.node()?,
        }),
        KIND_NODE_DELETE => Ok(Mutation::NodeDelete {
            old: reader.node()?,
        }),
        KIND_EDGE_INSERT => Ok(Mutation::EdgeInsert {
            new: reader.edge()?,
        }),
        KIND_EDGE_UPDATE => Ok(Mutation::EdgeUpdate {
            old: reader.edge()?,
            new: reader.edge()?,
        }),
        KIND_EDGE_DELETE => Ok(Mutation::EdgeDelete {
            old: reader.edge()?,
        }),
        KIND_GRAPH_RESET => Ok(Mutation::GraphReset),
        other => Err(ProtocolError::UnknownKind(other)),
    }
}

fn encode_node(payload: &mut Vec<u8>, row: &NodeRow) -> Result<(), ProtocolError> {
    if row.labels.len() > MAX_LABELS || row.labels.len() > u16::MAX as usize {
        return Err(ProtocolError::FieldTooLarge {
            field: "label count",
            length: row.labels.len(),
            maximum: MAX_LABELS.min(u16::MAX as usize),
        });
    }
    write_i64(payload, row.node_id);
    write_u16(payload, row.labels.len() as u16);
    for label in &row.labels {
        write_string(payload, "label", label)?;
    }
    write_properties(payload, &row.properties)
}

fn encode_edge(payload: &mut Vec<u8>, row: &EdgeRow) -> Result<(), ProtocolError> {
    write_i64(payload, row.rel_id);
    write_string(payload, "relationship type", &row.rel_type)?;
    write_i64(payload, row.source_node_id);
    write_i64(payload, row.target_node_id);
    write_properties(payload, &row.properties)
}

fn write_properties(
    payload: &mut Vec<u8>,
    properties: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ProtocolError> {
    let json = serde_json::to_vec(&serde_json::Value::Object(properties.clone()))
        .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
    write_bytes(payload, "properties", &json)
}

fn write_string(
    payload: &mut Vec<u8>,
    field: &'static str,
    value: &str,
) -> Result<(), ProtocolError> {
    write_bytes(payload, field, value.as_bytes())
}

fn write_bytes(
    payload: &mut Vec<u8>,
    field: &'static str,
    value: &[u8],
) -> Result<(), ProtocolError> {
    if value.len() > MAX_FIELD_BYTES {
        return Err(ProtocolError::FieldTooLarge {
            field,
            length: value.len(),
            maximum: MAX_FIELD_BYTES,
        });
    }
    let length = u32::try_from(value.len()).map_err(|_| ProtocolError::FieldTooLarge {
        field,
        length: value.len(),
        maximum: u32::MAX as usize,
    })?;
    write_u32(payload, length);
    payload.extend_from_slice(value);
    Ok(())
}

fn write_u16(payload: &mut Vec<u8>, value: u16) {
    payload.extend_from_slice(&value.to_be_bytes());
}

fn write_u32(payload: &mut Vec<u8>, value: u32) {
    payload.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(payload: &mut Vec<u8>, value: u64) {
    payload.extend_from_slice(&value.to_be_bytes());
}

fn write_i64(payload: &mut Vec<u8>, value: i64) {
    payload.extend_from_slice(&value.to_be_bytes());
}

struct Reader<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn take(&mut self, needed: usize) -> Result<&'a [u8], ProtocolError> {
        let remaining = self.payload.len().saturating_sub(self.offset);
        if remaining < needed {
            return Err(ProtocolError::PayloadTruncated {
                offset: self.offset,
                needed,
                remaining,
            });
        }
        let bytes = &self.payload[self.offset..self.offset + needed];
        self.offset += needed;
        Ok(bytes)
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, ProtocolError> {
        let bytes = self.take(8)?;
        Ok(i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8], ProtocolError> {
        let length = self.u32()? as usize;
        if length > MAX_FIELD_BYTES {
            return Err(ProtocolError::FieldTooLarge {
                field,
                length,
                maximum: MAX_FIELD_BYTES,
            });
        }
        self.take(length)
    }

    fn string(&mut self, field: &'static str) -> Result<String, ProtocolError> {
        let bytes = self.bytes(field)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ProtocolError::InvalidUtf8 { field })
    }

    fn properties(
        &mut self,
    ) -> Result<serde_json::Map<String, serde_json::Value>, ProtocolError> {
        let bytes = self.bytes("properties")?;
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| ProtocolError::InvalidJson(error.to_string()))?;
        value.as_object().cloned().ok_or(ProtocolError::PropertiesNotObject)
    }

    fn node(&mut self) -> Result<NodeRow, ProtocolError> {
        let node_id = self.i64()?;
        let label_count = self.u16()? as usize;
        if label_count > MAX_LABELS {
            return Err(ProtocolError::FieldTooLarge {
                field: "label count",
                length: label_count,
                maximum: MAX_LABELS,
            });
        }
        let mut labels = Vec::with_capacity(label_count);
        for _ in 0..label_count {
            labels.push(self.string("label")?);
        }
        Ok(NodeRow {
            node_id,
            labels,
            properties: self.properties()?,
        })
    }

    fn edge(&mut self) -> Result<EdgeRow, ProtocolError> {
        Ok(EdgeRow {
            rel_id: self.i64()?,
            rel_type: self.string("relationship type")?,
            source_node_id: self.i64()?,
            target_node_id: self.i64()?,
            properties: self.properties()?,
        })
    }

    fn finish(self) -> Result<(), ProtocolError> {
        let trailing = self.payload.len() - self.offset;
        if trailing == 0 {
            Ok(())
        } else {
            Err(ProtocolError::TrailingPayload(trailing))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties() -> serde_json::Map<String, serde_json::Value> {
        serde_json::json!({
            "active": true,
            "name": "O'Brien",
            "unicode": "\u{03bb}"
        })
        .as_object()
        .expect("object")
        .clone()
    }

    fn node(node_id: i64) -> NodeRow {
        NodeRow {
            node_id,
            labels: vec!["Person".into(), "Employee".into()],
            properties: properties(),
        }
    }

    fn edge(rel_id: i64) -> EdgeRow {
        EdgeRow {
            rel_id,
            rel_type: "KNOWS".into(),
            source_node_id: 11,
            target_node_id: 12,
            properties: properties(),
        }
    }

    fn mutations() -> Vec<Mutation> {
        vec![
            Mutation::NodeInsert { new: node(1) },
            Mutation::NodeUpdate {
                old: node(1),
                new: node(2),
            },
            Mutation::NodeDelete { old: node(2) },
            Mutation::EdgeInsert { new: edge(10) },
            Mutation::EdgeUpdate {
                old: edge(10),
                new: edge(11),
            },
            Mutation::EdgeDelete { old: edge(11) },
            Mutation::GraphReset,
        ]
    }

    #[test]
    fn round_trips_transaction_frames() {
        let frames = [
            Frame::Begin { xid: 42 },
            Frame::Commit {
                xid: 42,
                commit_lsn: 0x0102_0304_0506_0708,
                end_lsn: 0x1112_1314_1516_1718,
            },
        ];
        for frame in frames {
            let encoded = encode_frame(&frame).expect("encode");
            assert_eq!(decode_frame(&encoded).expect("decode"), frame);
        }
    }

    #[test]
    fn round_trips_every_mutation_frame() {
        for mutation in mutations() {
            let frame = Frame::Mutation {
                event_lsn: 0xAABB_CCDD_EEFF_0011,
                mutation,
            };
            let encoded = encode_frame(&frame).expect("encode");
            assert_eq!(decode_frame(&encoded).expect("decode"), frame);
        }
    }

    #[test]
    fn round_trips_logical_message_envelopes() {
        for mutation in mutations() {
            let encoded = encode_logical_message(&mutation).expect("encode logical message");
            assert_eq!(
                decode_logical_message(&encoded).expect("decode logical message"),
                mutation
            );
        }
    }

    #[test]
    fn rejects_invalid_header_fields() {
        let valid = encode_frame(&Frame::Begin { xid: 7 }).expect("encode");

        assert_eq!(decode_frame(&valid[..8]), Err(ProtocolError::HeaderTruncated));

        let mut bad_magic = valid.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode_frame(&bad_magic), Err(ProtocolError::InvalidMagic));

        let mut bad_version = valid.clone();
        bad_version[4..6].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(decode_frame(&bad_version), Err(ProtocolError::UnsupportedMajor(2)));

        let mut bad_flags = valid.clone();
        bad_flags[7] = 1;
        assert_eq!(decode_frame(&bad_flags), Err(ProtocolError::UnsupportedFlags(1)));

        let mut bad_kind = valid;
        bad_kind[6] = 0xFF;
        assert_eq!(decode_frame(&bad_kind), Err(ProtocolError::UnknownKind(0xFF)));
    }

    #[test]
    fn rejects_length_mismatch_truncation_and_trailing_payload() {
        let valid = encode_frame(&Frame::Mutation {
            event_lsn: 9,
            mutation: Mutation::GraphReset,
        })
        .expect("encode");

        let mut truncated = valid.clone();
        truncated.pop();
        assert_eq!(
            decode_frame(&truncated),
            Err(ProtocolError::PayloadLengthMismatch {
                declared: 8,
                actual: 7,
            })
        );

        let mut trailing = valid;
        trailing.push(0);
        trailing[8..12].copy_from_slice(&9_u32.to_be_bytes());
        assert_eq!(decode_frame(&trailing), Err(ProtocolError::TrailingPayload(1)));
    }

    #[test]
    fn rejects_invalid_utf8_and_non_object_properties() {
        let mut invalid_utf8 = encode_frame(&Frame::Mutation {
            event_lsn: 1,
            mutation: Mutation::NodeInsert { new: node(1) },
        })
        .expect("encode");
        let first_label_byte = HEADER_LEN + 8 + 8 + 2 + 4;
        invalid_utf8[first_label_byte] = 0xFF;
        assert_eq!(
            decode_frame(&invalid_utf8),
            Err(ProtocolError::InvalidUtf8 { field: "label" })
        );

        let mut payload = Vec::new();
        write_u64(&mut payload, 1);
        write_i64(&mut payload, 1);
        write_u16(&mut payload, 0);
        write_bytes(&mut payload, "properties", b"[]").expect("properties bytes");
        let mut non_object = Vec::new();
        non_object.extend_from_slice(&PROTOCOL_MAGIC);
        non_object.extend_from_slice(&PROTOCOL_MAJOR.to_be_bytes());
        non_object.push(KIND_NODE_INSERT);
        non_object.push(0);
        non_object.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        non_object.extend_from_slice(&payload);
        assert_eq!(decode_frame(&non_object), Err(ProtocolError::PropertiesNotObject));
    }

    #[test]
    fn rejects_oversized_fields_and_non_mutation_logical_envelopes() {
        let oversized = NodeRow {
            node_id: 1,
            labels: vec!["Label".into(); MAX_LABELS + 1],
            properties: properties(),
        };
        assert!(matches!(
            encode_frame(&Frame::Mutation {
                event_lsn: 1,
                mutation: Mutation::NodeInsert { new: oversized },
            }),
            Err(ProtocolError::FieldTooLarge {
                field: "label count",
                ..
            })
        ));

        let begin = encode_frame(&Frame::Begin { xid: 1 }).expect("encode begin");
        assert_eq!(
            decode_logical_message(&begin),
            Err(ProtocolError::InvalidLogicalMessageEnvelope)
        );
    }
}