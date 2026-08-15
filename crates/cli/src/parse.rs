use std::time::Duration;

use time::OffsetDateTime;

pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 63 {
        return Err("an instance name has 1 to 63 characters".into());
    }
    let bytes = name.as_bytes();
    if (!bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit())
        || (!bytes[bytes.len() - 1].is_ascii_lowercase()
            && !bytes[bytes.len() - 1].is_ascii_digit())
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err("an instance name uses lowercase letters, digits, and inner hyphens".into());
    }
    if matches!(name, "bento" | "www") {
        return Err(format!("the name {name:?} is reserved"));
    }
    Ok(())
}

pub(crate) fn parse_memory_mib(value: &str) -> Result<i64, String> {
    let (number, unit) =
        split_size(value).map_err(|()| format!("invalid memory size {value:?}"))?;
    match unit.as_str() {
        "" | "M" | "MB" | "MIB" => Ok(number),
        "G" | "GB" | "GIB" => number
            .checked_mul(1024)
            .ok_or_else(|| format!("invalid memory size {value:?}")),
        _ => Err(format!("invalid memory unit in {value:?} (use MiB or GiB)")),
    }
}

pub(crate) fn parse_disk_gib(value: &str) -> Result<i64, String> {
    let (number, unit) = split_size(value).map_err(|()| format!("invalid disk size {value:?}"))?;
    match unit.as_str() {
        "" | "G" | "GB" | "GIB" => Ok(number),
        _ => Err(format!("invalid disk unit in {value:?} (use GiB)")),
    }
}

fn split_size(value: &str) -> Result<(i64, String), ()> {
    let value = value.trim().to_ascii_uppercase();
    let split = value
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_ascii_digit())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let number = value[..split].parse::<i64>().map_err(|_| ())?;
    if number <= 0 {
        return Err(());
    }
    Ok((number, value[split..].trim().to_owned()))
}

pub(crate) fn format_cooldown(duration: Duration) -> String {
    if duration < Duration::from_secs(60) {
        return "less than a minute".into();
    }
    let minutes = duration.as_secs().div_ceil(60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    match (hours, minutes) {
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h{minutes}m"),
    }
}

pub(crate) fn ago(now: OffsetDateTime, then: Option<OffsetDateTime>) -> String {
    let Some(then) = then else {
        return "never".into();
    };
    let seconds = (now - then).whole_seconds();
    if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[derive(Default)]
pub(crate) struct Flags {
    pub(crate) positionals: Vec<String>,
    pub(crate) values: std::collections::HashMap<String, String>,
    pub(crate) booleans: std::collections::HashMap<String, bool>,
}

pub(crate) fn parse_flags(
    args: &[String],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<Flags, String> {
    let mut parsed = Flags::default();
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--" {
            parsed.positionals.extend_from_slice(&args[index + 1..]);
            break;
        }
        if !argument.starts_with('-') || argument == "-" {
            parsed.positionals.extend_from_slice(&args[index..]);
            break;
        }
        let raw = argument.trim_start_matches('-');
        let (name, inline) = raw
            .split_once('=')
            .map_or((raw, None), |(n, v)| (n, Some(v)));
        if bool_flags.contains(&name) {
            let value = match inline {
                None | Some("true") => true,
                Some("false") => false,
                Some(value) => return Err(format!("invalid value {value:?} for flag -{name}")),
            };
            parsed.booleans.insert(name.into(), value);
        } else if value_flags.contains(&name) {
            let value = if let Some(value) = inline {
                value.to_owned()
            } else {
                index += 1;
                args.get(index)
                    .cloned()
                    .ok_or_else(|| format!("flag needs an argument: -{name}"))?
            };
            parsed.values.insert(name.into(), value);
        } else {
            return Err(format!("flag provided but not defined: -{name}"));
        }
        index += 1;
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_time() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_754_827_200).unwrap()
    }

    #[test]
    fn parse_memory_mib_values() {
        for (input, expected) in [
            ("2048", Some(2048)),
            ("512M", Some(512)),
            ("512MiB", Some(512)),
            ("2G", Some(2048)),
            ("2GiB", Some(2048)),
            ("2gb", Some(2048)),
            (" 4 GiB ", Some(4096)),
            ("0", None),
            ("-1", None),
            ("2T", None),
            ("abc", None),
            ("", None),
        ] {
            assert_eq!(parse_memory_mib(input).ok(), expected, "{input:?}");
        }
    }

    #[test]
    fn parse_disk_gib_values() {
        for (input, expected) in [
            ("20", Some(20)),
            ("20G", Some(20)),
            ("20GiB", Some(20)),
            ("20gb", Some(20)),
            ("512M", None),
            ("0", None),
            ("x", None),
        ] {
            assert_eq!(parse_disk_gib(input).ok(), expected, "{input:?}");
        }
    }

    #[test]
    fn validate_instance_names() {
        for name in ["web", "my-app-2", "a", "0z"] {
            assert!(validate_name(name).is_ok(), "{name}");
        }
        for name in ["", "-web", "web-", "Web", "we_b", "we.b", "bento", "www"] {
            assert!(validate_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn cooldown_formatting() {
        for (duration, expected) in [
            (Duration::from_secs(30), "less than a minute"),
            (Duration::from_secs(90), "2m"),
            (Duration::from_secs(45 * 60), "45m"),
            (Duration::from_secs(60 * 60), "1h"),
            (Duration::from_secs(23 * 3600 + 59 * 60 + 5), "24h"),
            (Duration::from_secs(23 * 3600 + 30 * 60), "23h30m"),
            (Duration::from_secs(24 * 3600), "24h"),
        ] {
            assert_eq!(format_cooldown(duration), expected);
        }
    }

    #[test]
    fn ago_formatting() {
        let now = test_time();
        for (then, expected) in [
            (None, "never"),
            (Some(now - time::Duration::seconds(10)), "just now"),
            (Some(now - time::Duration::minutes(5)), "5m ago"),
            (Some(now - time::Duration::hours(3)), "3h ago"),
            (Some(now - time::Duration::hours(49)), "2d ago"),
        ] {
            assert_eq!(ago(now, then), expected);
        }
    }
}
