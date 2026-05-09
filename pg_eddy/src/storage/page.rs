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
// WAL record info bytes.
//
// PostgreSQL only allows bits 0-1 in the low nibble of xl_info when calling
// XLogInsert (bit 0 = XLR_SPECIAL_REL_UPDATE, bit 1 = XLR_CHECK_CONSISTENCY).
// Bits 2-3 are reserved; setting them causes a PANIC ("invalid xlog info mask").
// The RMGR opcode MUST therefore live entirely in the HIGH nibble (bits 4-7).
//
// The redo dispatcher strips the low nibble with `& !XLR_INFO_MASK` before
// matching, so each record type needs a unique HIGH-nibble value.
// ---------------------------------------------------------------------------
pub const XLOG_PG_EDDY_NODE_INSERT: u8     = 0x00; // high nibble 0
pub const XLOG_PG_EDDY_NODE_INSERT_OVF: u8 = 0x10; // high nibble 1 (node + overflow block)
pub const XLOG_PG_EDDY_NODE_DELETE: u8     = 0x20; // high nibble 2
pub const XLOG_PG_EDDY_NODE_COMPACT: u8    = 0x30; // high nibble 3 (FPI after PageRepairFragmentation)
pub const XLOG_PG_EDDY_EDGE_INSERT: u8     = 0x40; // high nibble 4
pub const XLOG_PG_EDDY_EDGE_DELETE: u8     = 0x50; // high nibble 5
pub const XLOG_PG_EDDY_ADJ_UPDATE: u8      = 0x60; // high nibble 6
pub const XLOG_PG_EDDY_VACUUM_PAGE: u8     = 0x70; // high nibble 7

/// The pd_special offset for node pages (= BLCKSZ − PD_NODE_SPECIAL_SIZE).
/// Overflow pages (no special area) have pd_special = BLCKSZ (8192).
pub const PD_NODE_SPECIAL_OFFSET: usize = 8192 - PD_NODE_SPECIAL_SIZE; // 5792

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
// Edge record wire layout constants
//
// An edge record in an edge page item slot has this layout (after HeapTupleHeaderData):
//   rel_id           (8B, i64 LE)
//   rel_type_id      (4B, i32 LE)
//   source_node_id   (8B, i64 LE)
//   target_node_id   (8B, i64 LE)
//   next_out_page    (4B, u32 LE)   — page of next outgoing edge (0=none if next_out_slot=0)
//   next_out_slot    (2B, u16 LE)   — slot of next outgoing edge (0 = end of chain)
//   next_in_page     (4B, u32 LE)
//   next_in_slot     (2B, u16 LE)
//   prop_inline_len  (2B, u16 LE)
//   prop_overflow_page (4B, u32 LE)
//   prop_data        (up to PROP_INLINE_MAX bytes)
//
// CHAIN SENTINEL: a head pointer with slot == 0 means "no edges".
//   The initial adjacency header has all zeros, so out_head_sl == 0 → empty.
// ---------------------------------------------------------------------------

/// Offset of rel_id within edge record data portion (after HeapTupleHeader).
pub const OFF_EDGE_REL_ID: usize = 0;
/// Offset of rel_type_id (i32).
pub const OFF_EDGE_REL_TYPE_ID: usize = 8;
/// Offset of source_node_id (i64).
pub const OFF_EDGE_SOURCE_NODE_ID: usize = 12;
/// Offset of target_node_id (i64).
pub const OFF_EDGE_TARGET_NODE_ID: usize = 20;
/// Offset of next_out_page (u32).
pub const OFF_EDGE_NEXT_OUT_PAGE: usize = 28;
/// Offset of next_out_slot (u16).
pub const OFF_EDGE_NEXT_OUT_SLOT: usize = 32;
/// Offset of next_in_page (u32).
pub const OFF_EDGE_NEXT_IN_PAGE: usize = 34;
/// Offset of next_in_slot (u16).
pub const OFF_EDGE_NEXT_IN_SLOT: usize = 38;
/// Offset of prop_inline_len (u16).
pub const OFF_EDGE_PROP_INLINE_LEN: usize = 40;
/// Offset of prop_overflow_page (u32).
pub const OFF_EDGE_PROP_OVERFLOW_PAGE: usize = 42;
/// Offset of inline property data.
pub const OFF_EDGE_PROP_DATA: usize = 46;
/// Fixed data size (without inline props).
pub const EDGE_FIXED_DATA_SIZE: usize = 46;

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
