use crate::{
    config::Config,
    script::{decompile, disasm_to_string, get_script_name, Scope},
    xor::XorStream,
};
use byteordered::byteorder::{ReadBytesExt, BE, LE};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    fmt::Write,
    io,
    io::{BufReader, Read, Seek, SeekFrom},
    mem,
    ops::Range,
    str,
};
use tracing::info_span;

pub const NICE: u8 = 0x69;

pub struct Index {
    pub lfl_disks: Vec<u8>,
    pub lfl_offsets: Vec<i32>,
    pub scripts: Directory,
    pub sounds: Directory,
    pub talkies: Directory,
}

pub struct Directory {
    /// The game internally calls this "disk number"
    pub room_numbers: Vec<u8>,
    /// The game internally calls this "disk offset"
    pub offsets: Vec<i32>,
    pub glob_sizes: Vec<i32>,
}

pub fn read_index(s: &mut (impl Read + Seek)) -> Result<Index, Box<dyn Error>> {
    let s = XorStream::new(s, NICE);
    let mut s = BufReader::new(s);

    let len = s.seek(SeekFrom::End(0))?;
    s.rewind()?;

    let mut state = IndexState {
        buf: Vec::with_capacity(64 << 10),
        lfl_disks: None,
        lfl_offsets: None,
        scripts: None,
        sounds: None,
        talkies: None,
    };

    scan_blocks(&mut s, &mut state, handle_index_block, len)?;

    Ok(Index {
        lfl_disks: state.lfl_disks.ok_or("index incomplete")?,
        lfl_offsets: state.lfl_offsets.ok_or("index incomplete")?,
        scripts: state.scripts.ok_or("index incomplete")?,
        sounds: state.sounds.ok_or("index incomplete")?,
        talkies: state.talkies.ok_or("index incomplete")?,
    })
}

struct IndexState {
    buf: Vec<u8>,
    lfl_disks: Option<Vec<u8>>,
    lfl_offsets: Option<Vec<i32>>,
    scripts: Option<Directory>,
    sounds: Option<Directory>,
    talkies: Option<Directory>,
}

fn handle_index_block<R: Read>(
    stream: &mut R,
    state: &mut IndexState,
    id: [u8; 4],
    len: u64,
) -> Result<(), Box<dyn Error>> {
    state.buf.resize(len.try_into().unwrap(), 0);
    stream.read_exact(&mut state.buf)?;
    let mut r = io::Cursor::new(&state.buf);

    match &id {
        b"DISK" => {
            let count = r.read_i16::<LE>()?;
            let mut list = vec![0; count.try_into()?];
            r.read_exact(&mut list)?;
            state.lfl_disks = Some(list);
        }
        b"DLFL" => {
            let count = r.read_i16::<LE>()?;
            let mut list = vec![0; count.try_into()?];
            r.read_i32_into::<LE>(&mut list)?;
            state.lfl_offsets = Some(list);
        }
        b"DIRS" => {
            state.scripts = Some(read_directory(&mut r)?);
        }
        b"DIRN" => {
            state.sounds = Some(read_directory(&mut r)?);
        }
        b"DIRT" => {
            state.talkies = Some(read_directory(&mut r)?);
        }
        _ => {
            r.seek(SeekFrom::End(0))?;
        }
    }
    Ok(())
}

fn read_directory(r: &mut impl Read) -> Result<Directory, Box<dyn Error>> {
    let count = r.read_i16::<LE>()?;
    let mut dir = Directory {
        room_numbers: vec![0; count.try_into()?],
        offsets: vec![0; count.try_into()?],
        glob_sizes: vec![0; count.try_into()?],
    };
    r.read_exact(&mut dir.room_numbers)?;
    r.read_i32_into::<LE>(&mut dir.offsets)?;
    r.read_i32_into::<LE>(&mut dir.glob_sizes)?;
    Ok(dir)
}

pub fn dump_index(w: &mut impl Write, index: &Index) -> fmt::Result {
    w.write_str("lfl_disks:\n")?;
    for (i, x) in index.lfl_disks.iter().enumerate() {
        writeln!(w, "\t{i}\t{x}")?;
    }
    w.write_str("lfl_offsets:\n")?;
    for (i, x) in index.lfl_disks.iter().enumerate() {
        writeln!(w, "\t{i}\t{x}")?;
    }
    w.write_str("scripts:\n")?;
    dump_directory(w, index, &index.scripts)?;
    w.write_str("sounds:\n")?;
    dump_directory(w, index, &index.sounds)?;
    Ok(())
}

fn dump_directory(w: &mut impl Write, index: &Index, dir: &Directory) -> fmt::Result {
    for i in 0..dir.room_numbers.len() {
        let room: usize = dir.room_numbers[i].into();
        writeln!(
            w,
            "\t{}\t{}\t{}\t{}\t{}\t{}",
            i,
            dir.room_numbers[i],
            dir.offsets[i],
            dir.glob_sizes[i],
            index.lfl_disks[room],
            index.lfl_offsets[room] + dir.offsets[i],
        )?;
    }
    Ok(())
}

pub fn extract(
    index: &Index,
    disk_number: u8,
    config: &Config,
    publish_scripts: bool,
    s: &mut (impl Read + Seek),
    write: &mut impl FnMut(&str, &[u8]) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let s = XorStream::new(s, NICE);
    let mut s = BufReader::new(s);

    let len = s.seek(SeekFrom::End(0))?;
    s.rewind()?;

    let mut state = ExtractState {
        disk_number,
        index,
        config,
        write,
        path: {
            let mut path = String::with_capacity(64);
            path.push('.');
            path
        },
        publish_scripts,
        current_room: 0,
        current_object: 0,
        block_numbers: HashMap::new(),
        map: String::with_capacity(1 << 10),
        buf: Vec::with_capacity(64 << 10),
    };

    scan_blocks(&mut s, &mut state, handle_extract_block, len)?;

    (state.write)(&format!("{}/.map", state.path), state.map.as_bytes())?;
    Ok(())
}

struct ExtractState<'a> {
    disk_number: u8,
    index: &'a Index,
    config: &'a Config,
    write: &'a mut dyn FnMut(&str, &[u8]) -> Result<(), Box<dyn Error>>,
    path: String,
    publish_scripts: bool,
    current_room: i32,
    current_object: u16,
    block_numbers: HashMap<[u8; 4], i32>,
    map: String,
    buf: Vec<u8>,
}

fn handle_extract_block<R: Read + Seek>(
    r: &mut R,
    state: &mut ExtractState,
    id: [u8; 4],
    len: u64,
) -> Result<(), Box<dyn Error>> {
    let offset = r.stream_position()?;

    if guess_is_block_recursive(r, len)? {
        extract_recursive(r, state, id, offset, len)?;
    } else {
        extract_flat(r, state, id, len, offset)?;
    }
    Ok(())
}

fn extract_recursive<R: Read + Seek>(
    r: &mut R,
    state: &mut ExtractState,
    id: [u8; 4],
    offset: u64,
    len: u64,
) -> Result<(), Box<dyn Error>> {
    let number = match &id {
        b"LECF" => state.disk_number.into(),
        b"LFLF" => {
            state.current_room = find_lfl_number(state.disk_number, offset, state.index)
                .ok_or("LFL not in index")?;
            state.current_room
        }
        b"DIGI" | b"TALK" => {
            find_object_number(
                state.index,
                &state.index.sounds,
                state.disk_number,
                offset - 8,
            )
            .ok_or("sound not in index")?
        }
        b"TLKE" => {
            find_object_number(
                state.index,
                &state.index.talkies,
                state.disk_number,
                offset - 8,
            )
            .ok_or("talkie not in index")?
        }
        _ => {
            *state
                .block_numbers
                .entry(id)
                .and_modify(|n| *n += 1)
                .or_insert(1)
        }
    };

    writeln!(state.map, "{}", IdAndNum(id, number))?;

    write!(state.path, "/{}", IdAndNum(id, number))?;

    // copy most fields, temporarily move some
    let mut inner = ExtractState {
        disk_number: state.disk_number,
        index: state.index,
        config: state.config,
        write: state.write,
        path: mem::take(&mut state.path),
        publish_scripts: state.publish_scripts,
        current_room: state.current_room,
        current_object: state.current_object,
        block_numbers: HashMap::new(),
        map: String::with_capacity(1 << 10),
        buf: mem::take(&mut state.buf),
    };

    scan_blocks(r, &mut inner, handle_extract_block, len)?;

    // return temporarily moved fields
    state.buf = mem::take(&mut inner.buf);
    state.path = mem::take(&mut inner.path);

    let map = inner.map;
    (state.write)(&format!("{}/.map", state.path), map.as_bytes())?;

    state.path.truncate(state.path.rfind('/').unwrap());
    Ok(())
}

fn extract_flat<R: Read + Seek>(
    r: &mut R,
    state: &mut ExtractState,
    id: [u8; 4],
    len: u64,
    offset: u64,
) -> Result<(), Box<dyn Error>> {
    state.buf.clear();
    state.buf.reserve(len.try_into()?);
    io::copy(&mut r.take(len), &mut state.buf)?;

    let number = match &id {
        // SCRP number comes from index
        b"SCRP" => {
            find_object_number(
                state.index,
                &state.index.scripts,
                state.disk_number,
                offset - 8,
            )
            .ok_or("script missing from index")?
        }
        // LSC2 number comes from block header
        b"LSC2" => {
            let number_bytes = state.buf.get(..4).ok_or("local script missing header")?;
            i32::from_le_bytes(number_bytes.try_into().unwrap())
        }
        b"CDHD" => {
            let number_bytes = state.buf.get(..2).ok_or("bad object header")?;
            state.current_object = u16::from_le_bytes(number_bytes.try_into().unwrap());
            state.current_object.into()
        }
        // Otherwise use a counter per block type
        _ => {
            *state
                .block_numbers
                .entry(id)
                .and_modify(|n| *n += 1)
                .or_insert(1)
        }
    };

    writeln!(state.map, "{}", IdAndNum(id, number))?;

    let filename = format!("{}/{}.bin", state.path, IdAndNum(id, number));
    (state.write)(&filename, &state.buf)?;

    match &id {
        b"SCRP" | b"LSC2" | b"ENCD" | b"EXCD" => extract_script(state, id, number)?,
        b"VERB" => {
            debug_assert!(number == 1); // only one VERB per OBCD
            extract_verb(state)?;
        }
        _ => {}
    }
    Ok(())
}

fn extract_script(
    state: &mut ExtractState,
    id: [u8; 4],
    number: i32,
) -> Result<(), Box<dyn Error>> {
    let mut range = 0..state.buf.len();

    if id == *b"LSC2" {
        // Skip header. Code starts at offset 4.
        range.start = 4;
    }

    let id_num = IdAndNum(id, number);
    let scope = match &id {
        b"SCRP" => Scope::Global(number),
        b"LSC2" => Scope::RoomLocal(state.current_room, number),
        b"ENCD" => Scope::RoomEnter(state.current_room),
        b"EXCD" => Scope::RoomExit(state.current_room),
        _ => unreachable!(),
    };
    output_script(state, range, id_num, scope)?;
    Ok(())
}

fn extract_verb(state: &mut ExtractState) -> Result<(), Box<dyn Error>> {
    let mut pos = 0;
    while let Some((number, offset)) = read_verb(&state.buf[pos..]) {
        let start = (offset - 8).try_into()?; // relative to block including type/len
        pos += 3;
        let next_offset = read_verb(&state.buf[pos..]).map(|(_, o)| o);
        let end = match next_offset {
            Some(o) => (o - 8).try_into()?,
            None => state.buf.len(),
        };

        let id_num = IdAndNum(*b"VERB", number.into());
        let scope = Scope::Verb(state.current_room, state.current_object);
        output_script(state, start..end, id_num, scope)?;
    }
    Ok(())
}

fn read_verb(buf: &[u8]) -> Option<(u8, u16)> {
    let number = *buf.get(0)?;
    if number == 0 {
        return None;
    }
    let offset = u16::from_le_bytes(buf.get(1..3)?.try_into().unwrap());
    Some((number, offset))
}

fn output_script(
    state: &mut ExtractState,
    range: Range<usize>,
    id_num: IdAndNum,
    scope: Scope,
) -> Result<(), Box<dyn Error>> {
    let code = &state.buf[range];

    let disasm = disasm_to_string(code);
    let filename = format!("{}/{}.s", state.path, id_num);
    (state.write)(&filename, disasm.as_bytes())?;

    let decomp = {
        let _span = info_span!("decompile", script = %id_num).entered();
        decompile(code, scope, state.config)
    };
    let mut filename = format!("{}/{}.scu", state.path, id_num);
    (state.write)(&filename, decomp.as_bytes())?;

    if state.publish_scripts {
        filename.clear();
        write!(filename, "scripts/")?;
        match scope {
            Scope::Global(number) => write!(filename, "scr{number:04}")?,
            Scope::RoomLocal(room, number) => write!(filename, "{room:02}/lsc{number:04}")?,
            Scope::RoomEnter(room) => write!(filename, "{room:02}/enter")?,
            Scope::RoomExit(room) => write!(filename, "{room:02}/exit")?,
            Scope::Verb(room, object) => {
                write!(
                    filename,
                    "{room:02}/obj{object:04} verb{verb:02}",
                    verb = id_num.1,
                )?;
            }
        }
        if let Some(name) = get_script_name(scope, state.config) {
            write!(filename, " {name}")?;
        }
        write!(filename, ".scu")?;
        (state.write)(&filename, decomp.as_bytes())?;
    }

    Ok(())
}

fn find_lfl_number(disk_number: u8, offset: u64, index: &Index) -> Option<i32> {
    for i in 0..index.lfl_disks.len() {
        if index.lfl_disks[i] == disk_number && Ok(index.lfl_offsets[i]) == offset.try_into() {
            return Some(i.try_into().unwrap());
        }
    }
    None
}

fn find_object_number(index: &Index, dir: &Directory, disk_number: u8, offset: u64) -> Option<i32> {
    let offset: i32 = offset.try_into().ok()?;
    for i in 0..dir.room_numbers.len() {
        let room_number: usize = dir.room_numbers[i].into();
        let dnum = index.lfl_disks[room_number];
        let doff = index.lfl_offsets[room_number] + dir.offsets[i];
        if dnum == disk_number && doff == offset {
            return Some(i.try_into().unwrap());
        }
    }
    None
}

#[derive(Copy, Clone)]
struct IdAndNum([u8; 4], i32);

impl fmt::Display for IdAndNum {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(str::from_utf8(&self.0).unwrap())?;
        f.write_char('_')?;
        write!(f, "{:02}", self.1)?;
        Ok(())
    }
}

type BlockHandler<Stream, State> =
    fn(&mut Stream, &mut State, [u8; 4], u64) -> Result<(), Box<dyn Error>>;

fn scan_blocks<Stream: Read + Seek, State>(
    s: &mut Stream,
    state: &mut State,
    handle_block: BlockHandler<Stream, State>,
    parent_len: u64,
) -> Result<(), Box<dyn Error>> {
    let start = s.stream_position()?;
    loop {
        let pos = s.stream_position()?;
        if pos == start + parent_len {
            break;
        }
        if pos > start + parent_len {
            return Err("misaligned block end".into());
        }

        read_block(s, |s, id, len| handle_block(s, state, id, len))?;
    }
    Ok(())
}

fn read_block<S: Read + Seek, R>(
    s: &mut S,
    f: impl FnOnce(&mut S, [u8; 4], u64) -> Result<R, Box<dyn Error>>,
) -> Result<R, Box<dyn Error>> {
    let start = s.stream_position()?;
    let mut id = [0; 4];
    s.read_exact(&mut id)?;
    let len = s.read_i32::<BE>()?;
    let len: u64 = len.try_into()?;
    let result = f(s, id, len - 8)?;
    let end = s.stream_position()?;
    if end - start != len {
        return Err("bug: block reader read wrong length".into());
    }
    Ok(result)
}

// heuristic
fn guess_is_block_recursive<S: Read + Seek>(s: &mut S, len: u64) -> Result<bool, Box<dyn Error>> {
    if len < 8 {
        return Ok(false);
    }
    let start = s.stream_position()?;
    let mut id = [0; 4];
    s.read_exact(&mut id)?;
    let len = s.read_i32::<BE>()?;
    s.seek(SeekFrom::Start(start))?; // only peek, don't consume

    Ok(id.iter().all(|&ch| (32..=126).contains(&ch)) && (0..0x100_0000).contains(&len))
}
