#![forbid(unsafe_code)]

#[path = "big_index.rs"]
mod big_index;

const MIN_MATCH: usize = 4;
const LAST_LITERALS: usize = 5;
const MFLIMIT: usize = 12;
const HASH_LOG: u32 = 16;
const HASH_SIZE: usize = 1 << HASH_LOG;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodecError {
    Overflow,
    InvalidBlock,
}

pub(crate) fn encode(input: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut output = Vec::with_capacity(input.len());
    let mut table = vec![usize::MAX; HASH_SIZE];
    let mut anchor = 0_usize;
    let mut cursor = 0_usize;
    let last_match_start = input.len().saturating_sub(MFLIMIT);
    let match_end = input.len().saturating_sub(LAST_LITERALS);

    while cursor <= last_match_start && cursor + MIN_MATCH <= input.len() {
        let hash = hash4(input, cursor)?;
        let candidate = table[hash];
        table[hash] = cursor;
        let valid = candidate != usize::MAX
            && cursor > candidate
            && cursor - candidate <= usize::from(u16::MAX)
            && input[candidate..candidate + MIN_MATCH] == input[cursor..cursor + MIN_MATCH];
        if !valid {
            cursor += 1;
            continue;
        }

        let mut match_len = MIN_MATCH;
        while cursor + match_len < match_end
            && input[candidate + match_len] == input[cursor + match_len]
        {
            match_len += 1;
        }
        emit_sequence(
            &mut output,
            input,
            anchor,
            cursor,
            candidate,
            match_len,
        )?;

        let next = cursor.checked_add(match_len).ok_or(CodecError::Overflow)?;
        let mut update = cursor + 1;
        while update < next && update <= last_match_start {
            table[hash4(input, update)?] = update;
            update += 1;
        }
        cursor = next;
        anchor = next;
    }

    emit_last_literals(&mut output, &input[anchor..])?;
    Ok(output)
}

pub(crate) fn decode(encoded: &[u8], expected: usize) -> Result<Vec<u8>, CodecError> {
    let mut input_pos = 0_usize;
    let mut output = Vec::with_capacity(expected);
    while input_pos < encoded.len() {
        let token = encoded[input_pos];
        input_pos += 1;

        let mut literal_len = usize::from(token >> 4);
        if literal_len == 15 {
            literal_len = literal_len
                .checked_add(read_length(encoded, &mut input_pos)?)
                .ok_or(CodecError::Overflow)?;
        }
        let literal_end = input_pos
            .checked_add(literal_len)
            .ok_or(CodecError::Overflow)?;
        let literals = encoded
            .get(input_pos..literal_end)
            .ok_or(CodecError::InvalidBlock)?;
        if output.len().saturating_add(literals.len()) > expected {
            return Err(CodecError::InvalidBlock);
        }
        output.extend_from_slice(literals);
        input_pos = literal_end;
        if input_pos == encoded.len() {
            break;
        }

        let offset_end = input_pos.checked_add(2).ok_or(CodecError::Overflow)?;
        let raw_offset: [u8; 2] = encoded
            .get(input_pos..offset_end)
            .ok_or(CodecError::InvalidBlock)?
            .try_into()
            .map_err(|_| CodecError::InvalidBlock)?;
        input_pos = offset_end;
        let offset = usize::from(u16::from_le_bytes(raw_offset));
        if offset == 0 || offset > output.len() {
            return Err(CodecError::InvalidBlock);
        }

        let mut match_len = usize::from(token & 0x0f) + MIN_MATCH;
        if token & 0x0f == 15 {
            match_len = match_len
                .checked_add(read_length(encoded, &mut input_pos)?)
                .ok_or(CodecError::Overflow)?;
        }
        if output.len().saturating_add(match_len) > expected {
            return Err(CodecError::InvalidBlock);
        }
        for _ in 0..match_len {
            let source = output.len().checked_sub(offset).ok_or(CodecError::InvalidBlock)?;
            let byte = *output.get(source).ok_or(CodecError::InvalidBlock)?;
            output.push(byte);
        }
    }
    if output.len() != expected {
        return Err(CodecError::InvalidBlock);
    }
    Ok(output)
}

pub(crate) fn decode_0padding(pcluster: &[u8], expected: usize) -> Result<Vec<u8>, CodecError> {
    let start = pcluster
        .iter()
        .position(|byte| *byte != 0)
        .ok_or(CodecError::InvalidBlock)?;
    decode(&pcluster[start..], expected)
}

fn hash4(input: &[u8], offset: usize) -> Result<usize, CodecError> {
    let end = offset.checked_add(MIN_MATCH).ok_or(CodecError::Overflow)?;
    let bytes: [u8; 4] = input
        .get(offset..end)
        .ok_or(CodecError::InvalidBlock)?
        .try_into()
        .map_err(|_| CodecError::InvalidBlock)?;
    let value = u32::from_le_bytes(bytes).wrapping_mul(2_654_435_761);
    usize::try_from(value >> (32 - HASH_LOG)).map_err(|_| CodecError::Overflow)
}

fn emit_sequence(
    output: &mut Vec<u8>,
    input: &[u8],
    anchor: usize,
    match_start: usize,
    match_ref: usize,
    match_len: usize,
) -> Result<(), CodecError> {
    let literal_len = match_start.checked_sub(anchor).ok_or(CodecError::Overflow)?;
    let match_code = match_len.checked_sub(MIN_MATCH).ok_or(CodecError::Overflow)?;
    let token = u8::try_from((literal_len.min(15) << 4) | match_code.min(15))
        .map_err(|_| CodecError::Overflow)?;
    output.push(token);
    if literal_len >= 15 {
        emit_length(output, literal_len - 15)?;
    }
    output.extend_from_slice(
        input
            .get(anchor..match_start)
            .ok_or(CodecError::InvalidBlock)?,
    );

    let offset = match_start.checked_sub(match_ref).ok_or(CodecError::Overflow)?;
    let offset = u16::try_from(offset).map_err(|_| CodecError::InvalidBlock)?;
    if offset == 0 {
        return Err(CodecError::InvalidBlock);
    }
    output.extend_from_slice(&offset.to_le_bytes());
    if match_code >= 15 {
        emit_length(output, match_code - 15)?;
    }
    Ok(())
}

fn emit_last_literals(output: &mut Vec<u8>, literals: &[u8]) -> Result<(), CodecError> {
    output.push(u8::try_from(literals.len().min(15) << 4).map_err(|_| CodecError::Overflow)?);
    if literals.len() >= 15 {
        emit_length(output, literals.len() - 15)?;
    }
    output.extend_from_slice(literals);
    Ok(())
}

fn emit_length(output: &mut Vec<u8>, mut length: usize) -> Result<(), CodecError> {
    while length >= 255 {
        output.push(255);
        length -= 255;
    }
    output.push(u8::try_from(length).map_err(|_| CodecError::Overflow)?);
    Ok(())
}

fn read_length(encoded: &[u8], input_pos: &mut usize) -> Result<usize, CodecError> {
    let mut total = 0_usize;
    loop {
        let byte = *encoded.get(*input_pos).ok_or(CodecError::InvalidBlock)?;
        *input_pos = (*input_pos).checked_add(1).ok_or(CodecError::Overflow)?;
        total = total
            .checked_add(usize::from(byte))
            .ok_or(CodecError::Overflow)?;
        if byte != 255 {
            return Ok(total);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_0padding_round_trip() {
        let mut input = vec![b'M'; 32768];
        input[64..88].copy_from_slice(b"LOOM-MULTI-LZ4-ROUNDTRIP");
        let encoded = encode(&input).unwrap();
        assert!(!encoded.is_empty());
        assert_ne!(encoded[0], 0);
        assert!(encoded.len() < 4096);
        assert_eq!(decode(&encoded, input.len()).unwrap(), input);

        let mut block = vec![0_u8; 4096];
        let start = block.len() - encoded.len();
        block[start..].copy_from_slice(&encoded);
        assert_eq!(decode_0padding(&block, input.len()).unwrap(), input);
    }

    #[test]
    fn random_payload_exceeds_one_block() {
        let mut state = 0x5354_4147_u32;
        let mut input = vec![0_u8; 32768];
        for byte in &mut input {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let encoded = encode(&input).unwrap();
        assert!(encoded.len() > 4096);
        assert_eq!(decode(&encoded, input.len()).unwrap(), input);
    }
}
