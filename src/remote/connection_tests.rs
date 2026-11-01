use super::{Cache, Config, Credentials, Temporary, MAX_RECORD};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn consecutive_requests_reuse_the_same_connection() {
    let (url, server) = serve_two_requests(false);
    let cache = Cache::new(Config {
        read_url: url,
        macos_cache_path: None,
        op_access_key_id: None,
        op_secret_access_key: None,
    })
    .unwrap();
    let key = "a".repeat(64);
    for _ in 0..2 {
        let record = cache
            .fetch_record(&key, &AtomicBool::new(false))
            .unwrap()
            .unwrap();
        assert_eq!(record, br#"{"blobs":[]}"#);
    }
    assert_eq!(server.join().unwrap(), 1, "GETs should share a connection");
}

#[test]
fn signed_requests_reuse_the_same_connection() {
    let (url, server) = serve_two_requests(true);
    let cache = Cache::new(Config {
        read_url: url.clone(),
        macos_cache_path: None,
        op_access_key_id: None,
        op_secret_access_key: None,
    })
    .unwrap();
    let credentials = Credentials {
        access: "fixture-access".into(),
        secret: "fixture-secret".into(),
    };
    let output = Temporary::new(&std::env::temp_dir()).unwrap();
    let url = format!("{url}/corgi/v2/actions/{}.json", "a".repeat(64));
    for _ in 0..2 {
        assert_eq!(
            cache
                .request_with_connection(
                    "GET",
                    &url,
                    &output.0,
                    None,
                    Some(&credentials),
                    MAX_RECORD,
                    &AtomicBool::new(false),
                    &[],
                )
                .unwrap(),
            200
        );
    }
    assert_eq!(
        server.join().unwrap(),
        1,
        "signed GETs should share a connection"
    );
}

#[test]
fn cancellation_interrupts_an_unresponsive_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/bucket", listener.local_addr().unwrap());
    let cancel = Arc::new(AtomicBool::new(false));
    let (started, waiting) = mpsc::channel();
    let server_cancel = Arc::clone(&cancel);
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        started.send(()).unwrap();
        while !server_cancel.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(10));
        }
        drop(stream);
    });
    let request_cancel = Arc::clone(&cancel);
    let request = thread::spawn(move || {
        Cache::new(Config {
            read_url: url,
            macos_cache_path: None,
            op_access_key_id: None,
            op_secret_access_key: None,
        })
        .unwrap()
        .fetch_record(&"a".repeat(64), &request_cancel)
    });
    waiting.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    cancel.store(true, Ordering::Relaxed);
    assert!(
        request
            .join()
            .unwrap()
            .unwrap_err()
            .is::<super::ReadCancelled>(),
        "the cancelled request must not become a cache miss"
    );
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "cancellation should not wait for a transport timeout"
    );
    server.join().unwrap();
}

fn serve_two_requests(signed: bool) -> (String, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/bucket", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        respond(&mut reader, &mut stream, signed).unwrap();
        if respond(&mut reader, &mut stream, signed).is_ok() {
            return 1;
        }
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        respond(&mut reader, &mut stream, signed).unwrap();
        2
    });
    (url, server)
}

fn respond(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    signed: bool,
) -> std::io::Result<()> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if !line.starts_with("GET /bucket/corgi/v2/actions/") {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut authorized = false;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line == "\r\n" {
            break;
        }
        authorized |= line
            .to_ascii_lowercase()
            .starts_with("authorization: aws4-hmac-sha256 ");
    }
    assert_eq!(authorized, signed, "only signed requests send credentials");
    let body = br#"{"blobs":[]}"#;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}
