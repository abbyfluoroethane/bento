use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::Error;

pub(crate) fn not_after(certificate_der: &[u8]) -> Result<OffsetDateTime, Error> {
    let certificate = expect_tlv(certificate_der, 0x30, "certificate")?;
    let (tbs, _) = take_tlv(certificate, "certificate body")?;
    if tbs.tag != 0x30 {
        return Err(Error::Certificate(
            "certificate body is not a sequence".to_string(),
        ));
    }

    let mut rest = tbs.value;
    if rest.first() == Some(&0xa0) {
        rest = take_tlv(rest, "certificate version")?.1;
    }
    for field in ["serial number", "signature algorithm", "issuer"] {
        rest = take_tlv(rest, field)?.1;
    }
    let (validity, _) = take_tlv(rest, "certificate validity")?;
    if validity.tag != 0x30 {
        return Err(Error::Certificate(
            "certificate validity is not a sequence".to_string(),
        ));
    }
    let (_, validity_rest) = take_tlv(validity.value, "notBefore")?;
    let (not_after, _) = take_tlv(validity_rest, "notAfter")?;
    parse_asn1_time(not_after.tag, not_after.value)
}

struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
}

fn expect_tlv<'a>(input: &'a [u8], tag: u8, name: &str) -> Result<&'a [u8], Error> {
    let (tlv, _) = take_tlv(input, name)?;
    if tlv.tag != tag {
        return Err(Error::Certificate(format!(
            "{name} has unexpected ASN.1 tag"
        )));
    }
    Ok(tlv.value)
}

fn take_tlv<'a>(input: &'a [u8], name: &str) -> Result<(Tlv<'a>, &'a [u8]), Error> {
    if input.len() < 2 {
        return Err(Error::Certificate(format!("truncated {name}")));
    }
    let tag = input[0];
    let initial = input[1];
    let (length, header_length) = if initial & 0x80 == 0 {
        (usize::from(initial), 2)
    } else {
        let count = usize::from(initial & 0x7f);
        if count == 0 || count > std::mem::size_of::<usize>() || input.len() < 2 + count {
            return Err(Error::Certificate(format!("invalid length in {name}")));
        }
        let mut length = 0usize;
        for byte in &input[2..2 + count] {
            length = length
                .checked_mul(256)
                .and_then(|value| value.checked_add(usize::from(*byte)))
                .ok_or_else(|| Error::Certificate(format!("length overflow in {name}")))?;
        }
        (length, 2 + count)
    };
    let end = header_length
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| Error::Certificate(format!("truncated {name}")))?;
    Ok((
        Tlv {
            tag,
            value: &input[header_length..end],
        },
        &input[end..],
    ))
}

fn parse_asn1_time(tag: u8, value: &[u8]) -> Result<OffsetDateTime, Error> {
    let (year, digits) = match (tag, value.len()) {
        (0x17, 13) => {
            let short = number(&value[0..2])?;
            (
                (if short >= 50 { 1900 } else { 2000 }) + i32::from(short),
                &value[2..12],
            )
        }
        (0x18, 15) => (i32::from(number(&value[0..4])?), &value[4..14]),
        _ => {
            return Err(Error::Certificate(
                "unsupported certificate notAfter time".to_string(),
            ));
        }
    };
    if value.last() != Some(&b'Z') {
        return Err(Error::Certificate(
            "certificate notAfter is not UTC".to_string(),
        ));
    }
    let month = Month::try_from(u8::try_from(number(&digits[0..2])?).map_err(|error| {
        Error::Certificate(format!("invalid certificate notAfter month: {error}"))
    })?)
    .map_err(|error| Error::Certificate(format!("invalid certificate notAfter: {error}")))?;
    let day = u8::try_from(number(&digits[2..4])?).map_err(|error| {
        Error::Certificate(format!("invalid certificate notAfter day: {error}"))
    })?;
    let hour = u8::try_from(number(&digits[4..6])?).map_err(|error| {
        Error::Certificate(format!("invalid certificate notAfter hour: {error}"))
    })?;
    let minute = u8::try_from(number(&digits[6..8])?).map_err(|error| {
        Error::Certificate(format!("invalid certificate notAfter minute: {error}"))
    })?;
    let second = u8::try_from(number(&digits[8..10])?).map_err(|error| {
        Error::Certificate(format!("invalid certificate notAfter second: {error}"))
    })?;
    let date = Date::from_calendar_date(year, month, day)
        .map_err(|error| Error::Certificate(format!("invalid certificate notAfter: {error}")))?;
    let time = Time::from_hms(hour, minute, second)
        .map_err(|error| Error::Certificate(format!("invalid certificate notAfter: {error}")))?;
    Ok(PrimitiveDateTime::new(date, time).assume_offset(UtcOffset::UTC))
}

fn number(digits: &[u8]) -> Result<u16, Error> {
    digits.iter().try_fold(0u16, |value, digit| {
        if !digit.is_ascii_digit() {
            return Err(Error::Certificate(
                "invalid digit in certificate notAfter".to_string(),
            ));
        }
        Ok(value * 10 + u16::from(digit - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, date_time_ymd};

    use super::not_after;

    #[test]
    fn reads_certificate_expiry() {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(["bento.example.org".to_string()]).unwrap();
        params.not_after = date_time_ymd(2031, 7, 8);
        let certificate = params.self_signed(&key).unwrap();

        let expiry = not_after(certificate.der()).unwrap();
        assert_eq!(expiry, date_time_ymd(2031, 7, 8));
    }
}
