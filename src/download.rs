//! Bounded parallel HTTP downloads, with a streaming fallback for ordinary servers.
use std::{
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::FileExt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Result, ensure};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, Response},
    header,
};
use tempfile::NamedTempFile;

use crate::output::DownloadProgress;

const PARALLEL_THRESHOLD: u64 = 1024 * 1024;
const CONNECTIONS: u64 = 4;
const BUFFER_SIZE: usize = 256 * 1024;

pub fn download(client: &Client, url: &Url, name: &str) -> Result<NamedTempFile> {
    let response = request(client, url)?;
    let mut file = NamedTempFile::new()?;
    crate::space::temporary(
        file.as_file(),
        response.content_length().unwrap_or(BUFFER_SIZE as u64),
    )?;
    if let Some((total, validator)) = parallel_parameters(&response) {
        // Use the final URL after redirects for all segments of this same object.
        let final_url = response.url().clone();
        if parallel(
            client,
            &final_url,
            response,
            file.as_file(),
            name,
            total,
            &validator,
        )
        .is_ok()
        {
            return Ok(file);
        }
        // Range support can be advertised but not honored, or an object may change.
        // Discard all segments and retry one complete response; never mix versions.
        file.as_file().set_len(0)?;
        stream(request(client, url)?, &mut file, name)?;
    } else {
        stream(response, &mut file, name)?;
    }
    Ok(file)
}

fn request(client: &Client, url: &Url) -> Result<Response> {
    let response = client
        .get(url.clone())
        .header(header::ACCEPT_ENCODING, "identity")
        .send()?
        .error_for_status()?;
    ensure!(
        response.status() == StatusCode::OK,
        "expected complete download from {url}"
    );
    Ok(response)
}

fn parallel_parameters(response: &Response) -> Option<(u64, header::HeaderValue)> {
    let total = response.content_length()?;
    if total < PARALLEL_THRESHOLD
        || response.headers().get(header::ACCEPT_RANGES)?.as_bytes() != b"bytes"
        || response.headers().contains_key(header::CONTENT_ENCODING)
    {
        return None;
    }
    // Strong ETags prevent silently joining ranges from different package versions.
    let validator = response.headers().get(header::ETAG)?;
    let text = validator.to_str().ok()?;
    if !text.starts_with('"') || !text.ends_with('"') {
        return None;
    }
    Some((total, validator.clone()))
}

fn stream(mut response: Response, file: &mut NamedTempFile, name: &str) -> Result<()> {
    let total = response.content_length();
    let mut progress = DownloadProgress::new(name.to_owned(), total);
    crate::space::temporary(file.as_file(), total.unwrap_or(BUFFER_SIZE as u64))?;
    let mut writer = crate::space::TemporaryWriter(file.as_file_mut());
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut received = 0;
    loop {
        let count = read(&mut response, &mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        received += count as u64;
        progress.advance(count);
    }
    ensure!(
        total.is_none_or(|total| total == received),
        "incomplete package download"
    );
    writer.flush()?;
    progress.finish();
    Ok(())
}

fn read(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn parallel(
    client: &Client,
    url: &Url,
    response: Response,
    file: &File,
    name: &str,
    total: u64,
    validator: &header::HeaderValue,
) -> Result<()> {
    crate::space::temporary(file, total)?;
    file.set_len(total)?;
    let received = AtomicU64::new(0);
    let cancelled = AtomicBool::new(false);
    let mut progress = DownloadProgress::new(name.to_owned(), Some(total));
    thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        let chunk = total.div_ceil(CONNECTIONS);
        let mut first = Some(response);
        for index in 0..CONNECTIONS {
            let start = index * chunk;
            let end = (start + chunk).min(total) - 1;
            let initial = if index == 0 { first.take() } else { None };
            let sender = sender.clone();
            let received = &received;
            let cancelled = &cancelled;
            scope.spawn(move || {
                let result = (|| -> Result<()> {
                    let is_initial = initial.is_some();
                    let mut response = match initial {
                        Some(response) => response,
                        None => {
                            let response = client
                                .get(url.clone())
                                .header(header::ACCEPT_ENCODING, "identity")
                                .header(header::RANGE, format!("bytes={start}-{end}"))
                                .header(header::IF_RANGE, validator.clone())
                                .send()?
                                .error_for_status()?;
                            ensure!(
                                response.status() == StatusCode::PARTIAL_CONTENT,
                                "server did not honor download range"
                            );
                            let expected = format!("bytes {start}-{end}/{total}");
                            ensure!(
                                response
                                    .headers()
                                    .get(header::CONTENT_RANGE)
                                    .is_some_and(|value| value.as_bytes() == expected.as_bytes()),
                                "incorrect download range"
                            );
                            ensure!(
                                response.headers().get(header::ETAG) == Some(validator),
                                "package changed during download"
                            );
                            ensure!(
                                !response.headers().contains_key(header::CONTENT_ENCODING),
                                "encoded download range"
                            );
                            response
                        }
                    };
                    let mut offset = start;
                    let mut buffer = vec![0; BUFFER_SIZE];
                    while offset <= end {
                        ensure!(!cancelled.load(Ordering::Relaxed), "download cancelled");
                        let limit = (end - offset + 1).min(BUFFER_SIZE as u64) as usize;
                        let count = read(&mut response, &mut buffer[..limit])?;
                        ensure!(count > 0, "incomplete download range");
                        crate::space::temporary(
                            file,
                            total.saturating_sub(received.load(Ordering::Relaxed)),
                        )?;
                        file.write_all_at(&buffer[..count], offset)?;
                        offset += count as u64;
                        received.fetch_add(count as u64, Ordering::Relaxed);
                    }
                    if !is_initial {
                        ensure!(
                            read(&mut response, &mut buffer[..1])? == 0,
                            "oversized download range"
                        );
                    }
                    Ok(())
                })();
                if result.is_err() {
                    cancelled.store(true, Ordering::Relaxed);
                }
                let _ = sender.send(result);
            });
        }
        drop(sender);
        let mut completed = 0;
        let mut error = None;
        let mut displayed = 0;
        while completed < CONNECTIONS {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(result) => {
                    completed += 1;
                    if let Err(failure) = result {
                        error.get_or_insert(failure);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("download worker stopped")
                }
            }
            let current = received.load(Ordering::Relaxed);
            progress.advance((current - displayed) as usize);
            displayed = current;
        }
        if let Some(error) = error {
            return Err(error);
        }
        ensure!(displayed == total, "incomplete parallel download");
        Ok(())
    })?;
    progress.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::{TcpListener, TcpStream},
        sync::{Arc, atomic::AtomicUsize},
        time::Instant,
    };

    #[derive(Clone, Copy)]
    enum Mode {
        Ranges,
        NoRanges,
        WeakEtag,
        UnknownLength,
        IgnoreRanges,
        Changed,
        BadRange,
        Truncated,
        TruncatedFull,
    }

    struct Server {
        url: Url,
        stopped: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
        ranges: Arc<AtomicUsize>,
        full: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
    }

    impl Server {
        fn new(body: Vec<u8>, mode: Mode) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = Url::parse(&format!(
                "http://{}/package",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let stopped = Arc::new(AtomicBool::new(false));
            let ranges = Arc::new(AtomicUsize::new(0));
            let full = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let active = Arc::new(AtomicUsize::new(0));
            let body = Arc::new(body);
            let stop = stopped.clone();
            let range_count = ranges.clone();
            let full_count = full.clone();
            let peak_count = peak.clone();
            let worker = thread::spawn(move || {
                let mut workers = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    let body = body.clone();
                    let ranges = range_count.clone();
                    let full = full_count.clone();
                    let active = active.clone();
                    let peak = peak_count.clone();
                    workers.push(thread::spawn(move || {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                        peak.fetch_max(current, Ordering::Relaxed);
                        let _ = serve(&mut socket, &body, mode, &ranges, &full);
                        active.fetch_sub(1, Ordering::Relaxed);
                    }));
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            });
            Self {
                url,
                stopped,
                worker: Some(worker),
                ranges,
                full,
                peak,
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn serve(
        socket: &mut TcpStream,
        body: &[u8],
        mode: Mode,
        ranges: &AtomicUsize,
        full: &AtomicUsize,
    ) -> io::Result<()> {
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            if socket.read(&mut byte)? == 0 {
                return Ok(());
            }
            request.push(byte[0]);
        }
        let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
        let range = request
            .lines()
            .find_map(|line| line.strip_prefix("range: bytes="));
        if range.is_some() {
            ranges.fetch_add(1, Ordering::Relaxed);
        } else {
            full.fetch_add(1, Ordering::Relaxed);
        }
        let mut bytes = body;
        let mut headers = String::new();
        let mut status = "200 OK";
        if let Some(range) = range.filter(|_| !matches!(mode, Mode::IgnoreRanges)) {
            assert!(request.contains("if-range: \"v1\""));
            let (start, end) = range.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap();
            bytes = &body[start..=end];
            status = "206 Partial Content";
            let reported_start = if matches!(mode, Mode::BadRange) {
                start + 1
            } else {
                start
            };
            headers += &format!(
                "Content-Range: bytes {reported_start}-{end}/{}\r\n",
                body.len()
            );
        }
        if !matches!(mode, Mode::UnknownLength) {
            headers += &format!("Content-Length: {}\r\n", bytes.len());
        }
        if !matches!(mode, Mode::NoRanges) {
            headers += "Accept-Ranges: bytes\r\n";
        }
        let etag = match mode {
            Mode::WeakEtag => "W/\"v1\"",
            Mode::Changed if range.is_some() => "\"v2\"",
            _ => "\"v1\"",
        };
        write!(
            socket,
            "HTTP/1.1 {status}\r\n{headers}ETag: {etag}\r\nConnection: close\r\n\r\n"
        )?;
        if (matches!(mode, Mode::Truncated) && range.is_some())
            || matches!(mode, Mode::TruncatedFull)
        {
            bytes = &bytes[..bytes.len() / 2];
        }
        for chunk in bytes.chunks(64 * 1024) {
            thread::sleep(Duration::from_millis(3));
            socket.write_all(chunk)?;
        }
        Ok(())
    }

    fn body(size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| ((index * 31 + index / 997) % 251) as u8)
            .collect()
    }

    #[test]
    fn parallel_download_is_exact_and_concurrent() {
        let bytes = body(2 * 1024 * 1024 + 17);
        let server = Server::new(bytes.clone(), Mode::Ranges);
        let client = Client::new();
        let start = Instant::now();
        let file = download(&client, &server.url, "test-1.0.0").unwrap();
        let parallel_time = start.elapsed();
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
        assert_eq!(server.ranges.load(Ordering::Relaxed), 3);
        assert_eq!(server.full.load(Ordering::Relaxed), 1);
        assert!(server.peak.load(Ordering::Relaxed) > 1);
        let start = Instant::now();
        stream(
            request(&client, &server.url).unwrap(),
            &mut NamedTempFile::new().unwrap(),
            "test-1.0.0",
        )
        .unwrap();
        eprintln!(
            "Controlled download benchmark: parallel={parallel_time:?}, single={:?}",
            start.elapsed()
        );
    }

    #[test]
    fn invalid_ranges_fall_back_to_a_clean_complete_download() {
        for mode in [
            Mode::IgnoreRanges,
            Mode::Changed,
            Mode::BadRange,
            Mode::Truncated,
        ] {
            let bytes = body(1024 * 1024 + 7);
            let server = Server::new(bytes.clone(), mode);
            let file = download(&Client::new(), &server.url, "test-1.0.0").unwrap();
            assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
            assert!(server.ranges.load(Ordering::Relaxed) > 0);
            assert_eq!(server.full.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn truncated_complete_downloads_are_rejected() {
        let server = Server::new(body(1024 * 1024), Mode::TruncatedFull);
        assert!(download(&Client::new(), &server.url, "test-1.0.0").is_err());
        assert_eq!(server.full.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn small_and_non_range_downloads_use_one_request() {
        for (mode, size) in [
            (Mode::Ranges, 17),
            (Mode::Ranges, 0),
            (Mode::NoRanges, 1024 * 1024),
            (Mode::WeakEtag, 1024 * 1024),
            (Mode::UnknownLength, 1024 * 1024),
        ] {
            let bytes = body(size);
            let server = Server::new(bytes.clone(), mode);
            let file = download(&Client::new(), &server.url, "test-1.0.0").unwrap();
            assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
            assert_eq!(server.ranges.load(Ordering::Relaxed), 0);
            assert_eq!(server.full.load(Ordering::Relaxed), 1);
        }
    }
}
