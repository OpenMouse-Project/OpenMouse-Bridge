use std::io::{Read, Seek, SeekFrom};

pub(super) fn validate(image: &mut (impl Read + Seek), length: u64) -> bool {
    validate_image(image, length).is_some()
}

fn read_at(image: &mut (impl Read + Seek), offset: u64, count: usize) -> Option<Vec<u8>> {
    image.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = vec![0; count];
    image.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn word(bytes: &[u8], offset: usize, little: bool) -> Option<u32> {
    let bytes = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
    })
}

fn wide(bytes: &[u8], offset: usize, little: bool) -> Option<u64> {
    let bytes = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(if little {
        u64::from_le_bytes(bytes)
    } else {
        u64::from_be_bytes(bytes)
    })
}

fn fits(offset: u64, size: u64, length: u64) -> Option<()> {
    (offset.checked_add(size)? <= length).then_some(())
}

fn validate_image(image: &mut (impl Read + Seek), length: u64) -> Option<()> {
    fits(0, 8, length)?;
    let header = read_at(image, 0, 8)?;
    let magic = word(&header, 0, false)?;
    if !matches!(magic, 0xcafebabe | 0xbebafeca | 0xcafebabf | 0xbfbafeca) {
        return thin(image, 0, length, None);
    }
    let little = matches!(magic, 0xbebafeca | 0xbfbafeca);
    let fat64 = matches!(magic, 0xcafebabf | 0xbfbafeca);
    let count = word(&header, 4, little)? as usize;
    if !(1..=16).contains(&count) {
        return None;
    }
    let stride = if fat64 { 32 } else { 20 };
    let table_end = 8 + count * stride;
    fits(0, table_end as u64, length)?;
    let table = read_at(image, 8, count * stride)?;
    let mut ranges = Vec::new();
    let mut cpus = Vec::new();
    for arch in table.chunks_exact(stride) {
        let cpu = word(arch, 0, little)?;
        let subtype = word(arch, 4, little)?;
        let (offset, size, align) = if fat64 {
            if word(arch, 28, little)? != 0 {
                return None;
            }
            (
                wide(arch, 8, little)?,
                wide(arch, 16, little)?,
                word(arch, 24, little)?,
            )
        } else {
            (
                word(arch, 8, little)? as u64,
                word(arch, 12, little)? as u64,
                word(arch, 16, little)?,
            )
        };
        if align > 31
            || offset < table_end as u64
            || !offset.is_multiple_of(1_u64 << align)
            || size == 0
            || cpus.contains(&(cpu, subtype))
        {
            return None;
        }
        fits(offset, size, length)?;
        let end = offset.checked_add(size)?;
        if ranges
            .iter()
            .any(|&(start, stop)| offset < stop && end > start)
        {
            return None;
        }
        ranges.push((offset, end));
        cpus.push((cpu, subtype));
        thin(image, offset, size, Some((cpu, subtype)))?;
    }
    Some(())
}

fn thin(
    image: &mut (impl Read + Seek),
    base: u64,
    length: u64,
    arch: Option<(u32, u32)>,
) -> Option<()> {
    fits(0, 32, length)?;
    let header = read_at(image, base, 32)?;
    let little = match word(&header, 0, false)? {
        0xfeedfacf => false,
        0xcffaedfe => true,
        _ => return None,
    };
    let cpu = word(&header, 4, little)?;
    let subtype = word(&header, 8, little)?;
    if !matches!(cpu, 0x01000007 | 0x0100000c)
        || if cpu == 0x01000007 {
            !matches!(subtype & 0x00ffffff, 3 | 8)
        } else {
            !matches!(subtype & 0x00ffffff, 0..=2)
        }
        || arch.is_some_and(|value| value != (cpu, subtype))
        || word(&header, 12, little)? != 2
        || word(&header, 28, little)? != 0
    {
        return None;
    }
    let count = word(&header, 16, little)? as usize;
    let size = word(&header, 20, little)? as usize;
    if count == 0 || count > 16384 || size > 1024 * 1024 || count > size / 8 {
        return None;
    }
    fits(32, size as u64, length)?;
    let commands = read_at(image, base.checked_add(32)?, size)?;
    let mut cursor: usize = 0;
    let mut executable_ranges = Vec::new();
    let mut entry = None;
    for _ in 0..count {
        let cmd = word(&commands, cursor, little)?;
        let size = word(&commands, cursor.checked_add(4)?, little)? as usize;
        if size < 8 || !size.is_multiple_of(8) {
            return None;
        }
        let data = commands.get(cursor..cursor.checked_add(size)?)?;
        match cmd {
            0x19 => {
                let offset = wide(data, 40, little)?;
                let size = wide(data, 48, little)?;
                let vmaddr = wide(data, 24, little)?;
                let vmsize = wide(data, 32, little)?;
                vmaddr.checked_add(vmsize)?;
                fits(offset, size, length)?;
                if size > vmsize {
                    return None;
                }
                let maxprot = word(data, 56, little)?;
                let initprot = word(data, 60, little)?;
                if initprot & !maxprot != 0 || maxprot & !7 != 0 {
                    return None;
                }
                if initprot & 4 != 0 && size > 0 {
                    executable_ranges.push((offset, offset.checked_add(size)?));
                }
                let sections = word(data, 64, little)? as usize;
                if data.len() != 72_usize.checked_add(sections.checked_mul(80)?)? {
                    return None;
                }
                for section in data[72..].as_chunks::<80>().0 {
                    let size = wide(section, 40, little)?;
                    fits(
                        wide(section, 32, little)?.checked_sub(vmaddr)?,
                        size,
                        vmsize,
                    )?;
                    if word(section, 52, little)? > 31 {
                        return None;
                    }
                    let start = word(section, 48, little)? as u64;
                    let flags = word(section, 64, little)? & 0xff;
                    if !matches!(flags, 1 | 0xc | 0x12) {
                        fits(start, size, length)?;
                        if size > 0
                            && (start < offset
                                || start.checked_add(size)?
                                    > offset.checked_add(wide(data, 48, little)?)?)
                        {
                            return None;
                        }
                    }
                    fits(
                        word(section, 56, little)? as u64,
                        (word(section, 60, little)? as u64).checked_mul(8)?,
                        length,
                    )?;
                }
            }
            0x2 => {
                if data.len() != 24 {
                    return None;
                }
                fits(
                    word(data, 8, little)? as u64,
                    (word(data, 12, little)? as u64).checked_mul(16)?,
                    length,
                )?;
                fits(
                    word(data, 16, little)? as u64,
                    word(data, 20, little)? as u64,
                    length,
                )?;
            }
            0xb => {
                if data.len() != 80 {
                    return None;
                }
                for (field, stride) in [(32, 8), (40, 56), (48, 4), (56, 4), (64, 8), (72, 8)] {
                    fits(
                        word(data, field, little)? as u64,
                        (word(data, field + 4, little)? as u64).checked_mul(stride)?,
                        length,
                    )?;
                }
            }
            0x22 | 0x80000022 => {
                if data.len() != 48 {
                    return None;
                }
                for field in (8..48).step_by(8) {
                    fits(
                        word(data, field, little)? as u64,
                        word(data, field + 4, little)? as u64,
                        length,
                    )?;
                }
            }
            0x1d | 0x1e | 0x26 | 0x29 | 0x2b | 0x2e | 0x80000033 | 0x80000034 | 0x36 | 0x37
            | 0x38 => {
                if data.len() != 16 {
                    return None;
                }
                fits(
                    word(data, 8, little)? as u64,
                    word(data, 12, little)? as u64,
                    length,
                )?;
            }
            0x16 => {
                if data.len() != 16 {
                    return None;
                }
                fits(
                    word(data, 8, little)? as u64,
                    (word(data, 12, little)? as u64).checked_mul(4)?,
                    length,
                )?;
            }
            0x31 => {
                if data.len() != 40 {
                    return None;
                }
                fits(wide(data, 24, little)?, wide(data, 32, little)?, length)?;
            }
            0x2c => {
                if data.len() != 24 || word(data, 20, little)? != 0 {
                    return None;
                }
                fits(
                    word(data, 8, little)? as u64,
                    word(data, 12, little)? as u64,
                    length,
                )?;
            }
            0xc | 0x80000018 | 0x8000001f | 0x20 | 0x80000023 => {
                if data.len() < 24 {
                    return None;
                }
                let offset = word(data, 8, little)? as usize;
                if offset < 24 || !data.get(offset..)?.contains(&0) {
                    return None;
                }
            }
            0xe | 0x8000001c | 0x27 => {
                let offset = word(data, 8, little)? as usize;
                if offset < 12 || !data.get(offset..)?.contains(&0) {
                    return None;
                }
            }
            0x32 => {
                let platform = word(data, 8, little)?;
                if !matches!(platform, 1 | 6)
                    || data.len()
                        != 24_usize
                            .checked_add((word(data, 20, little)? as usize).checked_mul(8)?)?
                {
                    return None;
                }
            }
            0x1 | 0x3 | 0x21 | 0x25 | 0x2f | 0x30 | 0x80000035 => return None,
            0x80000028 => {
                if data.len() != 24 || entry.is_some() {
                    return None;
                }
                entry = Some(wide(data, 8, little)?);
            }
            0x1b => {
                if data.len() != 24 {
                    return None;
                }
            }
            0x24 | 0x2a => {
                if data.len() != 16 {
                    return None;
                }
            }
            _ => return None,
        }
        cursor = cursor.checked_add(size)?;
    }
    let entry = entry?;
    (cursor == commands.len()
        && executable_ranges
            .iter()
            .any(|&(start, stop)| entry >= start && entry < stop))
    .then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn structural_image() -> Vec<u8> {
        let mut bytes = vec![0; 132];
        for (offset, value) in [
            (0, 0xfeedfacf_u32),
            (4, 0x0100000c),
            (12, 2),
            (16, 2),
            (20, 96),
            (32, 0x19),
            (36, 72),
            (88, 7),
            (92, 5),
            (104, 0x80000028),
            (108, 24),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [(64, 4096_u64), (80, 132), (112, 128)] {
            bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        bytes[128..132].copy_from_slice(&[0xc0, 0x03, 0x5f, 0xd6]);
        bytes
    }

    fn accepted(bytes: &[u8]) -> bool {
        validate(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn accepts_structural_thin_and_fat_images_and_rejects_truncation() {
        let image = structural_image();
        assert!(accepted(&image));
        for length in 0..image.len() {
            assert!(!accepted(&image[..length]), "{length}");
        }
        for fat64 in [false, true] {
            for little in [false, true] {
                let mut fat = vec![0; 64];
                let magic: u32 = if fat64 { 0xcafebabf } else { 0xcafebabe };
                let put = |fat: &mut Vec<u8>, offset: usize, value: u32| {
                    fat[offset..offset + 4].copy_from_slice(&if little {
                        value.to_le_bytes()
                    } else {
                        value.to_be_bytes()
                    });
                };
                put(&mut fat, 0, magic);
                put(&mut fat, 4, 1);
                put(&mut fat, 8, 0x0100000c);
                if fat64 {
                    for (offset, value) in [(16, 64_u64), (24, image.len() as u64)] {
                        fat[offset..offset + 8].copy_from_slice(&if little {
                            value.to_le_bytes()
                        } else {
                            value.to_be_bytes()
                        });
                    }
                    put(&mut fat, 32, 6);
                } else {
                    put(&mut fat, 16, 64);
                    put(&mut fat, 20, image.len() as u32);
                    put(&mut fat, 24, 6);
                }
                fat.extend_from_slice(&image);
                assert!(accepted(&fat));
                let end = fat.len() - 1;
                assert!(!accepted(&fat[..end]));
                put(&mut fat, 8, 0x01000007);
                assert!(!accepted(&fat));
            }
        }
    }

    #[test]
    fn rejects_invalid_architecture_commands_segments_and_entry_points() {
        for (offset, value) in [
            (4, 0_u32),
            (12, 6),
            (16, 0),
            (16, u32::MAX),
            (20, u32::MAX),
            (28, 1),
            (36, 0),
            (36, 8),
            (80, u32::MAX),
            (92, 8),
            (96, 1),
            (108, 16),
            (112, 132),
        ] {
            let mut image = structural_image();
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(!accepted(&image), "{offset}: {value}");
        }
    }

    #[test]
    fn rejects_out_of_bounds_referenced_data_in_load_commands() {
        for (cmd, size, fields) in [
            (0x2_u32, 24, vec![(8, u32::MAX), (12, 1)]),
            (0xb, 80, vec![(32, u32::MAX), (36, 1)]),
            (0x80000022, 48, vec![(8, u32::MAX), (12, 1)]),
            (0x1d, 16, vec![(8, u32::MAX), (12, 1)]),
            (0x16, 16, vec![(8, u32::MAX), (12, 1)]),
            (0x31, 40, vec![(24, u32::MAX), (32, 1)]),
            (0x2c, 24, vec![(8, u32::MAX), (12, 1)]),
            (0xc, 24, vec![(8, u32::MAX)]),
            (0x1b, 8, vec![]),
        ] {
            let mut command = vec![0; size];
            command[0..4].copy_from_slice(&cmd.to_le_bytes());
            command[4..8].copy_from_slice(&(size as u32).to_le_bytes());
            for (offset, value) in fields {
                command[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            let mut image = structural_image();
            image.splice(128..128, command);
            for (offset, value) in [
                (16, 3_u32),
                (20, 96 + size as u32),
                (80, image.len() as u32),
                (112, 128 + size as u32),
            ] {
                image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            }
            assert!(!accepted(&image), "{cmd:x}");
        }
    }

    #[test]
    fn rejects_magic_only_and_truncated_native_images() {
        for magic in [
            0xfeedface_u32,
            0xcefaedfe,
            0xfeedfacf,
            0xcffaedfe,
            0xcafebabe,
            0xbebafeca,
            0xcafebabf,
            0xbfbafeca,
        ] {
            let bytes = magic.to_be_bytes().to_vec();
            assert!(
                !validate(&mut Cursor::new(&bytes), bytes.len() as u64),
                "{magic:x}"
            );
        }
    }
}
