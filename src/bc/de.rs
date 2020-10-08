use super::model::*;
use super::xml::{BcPayloads, BcXml, BcXmls};
use super::xml_crypto;
use err_derive::Error;
use log::*;
use nom::IResult;
use nom::{bytes::streaming::take, combinator::*, number::streaming::*, sequence::*};
use std::io::Read;

#[derive(Debug, Error)]
pub enum Error {
    #[error(display = "Parsing error")]
    NomError(&'static str),
    #[error(display = "I/O error")]
    IoError(#[error(source)] std::io::Error),
}

type NomErrorTuple<'a> = (&'a [u8], nom::error::ErrorKind);

impl<'a> From<nom::Err<NomErrorTuple<'a>>> for Error {
    fn from(k: nom::Err<NomErrorTuple<'a>>) -> Self {
        let reason = match k {
            nom::Err::Error(_) => "Nom Error",
            nom::Err::Failure(_) => "Nom Failure",
            _ => "Unknown Nom error",
        };
        Error::NomError(reason)
    }
}

impl Bc {
    pub fn deserialize<R: Read>(context: &mut BcContext, r: R) -> Result<Bc, Error> {
        // Throw away the nom-specific return types
        read_from_reader(|reader| bc_msg(context, reader), r)
    }
}

fn read_from_reader<P, O, E, R>(mut parser: P, mut rdr: R) -> Result<O, E>
where
    R: Read,
    E: for<'a> From<nom::Err<NomErrorTuple<'a>>> + From<std::io::Error>,
    P: FnMut(&[u8]) -> nom::IResult<&[u8], O>,
{
    let mut input: Vec<u8> = Vec::new();
    loop {
        let to_read = match parser(&input) {
            Ok((_, parsed)) => return Ok(parsed),
            Err(nom::Err::Incomplete(needed)) => {
                match needed {
                    nom::Needed::Unknown => 1, // read one byte
                    nom::Needed::Size(len) => len,
                }
            }
            Err(e) => return Err(e.into()),
        };

        if 0 == (&mut rdr).take(to_read as u64).read_to_end(&mut input)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "Read returned 0 bytes",
            )
            .into());
        }
    }
}

fn bc_msg<'a, 'b>(context: &'a mut BcContext, buf: &'b [u8]) -> IResult<&'b [u8], Bc> {
    let (buf, header) = bc_header(buf)?;
    let (buf, body) = bc_body(context, &header, buf)?;

    let bc = Bc {
        meta: header.to_meta(),
        body,
    };

    Ok((buf, bc))
}

fn bc_body<'a, 'b, 'c>(
    context: &'c mut BcContext,
    header: &'a BcHeader,
    buf: &'b [u8],
) -> IResult<&'b [u8], BcBody> {
    if header.is_modern() {
        let (buf, body) = bc_modern_msg(context, header, buf)?;
        Ok((buf, BcBody::ModernMsg(body)))
    } else {
        let (buf, body) = match header.msg_id {
            MSG_ID_LOGIN => bc_legacy_login_msg(buf)?,
            _ => (buf, LegacyMsg::UnknownMsg),
        };
        Ok((buf, BcBody::LegacyMsg(body)))
    }
}

fn hex32<'a>() -> impl Fn(&'a [u8]) -> IResult<&'a [u8], String> {
    map_res(take(32usize), |slice: &'a [u8]| {
        String::from_utf8(slice.to_vec())
    })
}

fn bc_legacy_login_msg<'a>(buf: &'a [u8]) -> IResult<&'a [u8], LegacyMsg> {
    let (buf, username) = hex32()(buf)?;
    let (buf, password) = hex32()(buf)?;

    Ok((buf, LegacyMsg::LoginMsg { username, password }))
}

fn bc_modern_msg<'a, 'b>(
    context: &mut BcContext,
    header: &'a BcHeader,
    buf: &'b [u8],
) -> IResult<&'b [u8], ModernMsg> {
    use nom::{
        error::{make_error, ErrorKind},
        Err,
    };

    let xml_len = match header.payload_offset {
        Some(off) => off,
        _ => header.body_len,
    };

    let (buf, xml_buf) = take(xml_len)(buf)?;
    let payload_len = header.body_len - xml_len;
    let (buf, payload_buf) = take(payload_len)(buf)?;

    let decrypted;
    let processed_xml_buf = if !header.is_encrypted() {
        xml_buf
    } else {
        decrypted = xml_crypto::crypt(header.enc_offset, xml_buf);
        &decrypted
    };

    // Now we'll take the buffer that Nom gave a ref to and parse it.
    let xml;
    if xml_len > 0 {
        // Apply the XML parse function, but throw away the reference to decrypted in the Ok and
        // Err case. This error-error-error thing is the same idiom Nom uses internally.
        let parsed = BcXmls::try_parse(processed_xml_buf)
            .map_err(|_| Err::Error(make_error(buf, ErrorKind::MapRes)))?;
        xml = Some(parsed);
    } else {
        xml = None;
    }

    // Now to handle the payload block
    // This block can either be xml or binary depending on what the message expects.
    // For our purposes we use try_parse and if all xml based parsers fail we treat
    // As binary
    let payload;
    if payload_len > 0 {
        // Extract remainder of message as binary, if it exists
        let decrypted;
        let processed_payload_buf = if !header.is_encrypted() {
            xml_buf
        } else {
            decrypted = xml_crypto::crypt(header.enc_offset, payload_buf);
            &decrypted
        };
        if let Ok(xml) = BcXml::try_parse(processed_payload_buf) {
            payload = Some(BcPayloads::BcXml(xml));
        } else {
            payload = Some(BcPayloads::Binary(payload_buf.to_vec()));
        }
    } else {
        payload = None;
    }

    Ok((buf, ModernMsg { xml, payload }))
}

fn bc_header(buf: &[u8]) -> IResult<&[u8], BcHeader> {
    let (buf, _magic) = verify(le_u32, |x| *x == MAGIC_HEADER)(buf)?;
    let (buf, msg_id) = le_u32(buf)?;
    let (buf, body_len) = le_u32(buf)?;
    let (buf, enc_offset) = le_u32(buf)?;
    let (buf, (response_code, _ignored, class)) = tuple((le_u8, le_u8, le_u16))(buf)?;

    // All modern messages are encrypted.  In addition, it seems that the camera firmware checks
    // this field to see if some other messages should be encrypted.  This is still somewhat fuzzy.
    // A copy of the source code for the camera would be very useful.
    let encrypted = response_code != 0;

    let (buf, payload_offset) = cond(has_payload_offset(class), le_u32)(buf)?;

    Ok((
        buf,
        BcHeader {
            msg_id,
            body_len,
            enc_offset,
            encrypted,
            class,
            payload_offset,
        },
    ))
}

#[test]
fn test_bc_modern_login() {
    let sample = include_bytes!("samples/model_sample_modern_login.bin");

    let mut context = BcContext::new();

    let (buf, header) = bc_header(&sample[..]).unwrap();
    let (_, body) = bc_body(&mut context, &header, buf).unwrap();
    assert_eq!(header.msg_id, 1);
    assert_eq!(header.body_len, 145);
    assert_eq!(header.enc_offset, 0x1000000);
    assert_eq!(header.encrypted, true);
    assert_eq!(header.class, 0x6614);
    match body {
        BcBody::ModernMsg(ModernMsg {
            xml: Some(ref xml),
            binary: None,
        }) => assert_eq!(xml.encryption.as_ref().unwrap().nonce, "9E6D1FCB9E69846D"),
        _ => assert!(false),
    }
}

#[test]
fn test_bc_legacy_login() {
    let sample = include_bytes!("samples/model_sample_legacy_login.bin");

    let mut context = BcContext::new();

    let (buf, header) = bc_header(&sample[..]).unwrap();
    let (_, body) = bc_body(&mut context, &header, buf).unwrap();
    assert_eq!(header.msg_id, 1);
    assert_eq!(header.body_len, 1836);
    assert_eq!(header.enc_offset, 0x1000000);
    assert_eq!(header.encrypted, true);
    assert_eq!(header.class, 0x6514);
    match body {
        BcBody::LegacyMsg(LegacyMsg::LoginMsg { username, password }) => {
            assert_eq!(username, "21232F297A57A5A743894A0E4A801FC\0");
            assert_eq!(password, EMPTY_LEGACY_PASSWORD);
        }
        _ => assert!(false),
    }
}

#[test]
fn test_bc_modern_login_failed() {
    let sample = include_bytes!("samples/modern_login_failed.bin");

    let mut context = BcContext::new();

    let (buf, header) = bc_header(&sample[..]).unwrap();
    let (_, body) = bc_body(&mut context, &header, buf).unwrap();
    assert_eq!(header.msg_id, 1);
    assert_eq!(header.body_len, 0);
    assert_eq!(header.enc_offset, 0x0);
    assert_eq!(header.encrypted, true);
    assert_eq!(header.class, 0x0000);
    match body {
        BcBody::ModernMsg(ModernMsg {
            xml: None,
            binary: None,
        }) => {
            assert!(true);
        }
        _ => assert!(false),
    }
}

#[test]
fn test_bc_modern_login_success() {
    let sample = include_bytes!("samples/modern_login_success.bin");

    let mut context = BcContext::new();

    let (buf, header) = bc_header(&sample[..]).unwrap();
    let (_, body) = bc_body(&mut context, &header, buf).unwrap();
    assert_eq!(header.msg_id, 1);
    assert_eq!(header.body_len, 2949);
    assert_eq!(header.enc_offset, 0x0);
    assert_eq!(header.encrypted, true);
    assert_eq!(header.class, 0x0000);

    // Previously, we were not handling payload_offset == 0 (no bin offset) correctly.
    // Test that we decoded XML and no binary.
    match body {
        BcBody::ModernMsg(ModernMsg {
            xml: Some(_),
            binary: None,
        }) => assert!(true),
        _ => assert!(false),
    }
}

#[test]
fn test_bc_binary_mode() {
    let sample1 = include_bytes!("samples/modern_video_start1.bin");
    let sample2 = include_bytes!("samples/modern_video_start2.bin");

    let mut context = BcContext::new();

    let msg1 = Bc::deserialize(&mut context, &sample1[..]).unwrap();
    let msg2 = Bc::deserialize(&mut context, &sample2[..]).unwrap();
    match msg1.body {
        BcBody::ModernMsg(ModernMsg {
            xml: None,
            binary: Some(bin),
        }) => {
            assert_eq!(bin.len(), 32);
        }
        _ => assert!(false),
    }
    match msg2.body {
        BcBody::ModernMsg(ModernMsg {
            xml: None,
            binary: Some(bin),
        }) => {
            assert_eq!(bin.len(), 30344);
        }
        _ => assert!(false),
    }
}
