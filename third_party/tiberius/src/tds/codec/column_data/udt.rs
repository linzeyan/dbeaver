use std::borrow::Cow;

use crate::{sql_read_bytes::SqlReadBytes, ColumnData};

/// Any length at or above this makes `plp::decode` take its chunked path. The
/// number is never used as a size, only as that selector.
const ALWAYS_PLP: usize = 0xffff;

/// The body of a CLR user-defined type value: `geometry`, `geography`,
/// `hierarchyid`, or a type somebody registered themselves.
///
/// Handed over as bytes rather than as anything more specific, and that is a
/// deliberate division of labour. What the bytes mean is decided by the CLR type
/// named in `COLMETADATA`, which the caller can read from
/// `Column::udt_type_name`; turning a geography into text is a rendering
/// decision, and rendering decisions do not belong in a wire codec. What does
/// belong here is reading the right number of bytes, which is what the `todo!()`
/// this replaced never did.
///
/// UDT bodies are always partially length-prefixed (MS-TDS 2.2.5.2.3) whatever
/// `MAX_BYTE_SIZE` in the column metadata says, so the chunked path is taken
/// unconditionally instead of being chosen by a declared length the way
/// `varbinary` chooses it.
pub(crate) async fn decode<R>(src: &mut R) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    let data = super::plp::decode(src, ALWAYS_PLP).await?.map(Cow::from);

    Ok(ColumnData::Binary(data))
}
