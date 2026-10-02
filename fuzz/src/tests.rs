use super::*;
use std::{fs, path::Path};

#[test]
fn seed_corpus_replays_and_reaches_file_data() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    for (name, run) in [
        ("filesystem", filesystem as fn(&[u8])),
        ("compression", compression),
        ("xattrs", xattrs),
        ("devices", devices),
        ("read_contract", read_contract),
    ] {
        let entries: Vec<_> = fs::read_dir(base.join(name)).unwrap().collect();
        assert!(!entries.is_empty());
        for entry in entries {
            let path = entry.unwrap().path();
            let data = fs::read(&path).unwrap();
            eprintln!("replaying {}", path.display());
            run(&data);
            if name == "filesystem" {
                let states = [State::default(), State::default()];
                let (fs, _) = systems(&data, &[], &states).unwrap();
                match path.file_name().unwrap().to_str().unwrap() {
                    "cycle" => {
                        let mut entries = fs.walk_dir("/").unwrap();
                        assert!(entries.next().unwrap().unwrap().inode.is_dir());
                        assert!(
                            matches!(entries.next().unwrap(), Err(erofs_rs::Error::CorruptedData(message)) if message == "directory cycle")
                        );
                    }
                    "byte-name" | "plain" => {
                        let path = &data[32..34];
                        let mut file = fs.open(path).unwrap();
                        assert_eq!(file.read(&mut [0; 1]).unwrap(), 1);
                    }
                    "symlink" => assert_eq!(
                        fs.read_link_inode(fs.get_inode(1).unwrap())
                            .unwrap()
                            .as_bytes(),
                        b"../f"
                    ),
                    _ => {}
                }
            }
            if name == "devices" {
                let states = [State::default(), State::default()];
                let images = [&data[8192..10240], &data[10240..]];
                let (fs, afs) =
                    systems(&data[..8192], &images[..usize::from(data[10])], &states).unwrap();
                let (a, b) = inodes(&fs, &afs, number(&data, 16), &states).unwrap();
                let mut file = fs.open_inode_file(a).unwrap();
                let expected = match path.file_name().unwrap().to_str().unwrap() {
                    "compressed" if !cfg!(feature = "lz4") => {
                        assert!(file.read(&mut [0; 1]).is_err());
                        continue;
                    }
                    "compressed" => [vec![b'A'; 700], vec![b'B'; 325], vec![b'C'; 475]].concat(),
                    "flat" => [vec![b'C'; 512], vec![b'D'; 188]].concat(),
                    "inline" => [vec![b'C'; 512], vec![b'!'; 188]].concat(),
                    _ => [
                        vec![b'C'; 512],
                        vec![b'D'; 512],
                        vec![b'A'; 512],
                        vec![b'B'; 512],
                        vec![0; 1024],
                        vec![b'D'; 17],
                    ]
                    .concat(),
                };
                let mut position = 0;
                let mut buf = [0; 257];
                while position < expected.len() {
                    let n = file.read(&mut buf).unwrap();
                    check_read(n, &buf, &mut position, &expected);
                }
                stream(&fs, &afs, a, b, &states);
            }
            if name == "xattrs" {
                let states = [State::default(), State::default()];
                let (fs, _) = systems(&data, &[], &states).unwrap();
                let result = fs.xattrs_inode(fs.get_inode(1).unwrap());
                if path.file_name().unwrap() == "compressed-prefix" && !cfg!(feature = "lz4") {
                    assert!(matches!(result, Err(erofs_rs::Error::NotSupported(_))));
                } else {
                    let attrs = result.unwrap();
                    let prefix = data[1115] != 0;
                    let value = if path.file_name().unwrap() == "large-value" {
                        [[0xff, 0].repeat(32767), vec![b'!']].concat()
                    } else {
                        b"one".to_vec()
                    };
                    assert_eq!(attrs.len(), 2);
                    assert_eq!(
                        attrs[if prefix {
                            b"user.long.key".as_slice()
                        } else {
                            b"user.key".as_slice()
                        }],
                        value
                    );
                    assert_eq!(attrs[b"security.selinux".as_slice()], b"two");
                }
            }
            if name == "compression" {
                let states = [State::default(), State::default()];
                let (fs, _) = systems(&data, &[], &states).unwrap();
                let inode = fs.get_inode(1).unwrap();
                let mut file = fs.open_inode_file(inode).unwrap();
                let result = file.read(&mut [0; 1]);
                let bitmap = fs.super_block().compr_algs;
                let enabled = (bitmap & 1 == 0 || cfg!(feature = "lz4"))
                    && (bitmap & 2 == 0 || cfg!(feature = "lzma"))
                    && (bitmap & 4 == 0 || cfg!(feature = "deflate"))
                    && (bitmap & 8 == 0 || cfg!(feature = "zstd"));
                // This fixture begins with a >4 GiB hole; reading its prefix needs no codec.
                if enabled || path.file_name().unwrap() == "wide-extent" {
                    assert_eq!(result.unwrap(), 1, "{}", path.display());
                } else {
                    assert!(result.is_err());
                }
            }
        }
    }
}

#[test]
fn sparse_backend_obeys_bounds_exact_reads_and_injection() {
    use erofs_rs::backend::{AsyncImage, Image};
    let state = State::default();
    let source = Source {
        data: b"abc",
        state: &state,
    };
    for base in [0, 1 << 40, 1 << 48] {
        assert_eq!(source.get(base..base + 3), Some(&b"abc"[..]));
        assert!(source.get(base..base + 4).is_none());
        let mut bytes = [0; 3];
        ready(source.read_exact_at(&mut bytes, base)).unwrap();
        assert_eq!(&bytes, b"abc");
    }
    assert!(source.get(3..4).is_none());
    assert!(source.get(u64::MAX..=u64::MAX).is_none());
    state.arm(Some(0));
    let count = state.reads.load(Relaxed);
    ready(source.read_exact_at(&mut [], 0)).unwrap();
    assert_eq!(count, state.reads.load(Relaxed));
    assert!(ready(source.read_exact_at(&mut [0; 3], 0)).is_err());
    assert!(state.injected.load(Relaxed));
    state.arm(None);
    ready(source.read_exact_at(&mut [0; 3], 0)).unwrap();
}
