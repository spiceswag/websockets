use http_body_util::{BodyExt, Empty};
use hyper::{
    body::Bytes,
    client::conn::http1,
    header::{CONNECTION, HOST, UPGRADE},
    upgrade::Parts,
    Request, StatusCode,
};
use hyper_util::rt::TokioIo;
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::WebSocketError;

use super::{parsed_addr::ParsedAddr, socket::Socket};

#[derive(Debug)]
pub(super) struct Handshake {
    addr: ParsedAddr,
    key: String,
    version: usize,
    additional_headers: Vec<(String, String)>,
    subprotocols: Vec<String>,
}

impl Handshake {
    pub fn new(
        parsed_addr: &ParsedAddr,
        additional_handshake_headers: &Vec<(String, String)>,
        subprotocols: &Vec<String>,
    ) -> Self {
        // https://tools.ietf.org/html/rfc6455#section-5.3
        let mut rand_bytes = vec![0; 16];
        let mut rng = ChaCha20Rng::from_entropy();
        rng.fill_bytes(&mut rand_bytes);
        let key = base64::encode(rand_bytes);
        Self {
            addr: parsed_addr.clone(),
            key,
            // todo: support more versions
            version: 13,
            additional_headers: additional_handshake_headers.clone(),
            subprotocols: subprotocols.clone(),
        }
    }

    /// Perform a WebSocket handshake on the given socket.
    pub async fn send(&self, stream: Socket) -> Result<(Socket, Bytes), WebSocketError> {
        let mut uri = self.addr.clone();

        let io = TokioIo::new(stream);
        let (mut sender, connection) = http1::handshake(io)
            .await
            .map_err(WebSocketError::HandshakeError)?;

        tokio::spawn(async move {
            if let Err(err) = connection.await {
                println!("Connection failed: {:?}", err);
            }
        });

        uri.scheme = match uri.scheme.as_ref() {
            "ws" => "http".to_string(),
            "wss" => "https".to_string(),
            _ => unreachable!(),
        };

        let mut req_builder = Request::builder()
            .uri(Into::<String>::into(uri))
            .header(
                HOST,
                format!("{}:{}", self.addr.host.clone(), self.addr.addr.port()),
            )
            .header(UPGRADE, "websocket")
            .header(CONNECTION, "Upgrade")
            .header("Sec-WebSocket-Key", &self.key)
            .header("Sec-WebSocket-Version", self.version);

        if !self.subprotocols.is_empty() {
            req_builder =
                req_builder.header("Sec-WebSocket-Protocol", self.subprotocols.join(", "));
        }

        for (header_name, header_value) in &self.additional_headers {
            req_builder = req_builder.header(header_name, header_value);
        }

        let req = req_builder.body(Empty::<Bytes>::new()).unwrap();

        let res = sender
            .send_request(req)
            .await
            .map_err(WebSocketError::HandshakeError)?;

        if res.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(WebSocketError::HandshakeFailed {
                status_code: res.status().to_string(),
                headers: res
                    .headers()
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), value.as_bytes().to_owned()))
                    .collect(),
                body: res
                    .collect()
                    .await
                    .map_err(WebSocketError::HandshakeError)?
                    .to_bytes()
                    .into(),
            });
        }

        let upgraded = hyper::upgrade::on(res)
            .await
            .map_err(WebSocketError::HandshakeError)?;

        let parts: Parts<TokioIo<Socket>> = upgraded.downcast().unwrap_or_else(|_| unreachable!());

        let io = parts.io.into_inner();
        let buf = parts.read_buf;

        Ok((io, buf))
    }
}
