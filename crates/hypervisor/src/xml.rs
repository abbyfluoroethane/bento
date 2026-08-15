use crate::Error;

/// The libvirt spelling for x86-64.
pub const ARCH_AMD64: &str = "x86_64";
/// The libvirt spelling for 64-bit Arm.
pub const ARCH_ARM64: &str = "aarch64";

/// Every per-instance value interpolated into the one fixed domain XML
/// template (SPEC 4.1 and section 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainSpec {
    pub name: String,
    pub uuid: String,
    pub vcpu: u32,
    pub memory_mib: i64,
    /// Absolute path of the qcow2 overlay used as the virtio-blk root.
    pub disk_path: String,
    /// Optional cloud-init NoCloud ISO (SPEC 5.2).
    pub iso_path: String,
    /// The owner's libvirt network name (SPEC 6.2).
    pub network: String,
    /// Bento assigns the MAC; libvirt never generates it (SPEC 5).
    pub mac: String,
    /// Selects host-passthrough for nested virtualization (SPEC 5.5).
    pub nested: bool,
    /// False emits `<nosharepages/>` (SPEC 5.4).
    pub ksm: bool,
    /// Empty selects the host architecture. Bento only runs KVM guests
    /// of the host's architecture.
    pub arch: String,
}

impl DomainSpec {
    /// Rejects values that cannot produce valid domain XML.
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |message: String| Error::Operation(format!("domain spec: {message}"));
        if self.name.is_empty() {
            return Err(invalid("name is empty".to_string()));
        }
        if self.uuid.is_empty() {
            return Err(invalid("uuid is empty".to_string()));
        }
        if self.vcpu < 1 {
            return Err(invalid(format!("vcpu {} < 1", self.vcpu)));
        }
        if self.memory_mib < 1 {
            return Err(invalid(format!("memory {} MiB < 1", self.memory_mib)));
        }
        if self.disk_path.is_empty() {
            return Err(invalid("disk path is empty".to_string()));
        }
        if self.network.is_empty() {
            return Err(invalid("network is empty".to_string()));
        }
        if !valid_mac(&self.mac) {
            return Err(invalid(format!("mac {:?}: invalid MAC address", self.mac)));
        }
        if !matches!(self.arch.as_str(), "" | ARCH_AMD64 | ARCH_ARM64) {
            return Err(invalid(format!(
                "arch {:?} is not one of {ARCH_AMD64}, {ARCH_ARM64}",
                self.arch
            )));
        }
        Ok(())
    }
}

fn valid_mac(mac: &str) -> bool {
    let separator = if mac.contains(':') {
        ':'
    } else if mac.contains('-') {
        '-'
    } else {
        return false;
    };
    let parts: Vec<_> = mac.split(separator).collect();
    matches!(parts.len(), 6 | 8)
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.as_bytes().iter().all(u8::is_ascii_hexdigit))
}

/// Returns the running host architecture in libvirt's spelling.
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => ARCH_AMD64,
        "aarch64" => ARCH_ARM64,
        architecture => architecture,
    }
}

// Every string reaching the XML is hostile (SPEC 4.2). Escaping covers
// both element text and single-quoted attributes and preserves whitespace
// exactly when an XML parser reads the value back.
fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("&#34;"),
            '\'' => escaped.push_str("&#39;"),
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\t' => escaped.push_str("&#x9;"),
            '\n' => escaped.push_str("&#xA;"),
            '\r' => escaped.push_str("&#xD;"),
            c if is_xml_character(c) => escaped.push(c),
            _ => escaped.push('\u{fffd}'),
        }
    }
    escaped
}

fn is_xml_character(character: char) -> bool {
    matches!(character as u32, 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}

pub(crate) fn domain_identity(xml: &str) -> Result<(String, String), String> {
    validate_xml(xml)?;
    let root = first_start_tag(xml).ok_or_else(|| "document has no root element".to_string())?;
    if root != "domain" {
        return Err("root element is not domain".to_string());
    }
    Ok((direct_text(xml, "name")?, direct_text(xml, "uuid")?))
}

fn first_start_tag(xml: &str) -> Option<&str> {
    let mut rest = xml;
    loop {
        let start = rest.find('<')?;
        rest = &rest[start + 1..];
        if rest.starts_with('?') {
            rest = rest.split_once("?>")?.1;
            continue;
        }
        if rest.starts_with("!--") {
            rest = rest.split_once("-->")?.1;
            continue;
        }
        let end =
            rest.find(|character: char| character.is_ascii_whitespace() || character == '>')?;
        return Some(&rest[..end]);
    }
}

fn direct_text(xml: &str, name: &str) -> Result<String, String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml
        .find(&open)
        .ok_or_else(|| format!("domain xml missing {name}"))?
        + open.len();
    let end = xml[start..]
        .find(&close)
        .map(|offset| start + offset)
        .ok_or_else(|| format!("domain xml has unclosed {name}"))?;
    decode_entities(&xml[start..end])
}

fn validate_xml(xml: &str) -> Result<(), String> {
    let mut stack: Vec<String> = Vec::new();
    let mut offset = 0;
    let mut roots = 0;
    while offset < xml.len() {
        let Some(relative) = xml[offset..].find('<') else {
            validate_text(&xml[offset..])?;
            break;
        };
        let start = offset + relative;
        let text = &xml[offset..start];
        validate_text(text)?;
        if stack.is_empty() && !text.trim().is_empty() {
            return Err("text outside the root element".to_string());
        }

        if xml[start..].starts_with("<!--") {
            let end = xml[start + 4..]
                .find("-->")
                .map(|index| start + 4 + index + 3)
                .ok_or_else(|| "unclosed XML comment".to_string())?;
            offset = end;
            continue;
        }
        if xml[start..].starts_with("<?") {
            let end = xml[start + 2..]
                .find("?>")
                .map(|index| start + 2 + index + 2)
                .ok_or_else(|| "unclosed XML processing instruction".to_string())?;
            offset = end;
            continue;
        }
        let end = tag_end(xml, start + 1)?;
        let mut body = xml[start + 1..end].trim();
        if let Some(end_name) = body.strip_prefix('/') {
            let end_name = end_name.trim();
            if end_name.is_empty() || end_name.chars().any(char::is_whitespace) {
                return Err("invalid XML end tag".to_string());
            }
            let open_name = stack
                .pop()
                .ok_or_else(|| format!("unexpected </{end_name}>"))?;
            if open_name != end_name {
                return Err(format!(
                    "mismatched XML tags: <{open_name}> and </{end_name}>"
                ));
            }
        } else {
            let self_closing = body.ends_with('/');
            if self_closing {
                body = body[..body.len() - 1].trim_end();
            }
            let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
            let name = &body[..name_end];
            if !valid_xml_name(name) {
                return Err(format!("invalid XML element name {name:?}"));
            }
            validate_attributes(&body[name_end..])?;
            if stack.is_empty() {
                roots += 1;
                if roots > 1 {
                    return Err("document has more than one root element".to_string());
                }
            }
            if !self_closing {
                stack.push(name.to_string());
            }
        }
        offset = end + 1;
    }
    if let Some(name) = stack.last() {
        return Err(format!("unclosed XML element <{name}>"));
    }
    if roots != 1 {
        return Err("document has no root element".to_string());
    }
    Ok(())
}

fn tag_end(xml: &str, mut offset: usize) -> Result<usize, String> {
    let mut quote = None;
    while offset < xml.len() {
        let character = xml[offset..].chars().next().expect("offset is in bounds");
        match (quote, character) {
            (Some(expected), found) if expected == found => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, '>') => return Ok(offset),
            _ => {}
        }
        offset += character.len_utf8();
    }
    Err("unclosed XML tag".to_string())
}

fn valid_xml_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some('A'..='Z' | 'a'..='z' | '_' | ':'))
        && characters.all(|character| {
            matches!(character, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | ':' | '-' | '.')
        })
}

fn validate_attributes(mut attributes: &str) -> Result<(), String> {
    while !attributes.trim_start().is_empty() {
        attributes = attributes.trim_start();
        let name_end = attributes
            .find(|character: char| character.is_ascii_whitespace() || character == '=')
            .unwrap_or(attributes.len());
        let name = &attributes[..name_end];
        if !valid_xml_name(name) {
            return Err(format!("invalid XML attribute name {name:?}"));
        }
        attributes = attributes[name_end..].trim_start();
        attributes = attributes
            .strip_prefix('=')
            .ok_or_else(|| format!("attribute {name} has no equals sign"))?
            .trim_start();
        let quote = attributes
            .chars()
            .next()
            .filter(|quote| matches!(quote, '\'' | '"'))
            .ok_or_else(|| format!("attribute {name} is not quoted"))?;
        attributes = &attributes[quote.len_utf8()..];
        let end = attributes
            .find(quote)
            .ok_or_else(|| format!("attribute {name} has no closing quote"))?;
        let value = &attributes[..end];
        if value.contains('<') {
            return Err(format!("attribute {name} contains an unescaped <"));
        }
        decode_entities(value)?;
        attributes = &attributes[end + quote.len_utf8()..];
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<(), String> {
    if text.contains("]]>") {
        return Err("text contains ]]> outside CDATA".to_string());
    }
    decode_entities(text).map(drop)
}

fn decode_entities(text: &str) -> Result<String, String> {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        decoded.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let end = rest
            .find(';')
            .ok_or_else(|| "unclosed XML entity".to_string())?;
        let entity = &rest[..end];
        let character = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            value if value.starts_with("#x") => char::from_u32(
                u32::from_str_radix(&value[2..], 16)
                    .map_err(|_| format!("invalid XML entity &{entity};"))?,
            )
            .ok_or_else(|| format!("invalid XML code point &{entity};"))?,
            value if value.starts_with('#') => char::from_u32(
                value[1..]
                    .parse()
                    .map_err(|_| format!("invalid XML entity &{entity};"))?,
            )
            .ok_or_else(|| format!("invalid XML code point &{entity};"))?,
            _ => return Err(format!("unknown XML entity &{entity};")),
        };
        if !matches!(character, '\t' | '\n' | '\r') && !is_xml_character(character) {
            return Err(format!("invalid XML code point &{entity};"));
        }
        decoded.push(character);
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    Ok(decoded)
}

/// Renders one instance with the fixed device set in SPEC section 5.
/// The format literal is the sole domain template; every string argument
/// is XML-escaped before interpolation.
pub fn domain_xml(spec: &DomainSpec) -> Result<String, Error> {
    spec.validate()?;
    let arch = if spec.arch.is_empty() {
        host_arch()
    } else {
        &spec.arch
    };
    let arm64 = arch == ARCH_ARM64;
    let machine = if arm64 { "virt" } else { "q35" };
    let memory_backing = if spec.ksm {
        ""
    } else {
        "  <memoryBacking>\n    <nosharepages/>\n  </memoryBacking>\n"
    };
    let cpu_mode = if spec.nested || arm64 {
        "host-passthrough"
    } else {
        "host-model"
    };
    let interrupt_controller = if arm64 {
        "    <gic version='3'/>"
    } else {
        "    <apic/>"
    };
    let iso = if spec.iso_path.is_empty() {
        String::new()
    } else {
        let bus = if arm64 { "scsi" } else { "sata" };
        let controller = if arm64 {
            "    <!-- The aarch64 virt machine has no SATA controller, so the seed\n         CD-ROM of SPEC 5.2 hangs off virtio-scsi instead. -->\n    <controller type='scsi' index='0' model='virtio-scsi'/>\n"
        } else {
            ""
        };
        format!(
            "    <disk type='file' device='cdrom'>\n      <driver name='qemu' type='raw'/>\n      <source file='{}'/>\n      <target dev='sda' bus='{bus}'/>\n      <readonly/>\n    </disk>\n{controller}",
            escape_xml(&spec.iso_path)
        )
    };

    Ok(format!(
        "<domain type='kvm'>\n  <name>{}</name>\n  <uuid>{}</uuid>\n  <memory unit='MiB'>{}</memory>\n  <vcpu placement='static'>{}</vcpu>\n  <os firmware='efi'>\n    <type arch='{}' machine='{machine}'>hvm</type>\n  </os>\n{memory_backing}  <cpu mode='{cpu_mode}'/>\n  <features>\n    <acpi/>\n{interrupt_controller}\n  </features>\n  <on_poweroff>destroy</on_poweroff>\n  <on_reboot>restart</on_reboot>\n  <on_crash>destroy</on_crash>\n  <devices>\n    <disk type='file' device='disk'>\n      <driver name='qemu' type='qcow2'/>\n      <source file='{}'/>\n      <target dev='vda' bus='virtio'/>\n    </disk>\n{iso}    <interface type='network'>\n      <mac address='{}'/>\n      <source network='{}'/>\n      <model type='virtio'/>\n    </interface>\n    <console type='pty'>\n      <target type='virtio' port='0'/>\n    </console>\n    <channel type='unix'>\n      <source mode='bind'/>\n      <target type='virtio' name='org.qemu.guest_agent.0'/>\n    </channel>\n    <rng model='virtio'>\n      <backend model='random'>/dev/urandom</backend>\n    </rng>\n    <memballoon model='virtio' freePageReporting='on'/>\n  </devices>\n</domain>\n",
        escape_xml(&spec.name),
        escape_xml(&spec.uuid),
        spec.memory_mib,
        spec.vcpu,
        escape_xml(arch),
        escape_xml(&spec.disk_path),
        escape_xml(&spec.mac),
        escape_xml(&spec.network),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    type MutateSpec = fn(&mut DomainSpec);
    type HostileCase<'a> = (&'a str, MutateSpec, &'a str);

    fn base_spec() -> DomainSpec {
        DomainSpec {
            name: "bento-web".to_string(),
            uuid: "6d1e0f1c-9a3b-4f6e-8a2d-3c5b7e9f1a2b".to_string(),
            vcpu: 2,
            memory_mib: 2048,
            disk_path: "/var/lib/bento/instances/6d1e0f1c.qcow2".to_string(),
            iso_path: String::new(),
            network: "bento-user-1".to_string(),
            mac: "52:54:00:ab:cd:ef".to_string(),
            nested: false,
            ksm: true,
            arch: ARCH_AMD64.to_string(),
        }
    }

    #[test]
    fn domain_xml_matches_every_golden_variant() {
        let variants: [(&str, MutateSpec); 6] = [
            ("default.xml", |_| {}),
            ("nested.xml", |spec| spec.nested = true),
            ("noksm.xml", |spec| spec.ksm = false),
            ("iso.xml", |spec| {
                spec.iso_path = "/var/lib/bento/instances/6d1e0f1c-seed.iso".to_string();
            }),
            ("arm64.xml", |spec| spec.arch = ARCH_ARM64.to_string()),
            ("arm64-iso.xml", |spec| {
                spec.arch = ARCH_ARM64.to_string();
                spec.iso_path = "/var/lib/bento/instances/6d1e0f1c-seed.iso".to_string();
            }),
        ];
        for (golden, mutate) in variants {
            let mut spec = base_spec();
            mutate(&mut spec);
            let actual = domain_xml(&spec).unwrap();
            let expected = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("testdata")
                    .join(golden),
            )
            .unwrap();
            assert_eq!(actual, expected, "XML mismatch with {golden}");
            validate_xml(&actual).unwrap();
        }
    }

    #[test]
    fn domain_xml_has_the_fixed_device_set() {
        let xml = domain_xml(&base_spec()).unwrap();
        for required in [
            "<domain type='kvm'>",
            "<target dev='vda' bus='virtio'/>",
            "<model type='virtio'/>",
            "<mac address='52:54:00:ab:cd:ef'/>",
            "<backend model='random'>/dev/urandom</backend>",
            "<memballoon model='virtio' freePageReporting='on'/>",
            "<console type='pty'>",
            "<target type='virtio' port='0'/>",
            "<target type='virtio' name='org.qemu.guest_agent.0'/>",
            "<os firmware='efi'>",
            "<vcpu placement='static'>2</vcpu>",
            "<cpu mode='host-model'/>",
        ] {
            assert!(xml.contains(required), "XML missing {required}");
        }
        for banned in ["nosharepages", "cdrom", "host-passthrough", "<serial"] {
            assert!(!xml.contains(banned), "default XML contains {banned}");
        }
    }

    #[test]
    fn domain_xml_nested_and_ksm_variants() {
        let mut nested = base_spec();
        nested.nested = true;
        assert!(
            domain_xml(&nested)
                .unwrap()
                .contains("<cpu mode='host-passthrough'/>")
        );

        let mut no_ksm = base_spec();
        no_ksm.ksm = false;
        assert!(domain_xml(&no_ksm).unwrap().contains("<nosharepages/>"));
    }

    #[test]
    fn domain_xml_architecture_variants() {
        let mut native = base_spec();
        native.arch.clear();
        let xml = domain_xml(&native).unwrap();
        assert!(xml.contains(&format!("arch='{}'", host_arch())));

        let mut arm = base_spec();
        arm.arch = ARCH_ARM64.to_string();
        arm.iso_path = "/var/lib/bento/instances/6d1e0f1c-seed.iso".to_string();
        let xml = domain_xml(&arm).unwrap();
        for required in [
            "<type arch='aarch64' machine='virt'>hvm</type>",
            "<cpu mode='host-passthrough'/>",
            "<gic version='3'/>",
            "<target dev='sda' bus='scsi'/>",
            "<controller type='scsi' index='0' model='virtio-scsi'/>",
        ] {
            assert!(xml.contains(required), "aarch64 XML missing {required}");
        }
        for banned in ["<apic/>", "bus='sata'", "machine='q35'"] {
            assert!(!xml.contains(banned), "aarch64 XML contains {banned}");
        }
    }

    #[test]
    fn domain_spec_rejects_an_unknown_architecture() {
        let mut spec = base_spec();
        spec.arch = "riscv64".to_string();
        assert!(spec.validate().is_err());
    }

    #[test]
    fn domain_xml_escapes_every_hostile_value() {
        let cases: [HostileCase<'_>; 4] = [
            (
                "element injection in name",
                |spec| spec.name = "x</name><devices><disk type=\"block\"/></devices>".to_string(),
                "</name><devices>",
            ),
            (
                "attribute breakout in disk path",
                |spec| spec.disk_path = "/tmp/x' bus='0'/><serial type='tcp".to_string(),
                "' bus='0'/>",
            ),
            (
                "double quote and ampersand in network",
                |spec| spec.network = "net\"&<evil>".to_string(),
                "\"&<evil>",
            ),
            (
                "injection in uuid",
                |spec| spec.uuid = "1</uuid><vcpu>999</vcpu>".to_string(),
                "</uuid><vcpu>",
            ),
        ];
        for (name, apply, raw) in cases {
            let mut spec = base_spec();
            apply(&mut spec);
            let xml = domain_xml(&spec).unwrap();
            assert!(
                !xml.contains(raw),
                "{name}: hostile payload survived\n{xml}"
            );
            validate_xml(&xml).unwrap();
        }
    }

    #[test]
    fn domain_xml_escape_round_trip() {
        let mut spec = base_spec();
        spec.name = "a<b>&'\"c\t\n".to_string();
        let xml = domain_xml(&spec).unwrap();
        assert_eq!(domain_identity(&xml).unwrap().0, spec.name);
    }

    #[test]
    fn domain_spec_validation() {
        let cases: [(&str, MutateSpec); 8] = [
            ("empty name", |spec| spec.name.clear()),
            ("empty uuid", |spec| spec.uuid.clear()),
            ("zero vcpu", |spec| spec.vcpu = 0),
            ("zero memory", |spec| spec.memory_mib = 0),
            ("empty disk", |spec| spec.disk_path.clear()),
            ("empty network", |spec| spec.network.clear()),
            ("bad mac", |spec| spec.mac = "not-a-mac".to_string()),
            ("empty mac", |spec| spec.mac.clear()),
        ];
        assert!(domain_xml(&base_spec()).is_ok());
        for (name, mutate) in cases {
            let mut spec = base_spec();
            mutate(&mut spec);
            assert!(domain_xml(&spec).is_err(), "{name} should fail");
        }
    }
}
