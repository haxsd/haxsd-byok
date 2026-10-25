use std::io::{Read, Write};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};

use crate::{Error, Result};

pub const MAX_BODY_SIZE: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldValue {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(Vec<u8>),
    Fixed32([u8; 4]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub number: u32,
    pub value: FieldValue,
}

impl Field {
    pub fn varint(number: u32, value: u64) -> Self {
        Self {
            number,
            value: FieldValue::Varint(value),
        }
    }

    pub fn bytes(number: u32, value: impl AsRef<[u8]>) -> Self {
        Self {
            number,
            value: FieldValue::Bytes(value.as_ref().to_vec()),
        }
    }

    pub fn fixed64(number: u32, value: [u8; 8]) -> Self {
        Self {
            number,
            value: FieldValue::Fixed64(value),
        }
    }

    pub fn fixed32(number: u32, value: [u8; 4]) -> Self {
        Self {
            number,
            value: FieldValue::Fixed32(value),
        }
    }
}

pub fn parse_fields(payload: &[u8]) -> Result<Vec<Field>> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < payload.len() {
        let head = decode_varint(payload, &mut offset)?;
        let number = u32::try_from(head >> 3)
            .map_err(|_| Error::Protocol("Devin protobuf field number overflow".into()))?;
        if number == 0 {
            return Err(Error::Protocol(
                "Devin protobuf field number is zero".into(),
            ));
        }
        match (head & 7) as u8 {
            0 => fields.push(Field::varint(number, decode_varint(payload, &mut offset)?)),
            1 => fields.push(Field::fixed64(number, read_array(payload, &mut offset)?)),
            2 => {
                let length = usize::try_from(decode_varint(payload, &mut offset)?)
                    .map_err(|_| Error::Protocol("Devin protobuf length overflow".into()))?;
                let end = offset.checked_add(length).ok_or_else(|| {
                    Error::Protocol("Devin protobuf length arithmetic overflow".into())
                })?;
                if end > payload.len() {
                    return Err(Error::Protocol(
                        "truncated Devin protobuf bytes field".into(),
                    ));
                }
                fields.push(Field::bytes(number, &payload[offset..end]));
                offset = end;
            }
            5 => fields.push(Field::fixed32(number, read_array(payload, &mut offset)?)),
            wire_type => {
                return Err(Error::Protocol(format!(
                    "unsupported Devin protobuf wire type {wire_type}"
                )))
            }
        }
    }
    Ok(fields)
}

pub fn serialize_fields(fields: &[Field]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for field in fields {
        if field.number == 0 {
            return Err(Error::Protocol(
                "Devin protobuf field number is zero".into(),
            ));
        }
        let wire_type = match &field.value {
            FieldValue::Varint(_) => 0,
            FieldValue::Fixed64(_) => 1,
            FieldValue::Bytes(_) => 2,
            FieldValue::Fixed32(_) => 5,
        };
        encode_varint(u64::from(field.number) << 3 | wire_type, &mut output);
        match &field.value {
            FieldValue::Varint(value) => encode_varint(*value, &mut output),
            FieldValue::Fixed64(value) => output.extend_from_slice(value),
            FieldValue::Bytes(value) => {
                encode_varint(value.len() as u64, &mut output);
                output.extend_from_slice(value);
            }
            FieldValue::Fixed32(value) => output.extend_from_slice(value),
        }
    }
    Ok(output)
}

pub fn first_field(fields: &[Field], number: u32) -> Option<&Field> {
    fields.iter().find(|field| field.number == number)
}

pub fn all_fields(fields: &[Field], number: u32) -> impl Iterator<Item = &Field> {
    fields.iter().filter(move |field| field.number == number)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectError {
    pub code: String,
    pub message: String,
}

/// 解出来的请求：`enveloped` 表示请求体确实是一个 Connect 信封，响应必须照同样的
/// 封装回给客户端。
#[derive(Debug)]
pub struct UnwrappedRequest {
    pub payload: Vec<u8>,
    pub enveloped: bool,
}

pub fn unwrap_request(
    body: &[u8],
    http_content_encoding: Option<&str>,
    connect_content_encoding: Option<&str>,
) -> Result<UnwrappedRequest> {
    if body.len() > MAX_BODY_SIZE + 5 {
        return Err(Error::Protocol("Devin request body exceeds 24 MiB".into()));
    }
    let mut buffer = body.to_vec();
    // `Content-Encoding` describes the HTTP body. `Connect-Content-Encoding`
    // describes the message encoding: for an enveloped request the frame's own
    // flag already says whether its payload is gzip. Decompressing the whole
    // body from the Connect header would therefore unwrap the gzip twice.
    if encoding_has_gzip(http_content_encoding) {
        buffer = gunzip(&buffer)?;
    }
    if buffer.len() > MAX_BODY_SIZE + 5 {
        return Err(Error::Protocol("Devin request body exceeds 24 MiB".into()));
    }

    if buffer.len() >= 5 {
        let flags = buffer[0];
        let declared = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
        if declared == buffer.len() - 5 && flags <= 1 {
            let payload = &buffer[5..];
            let payload = if flags == 1 {
                gunzip(payload)?
            } else {
                payload.to_vec()
            };
            if payload.len() > MAX_BODY_SIZE {
                return Err(Error::Protocol("Devin request body exceeds 24 MiB".into()));
            }
            return Ok(UnwrappedRequest {
                payload,
                enveloped: true,
            });
        }
        if declared == buffer.len() - 5 && flags == 2 {
            return Err(Error::Protocol(
                "Devin end frame is not valid as a request".into(),
            ));
        }
    }
    if encoding_has_gzip(connect_content_encoding) {
        buffer = gunzip(&buffer)?;
    }
    if buffer.len() > MAX_BODY_SIZE {
        return Err(Error::Protocol("Devin request body exceeds 24 MiB".into()));
    }
    Ok(UnwrappedRequest {
        payload: buffer,
        enveloped: false,
    })
}

fn encoding_has_gzip(value: Option<&str>) -> bool {
    value
        .unwrap_or_default()
        .split(',')
        .any(|encoding| encoding.trim().eq_ignore_ascii_case("gzip"))
}

pub fn is_connect_envelope(body: &[u8]) -> bool {
    if body.len() < 5 {
        return false;
    }
    let flags = body[0];
    let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    declared == body.len() - 5 && flags <= 1
}

pub fn frame(payload: &[u8], compress: bool) -> Result<Vec<u8>> {
    if payload.len() > MAX_BODY_SIZE {
        return Err(Error::Protocol("Devin response body exceeds 24 MiB".into()));
    }
    let payload = if compress {
        gzip(payload)?
    } else {
        payload.to_vec()
    };
    framed(if compress { 1 } else { 0 }, &payload)
}

pub fn end_frame(error: Option<ConnectError>) -> Vec<u8> {
    let payload = match error.as_ref() {
        Some(error) => serde_json::to_vec(&serde_json::json!({
            "error": {"code": error.code, "message": error.message}
        }))
        .expect("Connect error JSON is serializable"),
        None => {
            serde_json::to_vec(&serde_json::json!({})).expect("Connect end JSON is serializable")
        }
    };
    framed(2, &payload).expect("Connect end frame length fits u32")
}

fn framed(flags: usize, payload: &[u8]) -> Result<Vec<u8>> {
    let length = u32::try_from(payload.len())
        .map_err(|_| Error::Protocol("Devin frame payload exceeds u32 length".into()))?;
    let mut frame = Vec::with_capacity(payload.len() + 5);
    frame.push(flags as u8);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn encode_varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn decode_varint(payload: &[u8], offset: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *payload
            .get(*offset)
            .ok_or_else(|| Error::Protocol("truncated Devin protobuf varint".into()))?;
        *offset += 1;
        if index == 9 && byte > 1 {
            return Err(Error::Protocol("Devin protobuf varint overflow".into()));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Protocol("unterminated Devin protobuf varint".into()))
}

fn read_array<const N: usize>(payload: &[u8], offset: &mut usize) -> Result<[u8; N]> {
    let end = offset
        .checked_add(N)
        .ok_or_else(|| Error::Protocol("Devin protobuf offset overflow".into()))?;
    let value = payload
        .get(*offset..end)
        .ok_or_else(|| Error::Protocol("truncated Devin protobuf fixed field".into()))?;
    *offset = end;
    value
        .try_into()
        .map_err(|_| Error::Protocol("invalid Devin protobuf fixed field".into()))
}

fn gzip(payload: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload).map_err(Error::Io)?;
    encoder.finish().map_err(Error::Io)
}

fn gunzip(payload: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(payload);
    let mut output = Vec::new();
    decoder
        .by_ref()
        .take((MAX_BODY_SIZE + 1) as u64)
        .read_to_end(&mut output)
        .map_err(|error| Error::Protocol(format!("invalid Devin gzip payload: {error}")))?;
    if output.len() > MAX_BODY_SIZE {
        return Err(Error::Protocol("Devin gzip payload exceeds 24 MiB".into()));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{ConnectError, Field, FieldValue, MAX_BODY_SIZE};

    #[test]
    fn protobuf_fields_round_trip_all_supported_wire_types() {
        let fields = vec![
            Field::varint(1, 300),
            Field::bytes(2, b"hello"),
            Field::fixed64(3, [1, 2, 3, 4, 5, 6, 7, 8]),
            Field::fixed32(4, [9, 10, 11, 12]),
        ];

        let encoded = super::serialize_fields(&fields).unwrap();
        let decoded = super::parse_fields(&encoded).unwrap();

        assert_eq!(decoded, fields);
        assert_eq!(decoded[0].value, FieldValue::Varint(300));
    }

    #[test]
    fn protobuf_parser_rejects_truncated_and_unterminated_varints() {
        for payload in [vec![0x80], vec![0x0a, 0x03, b'a'], vec![0x08; 11]] {
            assert!(super::parse_fields(&payload).is_err());
        }
    }

    #[test]
    fn connect_frame_round_trips_raw_and_gzip_payloads() {
        let payload = b"hello Devin";
        for compress in [false, true] {
            let frame = super::frame(payload, compress).unwrap();
            let unwrapped = super::unwrap_request(&frame, None, None).unwrap();
            assert_eq!(unwrapped.payload, payload);
            assert!(unwrapped.enveloped, "Connect 信封必须被认出来");
        }
    }

    #[test]
    fn http_gzip_wrapping_is_decoded_before_connect_frame() {
        let payload = b"hello Devin";
        let frame = super::frame(payload, false).unwrap();
        let http_body = super::gzip(&frame).unwrap();

        let unwrapped = super::unwrap_request(&http_body, Some("gzip"), None).unwrap();
        assert_eq!(unwrapped.payload, payload);
        assert!(
            unwrapped.enveloped,
            "HTTP gzip 包着的 Connect 信封同样是信封"
        );
    }

    #[test]
    fn connect_content_encoding_does_not_double_decompress_an_enveloped_payload() {
        let payload = b"hello Devin";
        let frame = super::frame(payload, true).unwrap();
        assert_eq!(frame[0], 1);
        assert_eq!(&frame[5..7], &[0x1f, 0x8b]);

        let unwrapped = super::unwrap_request(&frame, None, Some("gzip")).unwrap();
        assert_eq!(unwrapped.payload, payload);
        assert!(unwrapped.enveloped);
    }

    #[test]
    fn connect_content_encoding_can_decompress_a_raw_unary_body() {
        let payload = b"hello Devin";
        let body = super::gzip(payload).unwrap();

        let unwrapped = super::unwrap_request(&body, None, Some("gzip")).unwrap();
        assert_eq!(unwrapped.payload, payload);
        assert!(!unwrapped.enveloped);
    }

    #[test]
    fn recognizes_only_request_connect_frames() {
        let frame = super::frame(b"request", false).unwrap();
        assert!(super::is_connect_envelope(&frame));
        assert!(!super::is_connect_envelope(&super::end_frame(None)));
        assert!(!super::is_connect_envelope(b"raw protobuf"));
    }

    #[test]
    fn connect_error_end_frame_is_uncompressed_and_structured() {
        let frame = super::end_frame(Some(ConnectError {
            code: "invalid_argument".into(),
            message: "bad Devin payload".into(),
        }));

        assert_eq!(frame[0], 2);
        let payload = &frame[5..];
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(payload).unwrap(),
            serde_json::json!({
                "error": {"code": "invalid_argument", "message": "bad Devin payload"}
            })
        );
    }

    #[test]
    fn connect_end_frame_is_rejected_as_a_request() {
        assert!(super::unwrap_request(&super::end_frame(None), None, None).is_err());
    }

    #[test]
    fn connect_unwrap_rejects_body_larger_than_the_protocol_limit() {
        let payload = vec![0u8; MAX_BODY_SIZE + 1];
        let error = super::unwrap_request(&payload, None, None).unwrap_err();
        assert!(error.to_string().contains("24 MiB"));
    }
}
