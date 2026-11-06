use super::*;
use std::fs;

#[test]
fn file_paste_exports_only_locally_copied_files_and_validates_token_and_index() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("allowed.txt");
    fs::write(&path, b"allowed").unwrap();
    let export = paste::Export::new(vec![path.clone()], ClipboardVersion::default()).unwrap();
    assert!(export.open(&wire::new_id(), 0).is_err());
    assert!(export.open(&export.manifest.token, 1).is_err());
    let (mut file, entry) = export.open(&export.manifest.token, 0).unwrap();
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "allowed");
    assert_eq!(entry.name, "allowed.txt");
    assert_eq!(entry.size, 7);
    let json = serde_json::to_string(&export.manifest).unwrap();
    assert!(!json.contains(&directory.path().to_string_lossy().to_string()));
    assert!(
        paste::Export::new(vec![directory.path().into()], ClipboardVersion::default()).is_err()
    );
    for name in ["../outside", "C:\\secret", "x:stream", "CON"] {
        let manifest = paste::Manifest {
            token: wire::new_id(),
            files: vec![paste::Entry {
                name: name.into(),
                size: u64::MAX,
            }],
        };
        assert!(manifest.validate().is_err());
    }
}

#[test]
fn bulk_connection_requires_a_ticket_and_revokes_it_with_the_control_link() {
    use std::io::Write;
    use std::net::TcpStream;
    struct EmptyClipboard;
    impl Backend for EmptyClipboard {
        fn sequence(&mut self) -> u64 {
            0
        }
        fn read(&mut self) -> Result<Option<Content>> {
            Ok(None)
        }
        fn write(&mut self, _: &Content) -> Result<()> {
            Ok(())
        }
    }
    let rt = Runtime::new(&Config::default());
    let worker = launch(rt.clone(), [67; 32], true, Box::new(EmptyClipboard)).unwrap();
    let result = (|| -> Result<()> {
        let address = ("127.0.0.1", rt.exchange.port.load(Ordering::SeqCst));
        let connect = |token: String| -> Result<TcpStream> {
            let mut socket = TcpStream::connect(address)?;
            let mut noise = crate::transport::handshake(&mut socket, &[67; 32], true)?;
            socket.write_all(&wire::encode(
                &mut noise,
                &Packet::new("", "", Event::Join { ticket: token }),
            )?)?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            Ok(socket)
        };
        let closes = |socket: &mut TcpStream| -> bool {
            let mut bytes = [0; 1024];
            loop {
                match socket.read(&mut bytes) {
                    Ok(0) => return true,
                    Ok(_) => {} // Clock may already be buffered before revocation.
                    Err(e) => {
                        return matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        );
                    }
                }
            }
        };
        ensure!(
            closes(&mut connect(wire::new_id())?),
            "group key alone authorized a bulk connection"
        );
        ensure!(!rt.exchange.ready(), "invalid connection became ready");
        let parent = Arc::new(AtomicBool::new(false));
        let (_, token) = rt.exchange.register(wire::new_id(), parent.clone());
        let mut socket = connect(token.clone())?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rt.exchange.ready() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        ensure!(rt.exchange.ready(), "valid ticket not accepted");
        parent.store(true, Ordering::SeqCst);
        ensure!(
            closes(&mut socket),
            "control disconnect did not close data link"
        );
        ensure!(closes(&mut connect(token)?), "expired ticket was reusable");
        Ok(())
    })();
    rt.stop();
    worker.join().unwrap();
    result.unwrap();
}

#[test]
fn file_sizes_are_u64_without_the_original_clipboard_limit() {
    for size in [0, 100 * 1024 * 1024 + 1, 5 * 1024 * 1024 * 1024, u64::MAX] {
        Kind::File {
            name: "大文件.bin".into(),
        }
        .validate(size)
        .unwrap();
        let event = Event::Offer {
            id: wire::new_id(),
            kind: Kind::File {
                name: "data.bin".into(),
            },
            size,
            version: None,
        };
        let json = serde_json::to_vec(&event).unwrap();
        let Event::Offer {
            size: roundtrip, ..
        } = serde_json::from_slice(&json).unwrap()
        else {
            panic!()
        };
        assert_eq!(roundtrip, size);
    }
    assert!(Kind::Text.validate(wire::TEXT_LIMIT + 1).is_err());
}

#[test]
fn receiving_rejects_paths_and_never_overwrites_existing_files() {
    for name in [
        "../x", "..\\x", "C:\\x", "x:stream", "CON", "nul.txt", "COM1.bin", "LPT9", "x.", "x ", "",
        "..", "a/b",
    ] {
        assert!(files::validate_name(name).is_err(), "{name}");
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("中文.txt"), b"original").unwrap();
    let mut file = files::IncomingFile::create(dir.path(), "中文.txt", &wire::new_id(), 3).unwrap();
    assert!(file.write(1, b"bad").is_err());
    file.write(0, b"new").unwrap();
    let path = file.finish(&hex::encode(Sha256::digest(b"new"))).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"new");
    assert_eq!(fs::read(dir.path().join("中文.txt")).unwrap(), b"original");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn cancellation_and_bad_digest_remove_partial_files() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut file =
            files::IncomingFile::create(dir.path(), "x.bin", &wire::new_id(), 8).unwrap();
        file.write(0, b"1234").unwrap();
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    let mut file = files::IncomingFile::create(dir.path(), "x.bin", &wire::new_id(), 4).unwrap();
    file.write(0, b"1234").unwrap();
    assert!(file.finish(&"00".repeat(32)).is_err());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn encrypted_bulk_frames_preserve_binary_chunks_and_reject_tampering() {
    use std::net::TcpStream;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let other = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        crate::transport::handshake(&mut socket, &[57; 32], false).unwrap()
    });
    let mut socket = TcpStream::connect(addr).unwrap();
    let mut send = crate::transport::handshake(&mut socket, &[57; 32], true).unwrap();
    let mut receive = other.join().unwrap();
    let mut packet = Packet::new(
        &wire::new_id(),
        &wire::new_id(),
        Event::Chunk {
            id: wire::new_id(),
            offset: 5 * 1024 * 1024 * 1024,
        },
    );
    packet.data = (0..wire::CHUNK).map(|i| (i % 256) as u8).collect();
    let frame = wire::encode(&mut send, &packet).unwrap();
    assert!(frame.len() < wire::CHUNK + 1000);
    let mut incoming = frame[..frame.len() - 1].to_vec();
    assert!(wire::decode(&mut receive, &mut incoming).unwrap().is_none());
    incoming.push(*frame.last().unwrap());
    assert_eq!(
        wire::decode(&mut receive, &mut incoming)
            .unwrap()
            .unwrap()
            .data,
        packet.data
    );
    let mut tampered = wire::encode(&mut send, &packet).unwrap();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(wire::decode(&mut receive, &mut tampered).is_err());
}
