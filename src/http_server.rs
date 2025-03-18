//! http server implementation on top of `MAY`

use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::net::ToSocketAddrs;

use crate::request::{self, Request};
use crate::response::{self, Response};

#[cfg(unix)]
use bytes::Buf;
use bytes::{BufMut, BytesMut};
#[cfg(unix)]
use may::io::WaitIo;
use may::net::{TcpListener, TcpStream};
use may::{coroutine, go};
use tungstenite::accept;
use tungstenite::WebSocket;
use base64::{engine::general_purpose, Engine as _};
use sha1::{Digest, Sha1};

macro_rules! t_c {
    ($e: expr) => {
        match $e {
            Ok(val) => val,
            Err(err) => {
                error!("call = {:?}\nerr = {:?}", stringify!($e), err);
                continue;
            }
        }
    };
}

/// the http service trait
/// user code should supply a type that impl the `call` method for the http server
///
pub trait HttpService {
    fn call(&mut self, req: Request, rsp: &mut Response) -> io::Result<()>;
}

pub trait WsService: Send {
    /// Called when WebSocket connection is established
    fn on_connect(&mut self, path: &str, ctx: &mut WsContext) -> io::Result<()>;

    /// Called when message is received
    fn on_message(&mut self, opcode: u8, payload: &[u8], ctx: &mut WsContext) -> io::Result<()>;

    /// Called when connection is closed
    fn on_close(&mut self, code: u16, reason: &str, ctx: &mut WsContext) -> io::Result<()>;
}

// WebSocket context for sending messages
pub struct WsContext<'a> {
    stream: &'a mut TcpStream,
    write_buf: BytesMut,
}

impl<'a> WsContext<'a> {
    /// Send text message
    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send_frame(0x1, text.as_bytes())
    }

    /// Send binary message
    pub fn send_binary(&mut self, data: &[u8]) -> io::Result<()> {
        self.send_frame(0x2, data)
    }

    /// Send raw WebSocket frame
    pub fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        self.write_buf.clear();

        // Build frame header
        self.write_buf.put_u8(0x80 | opcode); // FIN + opcode

        // Payload length
        if payload.len() < 126 {
            self.write_buf.put_u8(payload.len() as u8);
        } else if payload.len() < 65536 {
            self.write_buf.put_u8(126);
            self.write_buf.put_u16(payload.len() as u16);
        } else {
            self.write_buf.put_u8(127);
            self.write_buf.put_u64(payload.len() as u64);
        }

        // Add payload
        self.write_buf.extend_from_slice(payload);

        // Write to stream
        self.stream.write_all(&self.write_buf)
    }

fn with_buf<F>(&mut self, f: F) -> io::Result<()>
    where
        F: FnOnce(&mut BytesMut) -> io::Result<()>,
    {
        let old_len = self.write_buf.len();
        f(&mut self.write_buf)?;

        // Tulis ke stream jika ada data baru
        if self.write_buf.len() > old_len {
            self.stream.write_all(&self.write_buf[old_len..])?;
        }
        Ok(())
    }

    // Method untuk mengirim pong
    pub fn send_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.with_buf(|buf| {
            buf.clear();
            buf.put_u8(0x8A); // FIN + Pong opcode

            if !payload.is_empty() {
                // Tulis payload length
                if payload.len() < 126 {
                    buf.put_u8(payload.len() as u8);
                } else if payload.len() < 65536 {
                    buf.put_u8(126);
                    buf.put_u16(payload.len() as u16);
                } else {
                    buf.put_u8(127);
                    buf.put_u64(payload.len() as u64);
                }

                // Salin payload
                buf.extend_from_slice(payload);
            } else {
                buf.put_u8(0x00);
            }
            Ok(())
        })
    }
}

pub trait HttpServiceFactory: Send + Sized + 'static {
    type Service: HttpService + Send;
    type MyWsService: WsService + Send;
    // create a new http service for each connection
    fn new_service(&self, id: usize) -> Self::Service;
    fn new_ws_service(&self) -> Self::MyWsService;

    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        go!(
            coroutine::Builder::new().name("TcpServerFac".to_owned()),
            move || {
                #[cfg(unix)]
                use std::os::fd::AsRawFd;
                #[cfg(windows)]
                use std::os::windows::io::AsRawSocket;
                for stream in listener.incoming() {


                    let mut stream = t_c!(stream);
                    #[cfg(unix)]
                    let id = stream.as_raw_fd() as usize;
                    #[cfg(windows)]
                    let id = stream.as_raw_socket() as usize;
                    // t_c!(stream.set_nodelay(true));
                    let service = self.new_service(id);
                    let ws_service = self.new_ws_service();
                    let builder = may::coroutine::Builder::new().id(id);
                    go!(
                        builder,
                        move || if let Err(e) = each_connection_loop(&mut stream, service, ws_service) {
                            error!("service err = {:?}", e);
                            stream.shutdown(std::net::Shutdown::Both).ok();
                        }
                    )
                    .unwrap();



                }
            }



        )
    }
}

#[inline]
#[cold]
pub(crate) fn err<T>(e: io::Error) -> io::Result<T> {
    Err(e)
}

#[cfg(unix)]
#[inline]
fn nonblock_read(stream: &mut impl Read, req_buf: &mut BytesMut) -> io::Result<bool> {
    reserve_buf(req_buf);
    let read_buf: &mut [u8] = unsafe { std::mem::transmute(req_buf.chunk_mut()) };
    let len = read_buf.len();

    let mut read_cnt = 0;
    while read_cnt < len {
        match stream.read(unsafe { read_buf.get_unchecked_mut(read_cnt..) }) {
            Ok(0) => return err(io::Error::new(io::ErrorKind::BrokenPipe, "read closed")),
            Ok(n) => read_cnt += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return err(e),
        }
    }

    unsafe { req_buf.advance_mut(read_cnt) };
    Ok(read_cnt < len)
}

#[cfg(unix)]
#[inline]
fn nonblock_write(stream: &mut impl Write, rsp_buf: &mut BytesMut) -> io::Result<usize> {
    let write_buf = rsp_buf.chunk();
    let len = write_buf.len();
    let mut write_cnt = 0;
    while write_cnt < len {
        match stream.write(unsafe { write_buf.get_unchecked(write_cnt..) }) {
            Ok(0) => return err(io::Error::new(io::ErrorKind::BrokenPipe, "write closed")),
            Ok(n) => write_cnt += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return err(e),
        }
    }
    rsp_buf.advance(write_cnt);
    Ok(write_cnt)
}

const BUF_LEN: usize = 4096 * 8;
#[inline]
pub(crate) fn reserve_buf(buf: &mut BytesMut) {
    let rem = buf.capacity() - buf.len();
    if rem < 1024 {
        buf.reserve(BUF_LEN - rem);
    }
}

/// this is the generic type http server
/// with a type parameter that impl `HttpService` trait
///
pub struct HttpServer<T, W>(pub T, pub W);

#[cfg(unix)]
fn each_connection_loop<T: HttpService, W: WsService>(stream: &mut TcpStream, mut service: T, mut ws_service: W) -> io::Result<()> {
    let mut req_buf = BytesMut::with_capacity(BUF_LEN);
    let mut rsp_buf = BytesMut::with_capacity(BUF_LEN);
    let mut body_buf = BytesMut::with_capacity(4096);

    loop {
        let read_blocked = nonblock_read(stream.inner_mut(), &mut req_buf)?;

        // Process requests
        loop {
            let mut headers = [MaybeUninit::uninit(); request::MAX_HEADERS];
            let req_result = request::decode(&mut headers, &mut req_buf, stream);

            match req_result? {
                Some(req) => {
                    let is_ws = is_websocket_upgrade(&req);
                    let path = req.path().to_string();  // Konversi ke String yang dimiliki

                    if is_ws {
                        let ws_key = get_websocket_key(&req).map(|k| k.to_vec());
                        if let Some(ws_key) = ws_key {
                            rsp_buf.clear();

                            let mut accept_key = None;

                              for header in req.headers() {
                                if header.name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
                                    const MAGIC_STRING: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

                                    let mut hasher = Sha1::new();
                                    hasher.update(header.value);
                                    hasher.update(MAGIC_STRING);
                                    let hash_result = hasher.finalize();

                                    accept_key = Some(general_purpose::STANDARD.encode(hash_result));
                                    break;
                                }
                            }

                            if let Some(accept_key) = accept_key {
                                rsp_buf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
                                rsp_buf.extend_from_slice(b"Upgrade: websocket\r\n");
                                rsp_buf.extend_from_slice(b"Connection: Upgrade\r\n");
                                rsp_buf.extend_from_slice(b"Sec-WebSocket-Accept: ");
                                rsp_buf.extend_from_slice(accept_key.as_bytes());
                                rsp_buf.extend_from_slice(b"\r\n\r\n");

                                stream.write_all(&rsp_buf)?;

                                rsp_buf.clear();

                                return handle_websocket_frames_zero_copy(stream, &mut req_buf, rsp_buf, ws_service, path);
                            } else {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, "Could not generate WebSocket accept key"));
                            }
                        } else {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "Missing WebSocket key"));
                        }
                    }

                    reserve_buf(&mut rsp_buf);
                    let mut rsp = Response::new(&mut body_buf);
                    match service.call(req, &mut rsp) {
                        Ok(()) => response::encode(rsp, &mut rsp_buf),
                        Err(e) => {
                            eprintln!("service err = {:?}", e);
                            response::encode_error(e, &mut rsp_buf);
                        }
                    }

                    req_buf.clear();
                },
                None => break,
            }
        }

        nonblock_write(stream.inner_mut(), &mut rsp_buf)?;

        if read_blocked {
            stream.wait_io();
        }
    }
}

 fn get_websocket_key<'a>(req: &'a Request<'a, 'a, 'a>) -> Option<&'a [u8]> {
    for header in req.headers() {
        if header.name.eq_ignore_ascii_case("Sec-WebSocket-Key") {
            return Some(&header.value);
        }
    }
    None
}

fn is_websocket_upgrade(req: &Request) -> bool {
    let has_connection_upgrade = req.headers().iter()
        .any(|h| h.name.eq_ignore_ascii_case("Connection") &&
             h.value.windows(7).any(|w| w.eq_ignore_ascii_case(b"upgrade")));

    if !has_connection_upgrade {
        return false;
    }

    let has_upgrade_websocket = req.headers().iter()
        .any(|h| h.name.eq_ignore_ascii_case("Upgrade") &&
             h.value.eq_ignore_ascii_case(b"websocket"));

    if !has_upgrade_websocket {
        return false;
    }

    req.headers().iter()
        .any(|h| h.name.eq_ignore_ascii_case("Sec-WebSocket-Key"))
}


fn handle_websocket_frames_zero_copy<W: WsService + Send>(
    stream: &mut TcpStream,
    mut read_buf: &mut BytesMut,
    mut write_buf: BytesMut,
    mut ws: W,
    path: String
) -> io::Result<()> {

    let mut ctx = WsContext {
        stream,
        write_buf: write_buf,
    };

    // Call on_connect handler
    ws.on_connect(&path, &mut ctx)?;

    read_buf.clear();

    loop {
let mut offset = 0;

        while offset + 2 <= read_buf.len() {
            // Parse header
            let header_start = offset;
            let fin = (read_buf[header_start] & 0x80) != 0;
            let opcode = read_buf[header_start] & 0x0F;
            let masked = (read_buf[header_start + 1] & 0x80) != 0;
            let payload_len_byte = read_buf[header_start + 1] & 0x7F;

            // Hitung payload_len dan header_size
            let (payload_len, header_size) = match payload_len_byte {
                126 => {
                    if header_start + 4 > read_buf.len() {
                        break;
                    }
                    let len = ((read_buf[header_start + 2] as u16) << 8) |
                             (read_buf[header_start + 3] as u16);
                    (len as usize, 4)
                },
                127 => {
                    if header_start + 10 > read_buf.len() {
                        break;
                    }
                    let len = ((read_buf[header_start + 2] as u64) << 56) |
                             ((read_buf[header_start + 3] as u64) << 48) |
                             ((read_buf[header_start + 4] as u64) << 40) |
                             ((read_buf[header_start + 5] as u64) << 32) |
                             ((read_buf[header_start + 6] as u64) << 24) |
                             ((read_buf[header_start + 7] as u64) << 16) |
                             ((read_buf[header_start + 8] as u64) << 8)  |
                             (read_buf[header_start + 9] as u64);
                    (len as usize, 10)
                },
                n => (n as usize, 2),
            };

            // Hitung mask_offset dan payload_offset
            let mask_offset = header_start + header_size;
            let payload_offset = mask_offset + if masked { 4 } else { 0 };
            let total_frame_size = header_size +
                                 (if masked { 4 } else { 0 }) +
                                 payload_len;

            // Pastikan frame lengkap
            if header_start + total_frame_size > read_buf.len() {
                break;
            }

            // Process the frame based on the opcode
            match opcode {
            0x8 => { // Close frame
                let mut close_code = 1000u16; // default normal closure
                let mut close_reason = "";

                // Parse close frame payload if present
                if payload_len >= 2 {
                    close_code = u16::from_be_bytes(
                        [read_buf[payload_offset], read_buf[payload_offset + 1]]
                    );
                    close_reason = std::str::from_utf8(&read_buf[payload_offset + 2..])
                        .unwrap_or("");
                }

                // Call on_close handler
                ws.on_close(close_code, close_reason, &mut ctx)?;

                // Send close response
                ctx.write_buf.clear();
                ctx.write_buf.put_u8(0x88); // FIN + Close
                ctx.write_buf.put_u8(0x00); // Empty payload
                ctx.stream.write_all(&ctx.write_buf)?;
                return Ok(());
            },


0x9 => {
    // Hitung parameter frame
    let mask_size = if masked { 4 } else { 0 };
    let payload_offset = offset + header_size + mask_size;

    // Ekstrak payload
    let payload = if payload_len > 0 {
        let mut payload_data = Vec::with_capacity(payload_len);
        if masked {
            let mask_offset = offset + header_size;
            let mask = [
                read_buf[mask_offset],
                read_buf[mask_offset + 1],
                read_buf[mask_offset + 2],
                read_buf[mask_offset + 3],
            ];

            for i in 0..payload_len {
                payload_data.push(read_buf[payload_offset + i] ^ mask[i % 4]);
            }
        } else {
            payload_data.extend_from_slice(&read_buf[payload_offset..payload_offset + payload_len]);
        }
        Some(payload_data)
    } else {
        None
    };

    // Kirim pong melalui context
    ctx.send_pong(payload.as_deref().unwrap_or_default())?;

    // Pindah ke frame berikutnya
    offset += total_frame_size;

    // Keluar dari blok match untuk melepaskan borrow
    continue;
},
                0x1 | 0x2 => { // Text/Binary frame
                let payload = &read_buf[payload_offset..payload_offset + payload_len];

                // Unmask if needed
                let mut payload_data = Vec::with_capacity(payload_len);
                if masked {
                    let mask = [
                        read_buf[mask_offset],
                        read_buf[mask_offset + 1],
                        read_buf[mask_offset + 2],
                        read_buf[mask_offset + 3],
                    ];
                    for (i, byte) in payload.iter().enumerate() {
                        payload_data.push(byte ^ mask[i % 4]);
                    }
                } else {
                    payload_data.extend_from_slice(payload);
                }

                // Call on_message handler
                ws.on_message(opcode, &payload_data, &mut ctx)?;
            },               // Ignore other opcodes (continuation frames, etc.)
                _ => {}
            }

            // Move to next frame
            offset += total_frame_size;
        }

        // Remove processed frames from buffer
        if offset > 0 {
            if offset < read_buf.len() {
                // Retain unprocessed data - use zero-copy approach when possible
                read_buf.advance(offset);
            } else {
                // All data processed
                read_buf.clear();
            }
        }
    }
}




#[cfg(not(unix))]
fn each_connection_loop<T: HttpService>(stream: &mut TcpStream, mut service: T) -> io::Result<()> {
    let mut req_buf = BytesMut::with_capacity(BUF_LEN);
    let mut rsp_buf = BytesMut::with_capacity(BUF_LEN);
    let mut body_buf = BytesMut::with_capacity(BUF_LEN);
    loop {
        // read the socket for requests
        reserve_buf(&mut req_buf);
        let read_buf: &mut [u8] = unsafe { std::mem::transmute(&mut *req_buf.chunk_mut()) };
        let read_cnt = stream.read(read_buf)?;
        if read_cnt == 0 {
            //connection was closed
            return err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
        }
        unsafe { req_buf.advance_mut(read_cnt) };

        // prepare the requests
        if read_cnt > 0 {
            loop {
                let mut headers = [MaybeUninit::uninit(); request::MAX_HEADERS];
                let req = match request::decode(&mut headers, &mut req_buf, stream)? {
                    Some(req) => req,
                    None => break,
                };
                let mut rsp = Response::new(&mut body_buf);
                match service.call(req, &mut rsp) {
                    Ok(()) => response::encode(rsp, &mut rsp_buf),
                    Err(e) => {
                        eprintln!("service err = {:?}", e);
                        response::encode_error(e, &mut rsp_buf);
                    }
                }
                req_buf.clear();
            }
        }

        // send the result back to client
        stream.write_all(&rsp_buf)?;
    }
}

impl<T: HttpService + Clone + Send + Sync + 'static, W: WsService + Clone + Send + Sync + 'static> HttpServer<T, W> {
    /// Spawns the http service, binding to the given address
    /// return a coroutine that you can cancel it when need to stop the service
    pub fn start<L: ToSocketAddrs>(self, addr: L) -> io::Result<coroutine::JoinHandle<()>> {
        let listener = TcpListener::bind(addr)?;
        let service = self.0;
        let ws_service = self.1;
        go!(
            coroutine::Builder::new().name("TcpServer".to_owned()),
            move || {
                for stream in listener.incoming() {
                    let mut stream = t_c!(stream);
                    // t_c!(stream.set_nodelay(true));
                    let service = service.clone();
                    let ws_service = ws_service.clone();
                    go!(
                        move || if let Err(e) = each_connection_loop(&mut stream, service, ws_service) {
                            error!("service err = {:?}", e);
                            stream.shutdown(std::net::Shutdown::Both).ok();
                        }
                    );
                }
            }
        )
    }
}
