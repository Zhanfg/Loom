use core::fmt;
use std::io;

use loom_sys::{add_key_nomount, PageBuffer};

pub const PAYLOAD_LEN: usize = 4096;
pub const BUFFER_LEN: usize = 4068;
pub const NOMOUNT_MAGIC: u64 = 0x004E_4F4D_4F55_4E54;
pub const NOMOUNT_VERSION: &str = "20";

pub const FLAG_IS_DIR: u32 = 1 << 0;
pub const FLAG_VIRTUAL_DIR: u32 = 1 << 1;
pub const FLAG_WHITEOUT: u32 = 1 << 2;

const RULE_HEADER_LEN: usize = 12;
const DEL_HEADER_LEN: usize = 6;
const KERNEL_ENOENT: i32 = -2;
const KERNEL_EEXIST: i32 = -17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum Command {
    GetVersion = 1,
    AddRule = 2,
    DelRule = 3,
    AddUid = 4,
    DelUid = 5,
    GetList = 9,
    GetUids = 10,
}

#[derive(Debug)]
pub enum NomountError {
    Io(io::Error),
    Protocol(String),
}

impl fmt::Display for NomountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "NoMount transport error: {error}"),
            Self::Protocol(detail) => write!(f, "NoMount protocol error: {detail}"),
        }
    }
}

impl std::error::Error for NomountError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for NomountError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NomountRule {
    pub flags: u32,
    pub uid: u32,
    pub virtual_path: String,
    pub real_path: String,
}

impl NomountRule {
    /// Creates a file or directory redirect rule.
    ///
    /// # Errors
    /// Returns `NomountError` when either path is invalid or too large for v20.
    pub fn redirect(
        virtual_path: impl Into<String>,
        real_path: impl Into<String>,
        uid: u32,
        is_dir: bool,
    ) -> Result<Self, NomountError> {
        let rule = Self {
            flags: if is_dir { FLAG_IS_DIR } else { 0 },
            uid,
            virtual_path: virtual_path.into(),
            real_path: real_path.into(),
        };
        rule.validate()?;
        Ok(rule)
    }

    /// Creates a whiteout rule.
    ///
    /// # Errors
    /// Returns `NomountError` when the virtual path is invalid.
    pub fn whiteout(virtual_path: impl Into<String>, uid: u32) -> Result<Self, NomountError> {
        let rule = Self {
            flags: FLAG_WHITEOUT,
            uid,
            virtual_path: virtual_path.into(),
            real_path: String::new(),
        };
        rule.validate()?;
        Ok(rule)
    }

    /// Creates a source-less virtual directory rule.
    ///
    /// # Errors
    /// Returns `NomountError` when the virtual path is invalid.
    pub fn virtual_dir(virtual_path: impl Into<String>, uid: u32) -> Result<Self, NomountError> {
        let rule = Self {
            flags: FLAG_IS_DIR | FLAG_VIRTUAL_DIR,
            uid,
            virtual_path: virtual_path.into(),
            real_path: String::new(),
        };
        rule.validate()?;
        Ok(rule)
    }

    /// Validates one rule against the `NoMount` v20 wire format.
    ///
    /// # Errors
    /// Returns `NomountError` for invalid paths, flags, or oversized records.
    pub fn validate(&self) -> Result<(), NomountError> {
        validate_absolute_path(&self.virtual_path, "virtual path")?;
        if self.flags & FLAG_WHITEOUT != 0 || self.flags & FLAG_VIRTUAL_DIR != 0 {
            if !self.real_path.is_empty() {
                return Err(NomountError::Protocol(
                    "source-less rules must not carry a real path".to_owned(),
                ));
            }
        } else {
            validate_absolute_path(&self.real_path, "real path")?;
        }
        let allowed = FLAG_IS_DIR | FLAG_VIRTUAL_DIR | FLAG_WHITEOUT;
        if self.flags & !allowed != 0 {
            return Err(NomountError::Protocol(format!(
                "unsupported NoMount flags {:#x}",
                self.flags
            )));
        }
        if self.flags & FLAG_VIRTUAL_DIR != 0 && self.flags & FLAG_IS_DIR == 0 {
            return Err(NomountError::Protocol(
                "virtual directory flag requires directory flag".to_owned(),
            ));
        }
        if self.record_len() > BUFFER_LEN {
            return Err(NomountError::Protocol(format!(
                "rule record needs {} bytes, maximum is {BUFFER_LEN}",
                self.record_len()
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn record_len(&self) -> usize {
        RULE_HEADER_LEN + self.virtual_path.len() + self.real_path.len()
    }

    fn write_record(&self, out: &mut Vec<u8>) -> Result<(), NomountError> {
        self.validate()?;
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.uid.to_le_bytes());
        let virtual_len = u16::try_from(self.virtual_path.len())
            .map_err(|_| NomountError::Protocol("virtual path length exceeds u16".to_owned()))?;
        let real_len = u16::try_from(self.real_path.len())
            .map_err(|_| NomountError::Protocol("real path length exceeds u16".to_owned()))?;
        out.extend_from_slice(&virtual_len.to_le_bytes());
        out.extend_from_slice(&real_len.to_le_bytes());
        out.extend_from_slice(self.virtual_path.as_bytes());
        out.extend_from_slice(self.real_path.as_bytes());
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedRule {
    pub flags: u32,
    pub uid: u32,
    pub virtual_path: String,
    pub real_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedRule {
    pub rule: NomountRule,
}

pub trait NomountTransport {
    /// Exchanges one complete `NoMount` page.
    ///
    /// # Errors
    /// Returns `NomountError` when delivery or response retrieval fails.
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, NomountError>;
}

pub struct SystemNomountTransport {
    page: PageBuffer,
}

impl SystemNomountTransport {
    /// Creates the Linux/Android keyring transport.
    ///
    /// # Errors
    /// Returns `NomountError` when the payload page cannot be allocated.
    pub fn new() -> Result<Self, NomountError> {
        Ok(Self {
            page: PageBuffer::new(PAYLOAD_LEN)?,
        })
    }
}

impl NomountTransport for SystemNomountTransport {
    fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, NomountError> {
        if request.len() != PAYLOAD_LEN {
            return Err(NomountError::Protocol(format!(
                "payload is {} bytes, expected {PAYLOAD_LEN}",
                request.len()
            )));
        }
        self.page.as_mut_slice().copy_from_slice(request);
        add_key_nomount(&mut self.page)?;
        Ok(self.page.as_mut_slice().to_vec())
    }
}

pub struct NomountClient<T> {
    transport: T,
}

impl<T: NomountTransport> NomountClient<T> {
    #[must_use]
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.transport
    }

    /// Reads the kernel-side `NoMount` protocol version.
    ///
    /// # Errors
    /// Returns `NomountError` for transport or malformed-response failures.
    pub fn version(&mut self) -> Result<String, NomountError> {
        let request = build_payload(Command::GetVersion, 0, &[])?;
        parse_version(&self.transport.exchange(&request)?)
    }

    /// Adds one or more path rules.
    ///
    /// # Errors
    /// Returns `NomountError` when encoding, delivery, or kernel consumption fails.
    pub fn add_rules(&mut self, rules: &[NomountRule]) -> Result<(), NomountError> {
        for payload in build_add_rule_payloads(rules)? {
            ensure_consumed(&self.transport.exchange(&payload)?)?;
        }
        Ok(())
    }

    /// Deletes exact virtual paths. Missing rules are idempotent.
    ///
    /// # Errors
    /// Returns `NomountError` for transport failures or malformed responses.
    pub fn remove_rules(&mut self, rules: &[NomountRule]) -> Result<(), NomountError> {
        for payload in build_del_rule_payloads(rules)? {
            let response = self.transport.exchange(&payload)?;
            ensure_status_allowing(&response, &[KERNEL_ENOENT])?;
            ensure_full_cursor(&response)?;
        }
        Ok(())
    }

    /// Adds one isolated UID. Existing entries are idempotent.
    ///
    /// # Errors
    /// Returns `NomountError` for transport or kernel failures.
    pub fn add_uid(&mut self, uid: u32) -> Result<(), NomountError> {
        let request = build_payload(Command::AddUid, uid, &[])?;
        let response = self.transport.exchange(&request)?;
        ensure_status_allowing(&response, &[KERNEL_EEXIST])
    }

    /// Removes one isolated UID. Missing entries are idempotent.
    ///
    /// # Errors
    /// Returns `NomountError` for transport or kernel failures.
    pub fn remove_uid(&mut self, uid: u32) -> Result<(), NomountError> {
        let request = build_payload(Command::DelUid, uid, &[])?;
        let response = self.transport.exchange(&request)?;
        ensure_status_allowing(&response, &[KERNEL_ENOENT])
    }

    /// Lists every installed rule through `GET_LIST` pagination.
    ///
    /// # Errors
    /// Returns `NomountError` for transport, pagination, or decoding failures.
    pub fn list_rules(&mut self) -> Result<Vec<ListedRule>, NomountError> {
        let mut cursor = 0_u32;
        let mut all = Vec::new();
        loop {
            let request = build_list_payload(Command::GetList, cursor)?;
            let response = self.transport.exchange(&request)?;
            let (mut batch, next) = parse_list(&response)?;
            if batch.is_empty() {
                return Ok(all);
            }
            if next <= cursor {
                return Err(NomountError::Protocol(format!(
                    "rule listing cursor did not advance: {cursor} -> {next}"
                )));
            }
            all.append(&mut batch);
            cursor = next;
        }
    }

    /// Lists every isolated UID through `GET_UIDS` pagination.
    ///
    /// # Errors
    /// Returns `NomountError` for transport, pagination, or decoding failures.
    pub fn list_uids(&mut self) -> Result<Vec<u32>, NomountError> {
        let mut cursor = 0_u32;
        let mut all = Vec::new();
        loop {
            let request = build_list_payload(Command::GetUids, cursor)?;
            let response = self.transport.exchange(&request)?;
            let (mut batch, next) = parse_uids(&response)?;
            if batch.is_empty() {
                return Ok(all);
            }
            if next <= cursor {
                return Err(NomountError::Protocol(format!(
                    "UID listing cursor did not advance: {cursor} -> {next}"
                )));
            }
            all.append(&mut batch);
            cursor = next;
        }
    }

    /// Applies one rule and returns a rollback token.
    ///
    /// # Errors
    /// Returns `NomountError` when the rule cannot be installed.
    pub fn apply(&mut self, rule: &NomountRule) -> Result<AppliedRule, NomountError> {
        self.add_rules(std::slice::from_ref(rule))?;
        Ok(AppliedRule { rule: rule.clone() })
    }

    /// Verifies one applied rule through `GET_LIST`.
    ///
    /// # Errors
    /// Returns `NomountError` if read-back does not exactly match the expected rule.
    pub fn verify(&mut self, token: &AppliedRule) -> Result<(), NomountError> {
        let expected = &token.rule;
        if self.list_rules()?.iter().any(|rule| {
            rule.flags == expected.flags
                && rule.uid == expected.uid
                && rule.virtual_path == expected.virtual_path
                && rule.real_path == expected.real_path
        }) {
            return Ok(());
        }
        Err(NomountError::Protocol(format!(
            "rule {} is absent from GET_LIST after apply",
            expected.virtual_path
        )))
    }

    /// Rolls back one applied rule.
    ///
    /// # Errors
    /// Returns `NomountError` when the kernel cannot remove the rule.
    pub fn rollback(&mut self, token: AppliedRule) -> Result<(), NomountError> {
        self.remove_rules(&[token.rule])
    }
}

fn validate_absolute_path(path: &str, label: &str) -> Result<(), NomountError> {
    if !path.starts_with('/') {
        return Err(NomountError::Protocol(format!(
            "{label} must be absolute: {path:?}"
        )));
    }
    if path.as_bytes().contains(&0) {
        return Err(NomountError::Protocol(format!("{label} contains NUL")));
    }
    if path.len() > usize::from(u16::MAX) {
        return Err(NomountError::Protocol(format!(
            "{label} is too long for the v20 wire format"
        )));
    }
    Ok(())
}

fn build_payload(
    command: Command,
    target_uid: u32,
    buffer: &[u8],
) -> Result<Vec<u8>, NomountError> {
    if buffer.len() > BUFFER_LEN {
        return Err(NomountError::Protocol(format!(
            "payload body is {} bytes, maximum is {BUFFER_LEN}",
            buffer.len()
        )));
    }
    let mut page = vec![0_u8; PAYLOAD_LEN];
    page[0..8].copy_from_slice(&NOMOUNT_MAGIC.to_le_bytes());
    page[8..12].copy_from_slice(&(command as u32).to_le_bytes());
    page[12..16].copy_from_slice(&target_uid.to_le_bytes());
    page[16..20].copy_from_slice(&(-1_i32).to_le_bytes());
    page[24..28].copy_from_slice(
        &u32::try_from(buffer.len())
            .map_err(|_| NomountError::Protocol("payload length exceeds u32".to_owned()))?
            .to_le_bytes(),
    );
    page[28..28 + buffer.len()].copy_from_slice(buffer);
    Ok(page)
}

fn build_add_rule_payloads(rules: &[NomountRule]) -> Result<Vec<Vec<u8>>, NomountError> {
    let mut payloads = Vec::new();
    let mut body = Vec::new();
    for rule in rules {
        rule.validate()?;
        if body.len() + rule.record_len() > BUFFER_LEN {
            payloads.push(build_payload(Command::AddRule, 0, &body)?);
            body.clear();
        }
        rule.write_record(&mut body)?;
    }
    if !body.is_empty() {
        payloads.push(build_payload(Command::AddRule, 0, &body)?);
    }
    Ok(payloads)
}

fn build_del_rule_payloads(rules: &[NomountRule]) -> Result<Vec<Vec<u8>>, NomountError> {
    let mut payloads = Vec::new();
    let mut body = Vec::new();
    for rule in rules {
        validate_absolute_path(&rule.virtual_path, "virtual path")?;
        let len = u16::try_from(rule.virtual_path.len())
            .map_err(|_| NomountError::Protocol("virtual path length exceeds u16".to_owned()))?;
        let record_len = DEL_HEADER_LEN + rule.virtual_path.len();
        if body.len() + record_len > BUFFER_LEN && !body.is_empty() {
            payloads.push(build_payload(Command::DelRule, 0, &body)?);
            body.clear();
        }
        body.extend_from_slice(&rule.uid.to_le_bytes());
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(rule.virtual_path.as_bytes());
    }
    if !body.is_empty() {
        payloads.push(build_payload(Command::DelRule, 0, &body)?);
    }
    Ok(payloads)
}

fn build_list_payload(command: Command, cursor: u32) -> Result<Vec<u8>, NomountError> {
    let mut page = build_payload(command, 0, &[])?;
    page[20..24].copy_from_slice(&cursor.to_le_bytes());
    Ok(page)
}

fn field<const N: usize>(
    payload: &[u8],
    offset: usize,
    label: &str,
) -> Result<[u8; N], NomountError> {
    payload
        .get(offset..offset + N)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            NomountError::Protocol(format!(
                "response is too short for {label} at offset {offset}"
            ))
        })
}

fn status(payload: &[u8]) -> Result<i32, NomountError> {
    Ok(i32::from_le_bytes(field(payload, 16, "status")?))
}

fn arg1(payload: &[u8]) -> Result<u32, NomountError> {
    Ok(u32::from_le_bytes(field(payload, 20, "arg1")?))
}

fn data_size(payload: &[u8]) -> Result<u32, NomountError> {
    Ok(u32::from_le_bytes(field(payload, 24, "data_size")?))
}

fn ensure_status_allowing(payload: &[u8], allowed: &[i32]) -> Result<(), NomountError> {
    let value = status(payload)?;
    if value < 0 && !allowed.contains(&value) {
        return Err(NomountError::Protocol(format!(
            "kernel returned status {value}"
        )));
    }
    Ok(())
}

fn ensure_full_cursor(payload: &[u8]) -> Result<(), NomountError> {
    let consumed = arg1(payload)?;
    let total = data_size(payload)?;
    if consumed != total {
        return Err(NomountError::Protocol(format!(
            "kernel consumed {consumed} of {total} bytes"
        )));
    }
    Ok(())
}

fn ensure_consumed(payload: &[u8]) -> Result<(), NomountError> {
    let value = status(payload)?;
    if value < 0 {
        return Err(NomountError::Protocol(format!(
            "kernel returned status {value} at offset {}",
            arg1(payload)?
        )));
    }
    ensure_full_cursor(payload)
}

fn response_body(payload: &[u8]) -> Result<&[u8], NomountError> {
    let len = usize::try_from(data_size(payload)?)
        .map_err(|_| NomountError::Protocol("response size cannot fit usize".to_owned()))?;
    if len > BUFFER_LEN {
        return Err(NomountError::Protocol(format!(
            "response claims {len} bytes, maximum is {BUFFER_LEN}"
        )));
    }
    payload.get(28..28 + len).ok_or_else(|| {
        NomountError::Protocol(format!(
            "response is too short for declared {len}-byte body"
        ))
    })
}

fn parse_version(payload: &[u8]) -> Result<String, NomountError> {
    ensure_status_allowing(payload, &[])?;
    let bytes = response_body(payload)?;
    let text = std::str::from_utf8(bytes)
        .map_err(|_| NomountError::Protocol("version is not valid UTF-8".to_owned()))?;
    Ok(text.trim().to_owned())
}

fn parse_list(payload: &[u8]) -> Result<(Vec<ListedRule>, u32), NomountError> {
    ensure_status_allowing(payload, &[])?;
    let body = response_body(payload)?;
    let mut rules = Vec::new();
    let mut pos = 0_usize;
    while pos < body.len() {
        let header = body.get(pos..pos + RULE_HEADER_LEN).ok_or_else(|| {
            NomountError::Protocol(format!("truncated rule header at offset {pos}"))
        })?;
        let flags = u32::from_le_bytes(field(header, 0, "flags")?);
        let uid = u32::from_le_bytes(field(header, 4, "uid")?);
        let virtual_len = usize::from(u16::from_le_bytes(field(header, 8, "virtual length")?));
        let real_len = usize::from(u16::from_le_bytes(field(header, 10, "real length")?));
        pos += RULE_HEADER_LEN;
        let path_len = virtual_len
            .checked_add(real_len)
            .ok_or_else(|| NomountError::Protocol("rule path length overflow".to_owned()))?;
        let paths = body.get(pos..pos + path_len).ok_or_else(|| {
            NomountError::Protocol(format!("rule at offset {pos} exceeds response body"))
        })?;
        let (virtual_path, real_path) = paths.split_at(virtual_len);
        rules.push(ListedRule {
            flags,
            uid,
            virtual_path: String::from_utf8_lossy(virtual_path).into_owned(),
            real_path: String::from_utf8_lossy(real_path).into_owned(),
        });
        pos += path_len;
    }
    Ok((rules, arg1(payload)?))
}

fn parse_uids(payload: &[u8]) -> Result<(Vec<u32>, u32), NomountError> {
    ensure_status_allowing(payload, &[])?;
    let body = response_body(payload)?;
    if body.len() % 4 != 0 {
        return Err(NomountError::Protocol(format!(
            "UID response length {} is not divisible by four",
            body.len()
        )));
    }
    let mut uids = Vec::with_capacity(body.len() / 4);
    for chunk in body.chunks_exact(4) {
        let word: [u8; 4] = chunk
            .try_into()
            .map_err(|_| NomountError::Protocol("invalid UID word".to_owned()))?;
        uids.push(u32::from_le_bytes(word));
    }
    Ok((uids, arg1(payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        requests: Vec<Vec<u8>>,
        responses: Vec<Vec<u8>>,
    }

    impl NomountTransport for FakeTransport {
        fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, NomountError> {
            self.requests.push(request.to_vec());
            if self.responses.is_empty() {
                return Err(NomountError::Protocol("missing fake response".to_owned()));
            }
            Ok(self.responses.remove(0))
        }
    }

    fn response(command: Command, status_value: i32, cursor: u32, body: &[u8]) -> Vec<u8> {
        let mut page = build_payload(command, 0, body).unwrap();
        page[16..20].copy_from_slice(&status_value.to_le_bytes());
        page[20..24].copy_from_slice(&cursor.to_le_bytes());
        page
    }

    #[test]
    fn redirect_rule_encodes_v20_header() {
        let rule = NomountRule::redirect("/system/etc/a", "/data/adb/a", 0, false).unwrap();
        let payloads = build_add_rule_payloads(&[rule]).unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            u64::from_le_bytes(payloads[0][0..8].try_into().unwrap()),
            NOMOUNT_MAGIC
        );
    }

    #[test]
    fn version_response_is_decoded() {
        let page = response(Command::GetVersion, 0, 0, NOMOUNT_VERSION.as_bytes());
        assert_eq!(parse_version(&page).unwrap(), NOMOUNT_VERSION);
    }

    #[test]
    fn add_rule_requires_full_kernel_cursor() {
        let rule = NomountRule::redirect("/system/etc/a", "/data/adb/a", 0, false).unwrap();
        let request = build_add_rule_payloads(std::slice::from_ref(&rule))
            .unwrap()
            .remove(0);
        let body_len = u32::from_le_bytes(request[24..28].try_into().unwrap());
        let mut ok = request.clone();
        ok[16..20].copy_from_slice(&0_i32.to_le_bytes());
        ok[20..24].copy_from_slice(&body_len.to_le_bytes());
        assert!(ensure_consumed(&ok).is_ok());
        ok[20..24].copy_from_slice(&0_u32.to_le_bytes());
        assert!(ensure_consumed(&ok).is_err());
    }

    #[test]
    fn verify_uses_get_list_read_back() {
        let rule = NomountRule::redirect("/system/etc/a", "/data/adb/a", 0, false).unwrap();
        let mut body = Vec::new();
        rule.write_record(&mut body).unwrap();
        let transport = FakeTransport {
            requests: Vec::new(),
            responses: vec![
                response(Command::GetList, 0, 1, &body),
                response(Command::GetList, 0, 1, &[]),
            ],
        };
        let mut client = NomountClient::new(transport);
        client.verify(&AppliedRule { rule: rule.clone() }).unwrap();
        assert_eq!(client.into_inner().requests.len(), 2);
    }
}
