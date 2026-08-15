//! `sql_variant`, which states the type of every value in front of the value.
//!
//! A `sql_variant` column has no one type: row 1 can hold an `int` and row 2 an
//! `nvarchar`. So each value carries a header — a base type byte, a count of
//! property bytes, then those properties — and the type information a normal
//! column would have taken from `COLMETADATA` is read here instead, per row.
//! That is why this decodes into the ordinary `ColumnData` variants rather than
//! into a variant of its own: what came out of a `sql_variant` cell *is* an
//! `int`, and saying so is more use to a caller than handing back a box that
//! says "something".
//!
//! MS-TDS 2.2.5.5.2 defines the layout and the per-base-type property rules.

use std::borrow::Cow;

use byteorder::{ByteOrder, LittleEndian};
use futures_util::io::AsyncReadExt;
use uuid::Uuid;

use crate::{
    error::Error,
    sql_read_bytes::SqlReadBytes,
    tds::{
        codec::guid,
        time::{DateTime, SmallDateTime},
        Collation, Numeric,
    },
    ColumnData,
};

#[cfg(feature = "tds73")]
use crate::tds::time::{Date, DateTime2, DateTimeOffset, Time};

// The base types `sql_variant` is allowed to hold, as their TDS type bytes.
// Note that the integer, bit, float, money and datetime families appear as
// their fixed-length tokens rather than the nullable ones: a variant records the
// concrete type it stored, so there is no "n" to be nullable about.
const GUID: u8 = 0x24;
#[cfg(feature = "tds73")]
const DATEN: u8 = 0x28;
#[cfg(feature = "tds73")]
const TIMEN: u8 = 0x29;
#[cfg(feature = "tds73")]
const DATETIME2N: u8 = 0x2A;
#[cfg(feature = "tds73")]
const DATETIMEOFFSETN: u8 = 0x2B;
const INT1: u8 = 0x30;
const BIT: u8 = 0x32;
const INT2: u8 = 0x34;
const INT4: u8 = 0x38;
const DATETIM4: u8 = 0x3A;
const FLT4: u8 = 0x3B;
const MONEY: u8 = 0x3C;
const DATETIME: u8 = 0x3D;
const FLT8: u8 = 0x3E;
const DECIMALN: u8 = 0x6A;
const NUMERICN: u8 = 0x6C;
const MONEY4: u8 = 0x7A;
const INT8: u8 = 0x7F;
const BIGVARBIN: u8 = 0xA5;
const BIGVARCHR: u8 = 0xA7;
const BIGBINARY: u8 = 0xAD;
const BIGCHAR: u8 = 0xAF;
const NVARCHAR: u8 = 0xE7;
const NCHAR: u8 = 0xEF;

pub(crate) async fn decode<R>(src: &mut R) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    let total_len = src.read_u32_le().await? as usize;

    // A null variant is a length of zero and nothing else: no base type, so
    // nothing says what kind of null it is. `String(None)` is the honest
    // carrier, because a variant column has to be read as text anyway — it is
    // the only Arrow type that can hold every base type this can produce.
    if total_len == 0 {
        return Ok(ColumnData::String(None));
    }

    let base_type = src.read_u8().await?;
    let prop_bytes = src.read_u8().await? as usize;

    // The header counts against the total, so a total that cannot cover it is a
    // stream this side has lost its place in. Saying so beats reading a length
    // that wraps.
    let value_len = total_len
        .checked_sub(2 + prop_bytes)
        .ok_or_else(|| Error::Protocol("sql_variant: header longer than the value".into()))?;

    let data = match base_type {
        BIT => ColumnData::Bit(Some(src.read_u8().await? > 0)),
        INT1 => ColumnData::U8(Some(src.read_u8().await?)),
        INT2 => ColumnData::I16(Some(src.read_i16_le().await?)),
        INT4 => ColumnData::I32(Some(src.read_i32_le().await?)),
        INT8 => ColumnData::I64(Some(src.read_i64_le().await?)),
        FLT4 => ColumnData::F32(Some(src.read_f32_le().await?)),
        FLT8 => ColumnData::F64(Some(src.read_f64_le().await?)),
        // The same halving as `money::decode`, and it loses the same low bits
        // for the same reason: `money` needs 63 bits and an f64 holds 53.
        MONEY4 => ColumnData::F64(Some(src.read_i32_le().await? as f64 / 1e4)),
        MONEY => {
            let high = src.read_i32_le().await? as i64;
            let low = src.read_u32_le().await? as f64;

            ColumnData::F64(Some(((high << 32) as f64 + low) / 1e4))
        }
        DATETIM4 => ColumnData::SmallDateTime(Some(SmallDateTime::decode(src).await?)),
        DATETIME => ColumnData::DateTime(Some(DateTime::decode(src).await?)),
        GUID => {
            let mut data = [0u8; 16];
            src.read_exact(&mut data).await?;
            guid::reorder_bytes(&mut data);

            ColumnData::Guid(Some(Uuid::from_bytes(data)))
        }
        DECIMALN | NUMERICN => {
            let _precision = src.read_u8().await?;
            let scale = src.read_u8().await?;

            // `decode_body` rather than `decode`, because a variant states the
            // length in its own header and does not repeat it in front of the
            // digits the way a `decimal` column does.
            ColumnData::Numeric(Numeric::decode_body(src, value_len as u8, scale).await?)
        }
        BIGBINARY | BIGVARBIN => {
            let _max_length = src.read_u16_le().await?;

            ColumnData::Binary(Some(Cow::from(read_bytes(src, value_len).await?)))
        }
        BIGCHAR | BIGVARCHR => {
            let collation = read_collation(src).await?;
            let _max_length = src.read_u16_le().await?;
            let buf = read_bytes(src, value_len).await?;

            let text = collation
                .encoding()?
                .decode_without_bom_handling_and_without_replacement(&buf)
                .ok_or_else(|| Error::Encoding("invalid sequence".into()))?
                .to_string();

            ColumnData::String(Some(text.into()))
        }
        NCHAR | NVARCHAR => {
            // The collation rides along even for UTF-16, where it decides
            // comparison rather than encoding. It is read and dropped.
            let _collation = read_collation(src).await?;
            let _max_length = src.read_u16_le().await?;
            let buf = read_bytes(src, value_len).await?;

            if buf.len() % 2 != 0 {
                return Err(Error::Protocol("sql_variant: odd-length utf-16 value".into()));
            }
            let utf16: Vec<u16> = buf.chunks(2).map(LittleEndian::read_u16).collect();

            ColumnData::String(Some(String::from_utf16(&utf16)?.into()))
        }
        #[cfg(feature = "tds73")]
        DATEN => ColumnData::Date(Some(Date::decode(src).await?)),
        #[cfg(feature = "tds73")]
        TIMEN => {
            let scale = src.read_u8().await? as usize;

            ColumnData::Time(Some(Time::decode(src, scale, value_len).await?))
        }
        #[cfg(feature = "tds73")]
        DATETIME2N => {
            let scale = src.read_u8().await? as usize;
            // The three trailing bytes are the date; what `DateTime2::decode`
            // wants is the width of the time in front of it.
            let time_len = value_len
                .checked_sub(3)
                .ok_or_else(|| Error::Protocol("sql_variant: datetime2 too short".into()))?;

            ColumnData::DateTime2(Some(DateTime2::decode(src, scale, time_len).await?))
        }
        #[cfg(feature = "tds73")]
        DATETIMEOFFSETN => {
            let scale = src.read_u8().await? as usize;
            // Three bytes of date and two of offset follow the time.
            let time_len = value_len
                .checked_sub(5)
                .ok_or_else(|| Error::Protocol("sql_variant: datetimeoffset too short".into()))?;

            ColumnData::DateTimeOffset(Some(
                DateTimeOffset::decode(src, scale, time_len as u8).await?,
            ))
        }
        // A base type this client has no decoder for. Its bytes are consumed
        // first so that the token stream is left where the next value starts,
        // and the failure names the byte, which is what somebody adding it
        // would need to know.
        other => {
            read_bytes(src, prop_bytes + value_len).await?;

            return Err(Error::Protocol(
                format!("sql_variant: no decoder for base type {:#04x}", other).into(),
            ));
        }
    };

    Ok(data)
}

/// The five collation bytes, in the order a `COLMETADATA` collation uses.
async fn read_collation<R>(src: &mut R) -> crate::Result<Collation>
where
    R: SqlReadBytes + Unpin,
{
    let info = src.read_u32_le().await?;
    let sort_id = src.read_u8().await?;

    Ok(Collation::new(info, sort_id))
}

/// Exactly `len` bytes, or the failure that says the stream ended early.
///
/// The buffer is not preallocated from `len`: the length came off the wire, and
/// a corrupt one would otherwise be an allocation of whatever it said.
async fn read_bytes<R>(src: &mut R, len: usize) -> crate::Result<Vec<u8>>
where
    R: SqlReadBytes + Unpin,
{
    let mut data = Vec::new();
    for _ in 0..len {
        data.push(src.read_u8().await?);
    }

    Ok(data)
}
