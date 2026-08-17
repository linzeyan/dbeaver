use crate::{sql_read_bytes::SqlReadBytes, ColumnData};

pub(crate) async fn decode<R>(src: &mut R, type_len: usize) -> crate::Result<ColumnData<'static>>
where
    R: SqlReadBytes + Unpin,
{
    let recv_len = src.read_u8().await? as usize;

    let res = match (recv_len, type_len) {
        (0, 1) => ColumnData::U8(None),
        (0, 2) => ColumnData::I16(None),
        (0, 4) => ColumnData::I32(None),
        (0, _) => ColumnData::I64(None),
        (1, _) => ColumnData::U8(Some(src.read_u8().await?)),
        (2, _) => ColumnData::I16(Some(src.read_i16_le().await?)),
        (4, _) => ColumnData::I32(Some(src.read_i32_le().await?)),
        (8, _) => ColumnData::I64(Some(src.read_i64_le().await?)),
        _ => {
            return Err(crate::Error::Protocol(
                format!(
                    "Intn with recv_len {} and type_len {} is not a supported size",
                    recv_len, type_len
                )
                .into(),
            ))
        }
    };

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_read_bytes::test_utils::IntoSqlReadBytes;
    use bytes::{BufMut, BytesMut};

    // No real SQL Server sends an Intn with an unsupported recv_len,
    // so we build the bytes by hand to prove the decoder returns an
    // error rather than panicking.
    #[tokio::test]
    async fn intn_unsupported_recv_len_returns_err() {
        // recv_len = 3 (not 0, 1, 2, 4, or 8), type_len = 4
        let mut buf = BytesMut::new();
        buf.put_u8(3); // recv_len
        buf.put_u8(0xAA); // dummy value (won't be read)

        let mut reader = buf.into_sql_read_bytes();
        let result = decode(&mut reader, 4).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, crate::Error::Protocol(_)));
    }
}
