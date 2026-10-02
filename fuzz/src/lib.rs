mod source;
#[cfg(not(feature = "std"))]
use erofs_rs::sync::file::Read;
use erofs_rs::{EroFS, r#async::EroFS as AsyncEroFS, types::Inode};
use libfuzzer_sys::arbitrary::{self, Arbitrary, Unstructured};
use source::{Source, State, ready};
#[cfg(feature = "std")]
use std::io::Read;
use std::{borrow::Cow, sync::atomic::Ordering::Relaxed};

const MAX_INPUT: usize = 1024 * 1024;
const STREAM_LIMIT: usize = 64 * 1024;
type Fs<'a> = EroFS<Source<'a>>;
type AsyncFs<'a> = AsyncEroFS<Source<'a>>;

fn number(data: &[u8], at: usize) -> u64 {
    data.get(at..at + 8)
        .map_or(0, |bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn pair<A, B, E, F>(a: Result<A, E>, b: Result<B, F>, states: &[State; 2]) -> Option<(A, B)> {
    if states.iter().any(|s| s.exhausted.load(Relaxed)) {
        return None;
    }
    assert_eq!(a.is_ok(), b.is_ok(), "sync/async success mismatch");
    match (a, b) {
        (Ok(a), Ok(b)) => Some((a, b)),
        _ => None,
    }
}

fn systems<'a>(
    data: &'a [u8],
    devices: &[&'a [u8]],
    states: &'a [State; 2],
) -> Option<(Fs<'a>, AsyncFs<'a>)> {
    pair(
        EroFS::new_with_devices(
            Source {
                data,
                state: &states[0],
            },
            devices
                .iter()
                .map(|data| Source {
                    data,
                    state: &states[0],
                })
                .collect(),
        ),
        ready(AsyncEroFS::new_with_devices(
            Source {
                data,
                state: &states[1],
            },
            devices
                .iter()
                .map(|data| Source {
                    data,
                    state: &states[1],
                })
                .collect(),
        )),
        states,
    )
}

fn inodes(fs: &Fs<'_>, afs: &AsyncFs<'_>, nid: u64, states: &[State; 2]) -> Option<(Inode, Inode)> {
    let (a, b) = pair(fs.get_inode(nid), ready(afs.get_inode(nid)), states)?;
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    Some((a, b))
}

fn stream(fs: &Fs<'_>, afs: &AsyncFs<'_>, a: Inode, b: Inode, states: &[State; 2]) {
    let Some((mut file, mut afile)) = pair(fs.open_inode_file(a), afs.open_inode_file(b), states)
    else {
        return;
    };
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut buf = [0; 4096];
    while left.len() < STREAM_LIMIT {
        let size = (STREAM_LIMIT - left.len()).min(4096);
        match file.read(&mut buf[..size]) {
            Ok(0) => {
                assert_eq!(left.len() as u64, a.data_size());
                break;
            }
            Ok(n) => {
                assert!(n <= size);
                left.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    // Deliberately use a different partition. A short read need not match per call.
    while right.len() < STREAM_LIMIT {
        let size = (STREAM_LIMIT - right.len()).min(257);
        match ready(afile.read(&mut buf[..size])) {
            Ok(0) => {
                assert_eq!(right.len() as u64, b.data_size());
                break;
            }
            Ok(n) => {
                assert!(n <= size);
                right.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    if states.iter().all(|s| !s.exhausted.load(Relaxed)) {
        assert_eq!(left, right);
    }
}

/// Raw image bytes; the unused boot area also supplies paths and a candidate NID.
pub fn filesystem(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let states = [State::default(), State::default()];
    let Some((fs, afs)) = systems(data, &[], &states) else {
        return;
    };
    for nid in [fs.super_block().root_inode_id(), 1, number(data, 16)] {
        if let Some((a, b)) = inodes(&fs, &afs, nid, &states) {
            stream(&fs, &afs, a, b, &states);
            if let Some((a, b)) = pair(fs.xattrs_inode(a), ready(afs.xattrs_inode(b)), &states) {
                assert_eq!(a, b);
            }
            if a.is_symlink() && a.data_size() <= 4096 {
                let _ = fs.read_link_inode(a);
            }
        }
    }
    let Some((dir, mut adir)) = pair(fs.walk_dir("/"), ready(afs.walk_dir("/")), &states) else {
        return;
    };
    let left: Vec<_> = dir
        .max_depth(8)
        .take(16)
        .map(|entry| {
            entry
                .map(|e| {
                    (
                        e.dir_entry.path().as_bytes().to_vec(),
                        e.inode.id(),
                        e.depth,
                    )
                })
                .ok()
        })
        .collect();
    adir = adir.max_depth(8);
    let mut right = Vec::new();
    for _ in 0..16 {
        let Some(entry) = ready(adir.next_entry()) else {
            break;
        };
        right.push(
            entry
                .map(|e| {
                    (
                        e.dir_entry.path().as_bytes().to_vec(),
                        e.inode.id(),
                        e.depth,
                    )
                })
                .ok(),
        );
    }
    if states.iter().all(|s| !s.exhausted.load(Relaxed)) {
        assert_eq!(left, right);
    }
    for path in [
        b"/f/.".as_slice(),
        b"/missing/../f",
        data.get(32..32 + usize::from(data.get(31).copied().unwrap_or(0) % 65))
            .unwrap_or_default(),
    ] {
        let _ = pair(fs.open(path), ready(afs.open(path)), &states);
    }
}

// Keep a minimal valid outer envelope in half the inputs, to reach inner parsers.
// The raw filesystem target still tests these fields without repair.
fn envelope(data: &[u8], compression: bool) -> Cow<'_, [u8]> {
    if data.get(9).is_none_or(|b| b & 1 == 0) {
        return Cow::Borrowed(data);
    }
    let mut image = data.to_vec();
    image.resize(image.len().max(2176), 0);
    image[1024..1028].copy_from_slice(&erofs_rs::types::MAGIC_NUMBER.to_le_bytes());
    image[1036] = 9;
    image[1056..1060].fill(0); // Valid inherited nanoseconds.
    image[1064..1068].copy_from_slice(&4u32.to_le_bytes());
    image[1105..1108].fill(0); // Known incompatibility bits.
    image[1110..1112].fill(0);
    image[2080..2082].copy_from_slice(
        &(if compression {
            if data.get(10).is_some_and(|b| b & 1 != 0) {
                6u16
            } else {
                2
            }
        } else {
            0
        })
        .to_le_bytes(),
    );
    image[2084..2086].copy_from_slice(&0o100644u16.to_le_bytes());
    if compression {
        image[2082..2084].fill(0);
    }
    Cow::Owned(image)
}

pub fn compression(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let image = envelope(data, true);
    let states = [State::default(), State::default()];
    if let Some((fs, afs)) = systems(&image, &[], &states) {
        let nid = if matches!(image, Cow::Owned(_)) {
            1
        } else {
            number(data, 16)
        };
        if let Some((a, b)) = inodes(&fs, &afs, nid, &states) {
            stream(&fs, &afs, a, b, &states);
        }
    }
}

pub fn xattrs(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let image = envelope(data, false);
    let states = [State::default(), State::default()];
    let Some((fs, afs)) = systems(&image, &[], &states) else {
        return;
    };
    let nid = if matches!(image, Cow::Owned(_)) {
        1
    } else {
        number(data, 16)
    };
    let Some((a, b)) = inodes(&fs, &afs, nid, &states) else {
        return;
    };
    if let Some((attrs, aattrs)) = pair(fs.xattrs_inode(a), ready(afs.xattrs_inode(b)), &states) {
        assert_eq!(attrs, aattrs);
        for name in attrs.keys() {
            assert!(!name.is_empty() && name.len() <= 255 && !name.contains(&0));
        }
    }
}

/// Fixed 8192-byte primary segment, 2048-byte device 1, then device 2.
pub fn devices(data: &[u8]) {
    if data.len() > MAX_INPUT {
        return;
    }
    let (primary, extra) = data.split_at(data.len().min(8192));
    let (first, second) = extra.split_at(extra.len().min(2048));
    let images = [first, second];
    let count = data.get(10).copied().unwrap_or(0) as usize % 3;
    let states = [State::default(), State::default()];
    if let Some((fs, afs)) = systems(primary, &images[..count], &states) {
        assert_eq!(fs.devices(), afs.devices());
        if let Some((a, b)) = inodes(&fs, &afs, number(data, 16), &states) {
            stream(&fs, &afs, a, b, &states);
        }
    }
}

#[derive(Arbitrary)]
struct Operation {
    size: u16,
    failure: u8,
}

fn check_read(n: usize, buf: &[u8], position: &mut usize, expected: &[u8]) {
    assert!(n <= buf.len());
    if buf.is_empty() || *position == expected.len() {
        assert_eq!(n, 0);
    } else {
        assert!(n > 0);
    }
    assert_eq!(&buf[..n], &expected[*position..*position + n]);
    *position += n;
}

fn directory_retry(failures: u8) {
    let image = include_bytes!("../seeds/filesystem/directory-blocks");
    let states = [State::default(), State::default()];
    let (fs, afs) = systems(image, &[], &states).unwrap();
    let mut dir = fs.read_dir("/").unwrap();
    let mut adir = ready(afs.read_dir("/")).unwrap();
    assert_eq!(dir.next().unwrap().unwrap().dir_entry.file_name(), b"f");
    assert_eq!(
        ready(adir.next_entry())
            .unwrap()
            .unwrap()
            .dir_entry
            .file_name(),
        b"f"
    );
    // Fail the next directory block load, not the subsequent dirent inode lookup.
    for _ in 0..failures % 4 {
        for state in &states {
            state.arm(Some(0));
        }
        assert!(dir.next().unwrap().is_err());
        assert!(ready(adir.next_entry()).unwrap().is_err());
        assert!(states.iter().all(|state| state.injected.load(Relaxed)));
    }
    for state in &states {
        state.arm(None);
    }
    assert_eq!(dir.next().unwrap().unwrap().dir_entry.file_name(), b"g");
    assert_eq!(
        ready(adir.next_entry())
            .unwrap()
            .unwrap()
            .dir_entry
            .file_name(),
        b"g"
    );
    assert!(dir.next().is_none());
    assert!(ready(adir.next_entry()).is_none());
}

/// Only operations mutate here. Images and expected bytes form an independent oracle.
pub fn read_contract(data: &[u8]) {
    directory_retry(data.last().copied().unwrap_or(0));
    let images: &[(&[u8], bool)] = &[
        (include_bytes!("../seeds/filesystem/plain"), true),
        (
            include_bytes!("../seeds/compression/full-lz4"),
            cfg!(feature = "lz4"),
        ),
        (
            include_bytes!("../seeds/compression/full-lzma"),
            cfg!(feature = "lzma"),
        ),
        (
            include_bytes!("../seeds/compression/full-deflate"),
            cfg!(feature = "deflate"),
        ),
        (
            include_bytes!("../seeds/compression/full-zstd"),
            cfg!(feature = "zstd"),
        ),
    ];
    let index = data.first().copied().unwrap_or(0) as usize % images.len();
    let (image, enabled) = images[index];
    if !enabled {
        return;
    }
    let states = [State::default(), State::default()];
    let (fs, afs) = systems(image, &[], &states).unwrap();
    let mut file = fs.open_inode_file(fs.get_inode(1).unwrap()).unwrap();
    let mut afile = afs
        .open_inode_file(ready(afs.get_inode(1)).unwrap())
        .unwrap();
    let expected = [vec![b'A'; 700], vec![b'B'; 325], vec![b'C'; 475]].concat();
    let mut positions = [0, 0];
    let mut buf = [0; 2048];
    // Establish a cache and prove that a subsequent read cannot hit the backend.
    for step in 0..2 {
        for state in &states {
            state.arm((step == 1).then_some(0));
        }
        let counts = states.each_ref().map(|s| s.reads.load(Relaxed));
        let n = file.read(&mut buf[..1]).unwrap();
        check_read(n, &buf[..1], &mut positions[0], &expected);
        let n = ready(afile.read(&mut buf[..1])).unwrap();
        check_read(n, &buf[..1], &mut positions[1], &expected);
        if step == 1 {
            assert_eq!(counts, states.each_ref().map(|s| s.reads.load(Relaxed)));
        }
    }
    let mut input = Unstructured::new(data.get(1..).unwrap_or_default());
    for _ in 0..32 {
        let Ok(op) = input.arbitrary::<Operation>() else {
            break;
        };
        let size = usize::from(op.size) % 2049;
        for state in &states {
            state.arm((op.failure != 0).then_some(usize::from(op.failure) % 8));
        }
        let counts = states.each_ref().map(|s| s.reads.load(Relaxed));
        let empty = [
            size == 0 || positions[0] == expected.len(),
            size == 0 || positions[1] == expected.len(),
        ];
        let n = match file.read(&mut buf[..size]) {
            Ok(n) => n,
            Err(_) => {
                assert!(states[0].injected.load(Relaxed));
                states[0].arm(None);
                file.read(&mut buf[..size]).unwrap()
            }
        };
        check_read(n, &buf[..size], &mut positions[0], &expected);
        let n = match ready(afile.read(&mut buf[..size])) {
            Ok(n) => n,
            Err(_) => {
                assert!(states[1].injected.load(Relaxed));
                states[1].arm(None);
                ready(afile.read(&mut buf[..size])).unwrap()
            }
        };
        check_read(n, &buf[..size], &mut positions[1], &expected);
        for side in 0..2 {
            if empty[side] {
                assert_eq!(counts[side], states[side].reads.load(Relaxed));
            }
        }
    }
    for state in &states {
        state.arm(None);
    }
    while positions[0] < expected.len() {
        let n = file.read(&mut buf[..7]).unwrap();
        check_read(n, &buf[..7], &mut positions[0], &expected);
    }
    while positions[1] < expected.len() {
        let n = ready(afile.read(&mut buf[..113])).unwrap();
        check_read(n, &buf[..113], &mut positions[1], &expected);
    }
    for state in &states {
        state.arm(Some(0));
    }
    assert_eq!(file.read(&mut buf).unwrap(), 0);
    assert_eq!(ready(afile.read(&mut buf)).unwrap(), 0);
    assert!(
        states
            .iter()
            .all(|s| !s.injected.load(Relaxed) && !s.exhausted.load(Relaxed))
    );
}

#[cfg(test)]
mod tests;
