use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io,
    os::unix::fs::FileExt as _,
};

const WORD: usize = size_of::<usize>();

pub fn hide() -> io::Result<()> {
    let stat = std::fs::read_to_string("/proc/self/stat")?;
    let fields: Vec<&str> = stat
        .trim_end()
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("/proc/self/stat has no command name"))?
        .1
        .split(' ')
        .collect();
    let field = |number: usize| {
        fields
            .get(number - 3)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::other(format!("/proc/self/stat has no address in field {number}"))
            })
    };
    let (stack, start, end) = (field(28)?, field(48)?, field(49)?);
    let memory = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/proc/self/mem")?;
    let mut area = vec![0; usize::try_from(end - start).map_err(io::Error::other)?];
    memory.read_exact_at(&mut area, start)?;
    let arguments: Vec<&[u8]> = area
        .strip_suffix(&[0])
        .ok_or_else(|| io::Error::other("the argument area was already rewritten"))?
        .split(|byte| *byte == 0)
        .collect();
    let count = read_word(&memory, stack)?;
    let skipped = match arguments.len().checked_sub(count) {
        Some(skipped) if skipped < arguments.len() => skipped,
        _ => {
            return Err(io::Error::other(
                "the argument area does not hold the arguments exec gave",
            ));
        }
    };
    let mut slots = vec![0; count * WORD];
    memory.read_exact_at(&mut slots, stack + WORD as u64)?;
    let mut offset = start;
    for (index, argument) in arguments.iter().enumerate() {
        let Some(shifted) = index.checked_sub(skipped) else {
            offset += argument.len() as u64 + 1;
            continue;
        };
        let slot = &mut slots[shifted * WORD..(shifted + 1) * WORD];
        let pointer = usize::from_ne_bytes(slot.try_into().map_err(io::Error::other)?);
        if pointer as u64 != offset {
            return Err(io::Error::other("argv does not point at the argument area"));
        }
        let copy = Box::leak(
            CString::new(*argument)
                .map_err(io::Error::other)?
                .into_boxed_c_str(),
        );
        slot.copy_from_slice(&(copy.as_ptr() as usize).to_ne_bytes());
        offset += argument.len() as u64 + 1;
    }
    assert_eq!(offset, end);
    memory.write_all_at(&slots, stack + WORD as u64)?;
    let shown = arguments[0].len() + 1;
    assert!(shown <= area.len());
    memory.write_all_at(&vec![0; area.len() - shown], start + shown as u64)
}

fn read_word(memory: &File, address: u64) -> io::Result<usize> {
    let mut word = [0; WORD];
    memory.read_exact_at(&mut word, address)?;
    Ok(usize::from_ne_bytes(word))
}
