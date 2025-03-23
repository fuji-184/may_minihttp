use std::io;

use crate::request::MAX_HEADERS;

use bytes::BytesMut;

pub struct Response<'a> {
    headers: [Header; MAX_HEADERS],
    headers_len: usize,
    status_message: StatusMessage,
    body: Body,
    rsp_buf: &'a mut BytesMut,
    header_storage: Vec<String>,
}

#[derive(Clone, Copy)]
enum Header {
    Static(&'static str),
    Owned(usize),
    Empty,
}

enum Body {
    Str(&'static str),
    String(String),
    Vec(Vec<u8>),
    Dummy,
}

struct StatusMessage {
    code: usize,
    msg: &'static str,
}

impl<'a> Response<'a> {
    pub(crate) fn new(rsp_buf: &'a mut BytesMut) -> Response<'a> {
        let headers = [Header::Empty; 32];

        Response {
            headers,
            headers_len: 0,
            body: Body::Dummy,
            status_message: StatusMessage {
                code: 200,
                msg: "Ok",
            },
            rsp_buf,
            header_storage: Vec::new(),
        }
    }

    #[inline]
    pub fn status_code(&mut self, code: usize, msg: &'static str) -> &mut Self {
        self.status_message = StatusMessage { code, msg };
        self
    }

    #[inline]
    pub fn header(&mut self, header: &'static str) -> &mut Self {
        self.headers[self.headers_len] = Header::Static(header);
        self.headers_len += 1;
        self
    }

    #[inline]
    pub fn header_str(&mut self, header: String) -> &mut Self {
        let index = self.header_storage.len();
        self.header_storage.push(header);
        self.headers[self.headers_len] = Header::Owned(index);
        self.headers_len += 1;
        self
    }

    #[inline]
    pub fn body(&mut self, s: &'static str) {
        self.body = Body::Str(s);
    }

    #[inline]
    pub fn body_string(&mut self, s: String) {
        self.body = Body::String(s);
    }

    #[inline]
    pub fn body_vec(&mut self, v: Vec<u8>) {
        self.body = Body::Vec(v);
    }

    #[inline]
    pub fn body_mut(&mut self) -> &mut BytesMut {
        match &self.body {
            Body::Dummy => {}
            Body::Str(s) => {
                self.rsp_buf.extend_from_slice(s.as_bytes());
                self.body = Body::Dummy;
            }
            Body::String(s) => {
                self.rsp_buf.extend_from_slice(s.as_bytes());
                self.body = Body::Dummy;
            }
            Body::Vec(v) => {
                self.rsp_buf.extend_from_slice(v);
                self.body = Body::Dummy;
            }
        }
        self.rsp_buf
    }

    #[inline]
    fn body_len(&self) -> usize {
        match &self.body {
            Body::Dummy => self.rsp_buf.len(),
            Body::Str(s) => s.len(),
            Body::String(s) => s.len(),
            Body::Vec(v) => v.len(),
        }
    }

    #[inline]
    fn get_body(&mut self) -> &[u8] {
        match &self.body {
            Body::Dummy => self.rsp_buf.as_ref(),
            Body::Str(s) => s.as_bytes(),
            Body::String(s) => s.as_bytes(),
            Body::Vec(v) => v,
        }
    }

    #[inline]
    fn get_header(&self, index: usize) -> &str {
        match &self.headers[index] {
            Header::Static(s) => s,
            Header::Owned(idx) => &self.header_storage[*idx],
            Header::Empty => "",
        }
    }
}

impl Drop for Response<'_> {
    fn drop(&mut self) {
        self.rsp_buf.clear();
    }
}

pub(crate) fn encode(mut rsp: Response, buf: &mut BytesMut) {
    if rsp.status_message.code == 200 {
        buf.extend_from_slice(b"HTTP/1.1 200 Ok\r\nServer: M\r\nDate: ");
    } else {
        buf.extend_from_slice(b"HTTP/1.1 ");
        let mut code = itoa::Buffer::new();
        buf.extend_from_slice(code.format(rsp.status_message.code).as_bytes());
        buf.extend_from_slice(b" ");
        buf.extend_from_slice(rsp.status_message.msg.as_bytes());
        buf.extend_from_slice(b"\r\nServer: M\r\nDate: ");
    }
    crate::date::append_date(buf);
    buf.extend_from_slice(b"\r\nContent-Length: ");
    let mut length = itoa::Buffer::new();
    buf.extend_from_slice(length.format(rsp.body_len()).as_bytes());

    // SAFETY: we already have bound check when insert headers
    for i in 0..rsp.headers_len {
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(rsp.get_header(i).as_bytes());
    }

    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(rsp.get_body());
}

#[cold]
pub(crate) fn encode_error(e: io::Error, buf: &mut BytesMut) {
    error!("error in service: err = {:?}", e);
    let msg_string = e.to_string();
    let msg = msg_string.as_bytes();

    buf.extend_from_slice(b"HTTP/1.1 500 Internal Server Error\r\nServer: M\r\nDate: ");
    crate::date::append_date(buf);
    buf.extend_from_slice(b"\r\nContent-Length: ");
    let mut length = itoa::Buffer::new();
    buf.extend_from_slice(length.format(msg.len()).as_bytes());

    buf.extend_from_slice(b"\r\n\r\n");
    buf.extend_from_slice(msg);
}
