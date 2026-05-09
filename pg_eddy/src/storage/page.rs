#![allow(dead_code)]

/// Page-format constants and layout types for pg_eddy custom AM pages.
///
/// Node pages use standard PostgreSQL page infrastructure:
///   • Standard `PageHeaderData` at offset 0 (managed by `PageInit`)
///   • Item pointer array (pd_lower → pd_upper region, grows downward)
///   • Node records in item slots (each record starts with `HeapTupleHeaderData`)
///   • `pd_special` area at page end: array of `NodeAdjHeader` structs (Region 1)
///
/// The `pd_special` area is sized for `MAX_NODE_SLOTS_PER_PAGE` adjacency headers.
/// Each header maps 1:1 to an item slot (adj_slot_idx in the node record).

// ---------------------------------------------------------------------------
// WAL record info bytes (high nibble = 0x0_ for node ops, 0x1_ for edge ops)
// ---------------------------------------------------------------------------
pub const XLOG_PG_EDDY_NODE_INSERT: u8 = 0x00;
pub const XLOG_PG_EDDY_NODE_UPDATE_PROPS: u8 = 0x01;
pub const XLOG_PG_EDDY_NODE_DELETE: u8 = 0x02;
pub const XLOG_PG_EDDY_EDGE_INSERT: u8 = 0x10; // Phase 2
pub const XLOG_PG_EDDY_EDGE_DELETE: u8 = 0x11; // Phase 2
pub const XLOG_PG_EDDY_ADJ_UPDATE: u8 = 0x20; // Phase 2

// ---------------------------------------------------------------------------
// Page geometry
// ---------------------------------------------------------------------------

/// Maximum node item slots per node page.
/// Sized so that pd_special = MAX_NODE_SLOTS_PER_PAGE × ADJ_HEADER_BYTES fits
/// while still leaving room for a reasonable number of node records.
///
/// With BLCKSZ=8192, PageHeader=24, each item needs ItemId(4B) + ≥42B data:
///   8192 = 24 + 4·N + 42·N + 24·N  →  N ≈ 116
/// We use 100 as a conservative round number.
pub const MAX_NODE_SLOTS_PER_PAGE: usize = 100;

/// Bytes per adjacency header entry (24 B, layout described below).
pub const ADJ_HEADER_BYTES: usize = 24;

/// Size of the pd_special area for node pages (Region 1).
pub const PD_NODE_SPECIAL_SIZE: usize = MAX_NODE_SLOTS_PER_PAGE * ADJ_HEADER_BYTES;

/// Maximum bytes of property data stored inline inside the node record.
/// Properties exceeding this limit spill to overflow pages (Phase 1: error).
pub const PROP_INLINE_MAX: usize = 48;

/// Maximum label IDs per node.
pub const MAX_LABELS_PER_NODE: usize = 32;

/// Fixed portion of a node record after `HeapTupleHeaderData`:
///   node_id (8) + adj_slot_idx (2) + label_count (1) + prop_inline_len (2)
///   + prop_overflow_page (4) + _pad (1)  = 18 bytes
pub const NODE_FIXED_DATA_SIZE: usize = 18;

// ---------------------------------------------------------------------------
// NodeAdjHeader — Region 1 entry (24 bytes, byte-array backed to avoid
// alignment padding issues with #[repr(C)] and mixed u32/u16 fields)
// ---------------------------------------------------------------------------

/// Adjacency header stored in the pd_special area of node pages.
///
/// Layout (little-endian, no padding):
///   [0..4]   out_head_pg      — page number of first outgoing edge chain head
///   [4..6]   out_head_sl      — slot index within that page
///   [6..10]  in_head_pg       — page number of first incoming edge chain head
///   [10..12] in_head_sl       — slot index within that page
///   [12..16] out_degree       — approximate outgoing edge count
///   [16..20] in_degree        — approximate incoming edge count
///   [20..24] graph_partition_id — reserved for future Citus distribution
///
/// Updated **in-place under exclusive buffer lock** (not MVCC-versioned).
/// Phase 1: all fields are zero; populated in Phase 2 when edges are added.
#[derive(Clone, Copy, Default)]
pub struct NodeAdjHeader([u8; ADJ_HEADER_BYTES]);

impl NodeAdjHeader {
    #[inline] pub fn out_head_pg(&self) -> u32 { u32::from_le_bytes(self.0[0..4].try_into().unwrap()) }
    #[inline] pub fn out_head_sl(&self) -> u16 { u16::from_le_bytes(self.0[4..6].try_into().unwrap()) }
    #[inline] pub fn in_head_pg(&self) -> u32  { u32::from_le_bytes(self.0[6..10].try_into().unwrap()) }
    #[inline] pub fn in_head_sl(&self) -> u16  { u16::from_le_bytes(self.0[10..12].try_into().unwrap()) }
    #[inline] pub fn out_degree(&self) -> u32  { u32::from_le_bytes(self.0[12..16].try_into().unwrap()) }
    #[inline] pub fn in_degree(&self) -> u32   { u32::from_le_bytes(self.0[16..20].try_into().unwrap()) }
    #[inline] pub fn graph_partition_id(&self) -> u32 { u32::from_le_bytes(self.0[20..24].try_into().unwrap()) }

    #[inline] pub fn set_out_head_pg(&mut self, v: u32) { self.0[0..4].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_out_head_sl(&mut self, v: u16) { self.0[4..6].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_in_head_pg(&mut self, v: u32)  { self.0[6..10].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_in_head_sl(&mut self, v: u16)  { self.0[10..12].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_out_degree(&mut self, v: u32)  { self.0[12..16].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_in_degree(&mut self, v: u32)   { self.0[16..20].copy_from_slice(&v.to_le_bytes()); }
    #[inline] pub fn set_graph_partition_id(&mut self, v: u32) { self.0[20..24].copy_from_slice(&v.to_le_bytes()); }

    #[inline] pub fn as_bytes(&self) -> &[u8; ADJ_HEADER_BYTES] { &self.0 }
    #[inline] pub fn from_bytes(b: &[u8; ADJ_HEADER_BYTES]) -> Self { NodeAdjHeader(*b) }
}

// ---------------------------------------------------------------------------
// Node record wire layout helpers
//
// A node record in a page item slot has this layout:
//   [0..SizeofHeapTupleHeader]  HeapTupleHeaderData  (MVCC + self TID)
//   [SizeofHeapTupleHeader..]   node_id:       i64  (8 bytes, LE)
//                               adj_slot_idx:  u16  (2 bytes, LE)
//                               label_count:   u8   (1 byte)
//                               prop_inline_len: u16 (2 bytes, LE)
//                               prop_overflow_page: u32 (4 bytes, LE, 0=none)
//                               _pad:          u8   (1 byte — align to even)
//                               label_ids[]:   i32  × label_count (4 bytes each, LE)
//                               prop_data[]:   u8   × prop_inline_len
//
// Total minimum (0 labels, 0 props) = SizeofHeapTupleHeader + 18 bytes.
// ---------------------------------------------------------------------------

/// Offset of node_id within the data portion (after HeapTupleHeader).
pub const OFF_NODE_ID: usize = 0;
/// Offset of adj_slot_idx (u16).
pub const OFF_ADJ_SLOT: usize = 8;
/// Offset of label_count (u8).
pub const OFF_LABEL_COUNT: usize = 10;
/// Offset of prop_inline_len (u16).
pub const OFF_PROP_INLINE_LEN: usize = 11;
/// Offset of prop_overflow_page (u32).
pub const OFF_PROP_OVERFLOW_PAGE: usize = 13;
/// Offset of label_ids array (i32 × label_count).
pub const OFF_LABEL_IDS: usize = 18; // 8+2+1+2+4+1 (pad)

/// Compute total item size for a node with `nlabels` labels and `nprop` bytes of properties.
#[inline]
pub fn node_item_size(nlabels: usize, nprop: usize) -> usize {
    // HeapTupleHeader size from pgrx is available as SizeofHeapTupleHeader
    // but we hard-code the PG18 value here (24 bytes) for compile-time use.
    // The pg_sys::SizeofHeapTupleHeader constant is checked at test time.
    const HTUP_HEADER: usize = 24;
    HTUP_HEADER + NODE_FIXED_DATA_SIZE + nlabels * 4 + nprop
}

/// Read a little-endian i64 from a byte slice at offset `off`.
#[inline]
pub fn read_i64(buf: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

/// Read a little-endian u16 from a byte slice at offset `off`.
#[inline]
pub fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

/// Read a little-endian u32 from a byte slice at offset `off`.
#[inline]
pub fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Read a little-endian i32 from a byte slice at offset `off`.
#[inline]
pub fn read_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
