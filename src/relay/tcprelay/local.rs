#[phase(plugin, link)]
extern crate log;

use std::sync::Arc;
use std::io::{Listener, TcpListener, Acceptor, TcpStream};
use std::io::{EndOfFile};
use std::io::net::ip::{Ipv4Addr, Ipv6Addr};
use std::vec::Vec;
use std::string::String;

use config::Config;
use relay::Relay;

use relay::{SOCK5_VERSION, SOCK5_AUTH_METHOD_NONE};
use relay::{SOCK5_CMD_TCP_CONNECT, SOCK5_CMD_TCP_BIND, SOCK5_CMD_UDP_ASSOCIATE};
use relay::{SOCK5_REPLY_COMMAND_NOT_SUPPORTED, SOCK5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED};
use relay::SOCK5_REPLY_SUCCEEDED;
use relay::{SOCK5_ADDR_TYPE_IPV4, SOCK5_ADDR_TYPE_IPV6, SOCK5_ADDR_TYPE_DOMAIN_NAME};
use relay::{Sock5AddrType, Sock5AddrTypeIpv4, Sock5AddrTypeIpv6, Sock5AddrTypeDomainName};

use crypto::cipher;
use crypto::cipher::CipherVariant;
use crypto::cipher::Cipher;

pub struct TcpRelayLocal {
    config: Config,
}

impl TcpRelayLocal {
    pub fn new(c: Config) -> TcpRelayLocal {
        TcpRelayLocal {
            config: c,
        }
    }

    fn do_handshake(stream: &mut TcpStream) {
        // Read the handshake header
        // +----+----------+----------+
        // |VER | NMETHODS | METHODS  |
        // +----+----------+----------+
        // | 5  |    1     | 1 to 255 |
        // +----+----------+----------+
        let handshake_header = stream.read_exact(2).ok().expect("Error occurs while receiving handshake header");
        let (sock_ver, nmethods) = (handshake_header[0], handshake_header[1]);

        if sock_ver != SOCK5_VERSION {
            fail!("Invalid sock version {}", sock_ver);
        }

        let _ = stream.read_exact(nmethods as uint).ok().expect("Error occurs while receiving methods");
        // TODO: validating methods

        // Reply to client
        // +----+--------+
        // |VER | METHOD |
        // +----+--------+
        // | 1  |   1    |
        // +----+--------+
        let data_to_send: &[u8] = [SOCK5_VERSION, SOCK5_AUTH_METHOD_NONE];
        stream.write(data_to_send).ok().expect("Error occurs while sending handshake reply");
    }

    fn send_error_reply(stream: &mut TcpStream, err_code: u8) {
        let reply = [SOCK5_VERSION, err_code, 0x00];
        stream.write(reply).ok().expect("Error occurs while sending errors");
    }

    fn parse_request_header(stream: &mut TcpStream) -> (Vec<u8>, Sock5AddrType, String, u16) {
        let mut raw_header = Vec::new();

        let atyp = stream.read_exact(1).unwrap()[0];
        raw_header.push(atyp);
        match atyp {
            SOCK5_ADDR_TYPE_IPV4 => {
                let raw_addr = stream.read_exact(4).unwrap();
                raw_header.push_all(raw_addr.as_slice());
                let v4addr = Ipv4Addr(raw_addr[0], raw_addr[1], raw_addr[2], raw_addr[3]);

                let raw_port = stream.read_exact(2).unwrap();
                raw_header.push_all(raw_port.as_slice());
                let port = (raw_port[0] as u16 << 8) | raw_port[1] as u16;

                (raw_header, Sock5AddrTypeIpv4, v4addr.to_string(), port)
            },
            SOCK5_ADDR_TYPE_IPV6 => {
                let raw_addr = stream.read_exact(16).unwrap();
                raw_header.push_all(raw_addr.as_slice());
                let v6addr = Ipv6Addr((raw_addr[0] as u16 << 8) | raw_addr[1] as u16,
                                      (raw_addr[2] as u16 << 8) | raw_addr[3] as u16,
                                      (raw_addr[4] as u16 << 8) | raw_addr[5] as u16,
                                      (raw_addr[6] as u16 << 8) | raw_addr[7] as u16,
                                      (raw_addr[8] as u16 << 8) | raw_addr[9] as u16,
                                      (raw_addr[10] as u16 << 8) | raw_addr[11] as u16,
                                      (raw_addr[12] as u16 << 8) | raw_addr[13] as u16,
                                      (raw_addr[14] as u16 << 8) | raw_addr[15] as u16);

                let raw_port = stream.read_exact(2).unwrap();
                raw_header.push_all(raw_port.as_slice());
                let port = (raw_port[0] as u16 << 8) | raw_port[1] as u16;

                (raw_header, Sock5AddrTypeIpv6, v6addr.to_string(), port)
            },
            SOCK5_ADDR_TYPE_DOMAIN_NAME => {
                let addr_len = stream.read_exact(1).unwrap()[0];
                raw_header.push(addr_len);
                let raw_addr = stream.read_exact(addr_len as uint).unwrap();
                raw_header.push_all(raw_addr.as_slice());

                let raw_port = stream.read_exact(2).unwrap();
                raw_header.push_all(raw_port.as_slice());
                let port = (raw_port[0] as u16 << 8) | raw_port[1] as u16;

                (raw_header, Sock5AddrTypeDomainName, String::from_utf8(raw_addr).unwrap(), port)
            },
            _ => {
                // Address type not supported
                TcpRelayLocal::send_error_reply(stream, SOCK5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED);
                fail!("Unsupported address type: {}", atyp);
            }
        }
    }

    fn handle_connect_local_stream(local_stream: &mut TcpStream, remote_stream: &mut TcpStream,
                                   cipher: &mut CipherVariant) {
        let mut buf = [0u8, .. 0xffff];

        loop {
            match local_stream.read_at_least(1, buf) {
                Ok(len) => {
                    let real_buf = buf.slice_to(len);

                    let encrypted_msg = cipher.encrypt(real_buf);
                    match remote_stream.write(encrypted_msg.as_slice()) {
                        Ok(..) => {},
                        Err(err) => {
                            if err.kind != EndOfFile {
                                error!("Error occurs in handle_local_stream while writing to remote stream: {}", err);
                            }
                            break
                        }
                    }
                },
                Err(err) => {
                    if err.kind != EndOfFile {
                        debug!("Error occurs in handle_local_stream while reading from local stream: {}", err);
                    }
                    break
                }
            }
        }
    }

    fn async_handle_connect_remote_stream(mut local_stream: TcpStream, mut remote_stream: TcpStream,
                                          mut cipher: CipherVariant) {

        spawn(proc() {
            let mut buf = [0u8, .. 0xffff];

            loop {
                match remote_stream.read_at_least(1, buf) {
                    Ok(len) => {
                        let real_buf = buf.slice_to(len);

                        let decrypted_msg = cipher.decrypt(real_buf);

                        // debug!("Recv from remote: {}", decrypted_msg);

                        match local_stream.write(decrypted_msg.as_slice()) {
                            Ok(..) => {},
                            Err(err) => {
                                if err.kind != EndOfFile {
                                    debug!("Error occurs in handle_remote_stream while writing to local stream: {}", err);
                                }
                                break
                            }
                        }
                    },
                    Err(err) => {
                        if err.kind != EndOfFile {
                            error!("Error occurs in handle_remote_stream while reading from remote stream: {}", err);
                        }
                        break
                    }
                }
            }
        });
    }
}

impl Relay for TcpRelayLocal {
    fn run(&self) {
        let local_addr = self.config.local.as_slice();
        let local_port = self.config.local_port;

        let server_addr = Arc::new(self.config.server.clone());
        let server_port = Arc::new(self.config.server_port);

        let password = Arc::new(self.config.password.clone());
        let encrypt_method = Arc::new(self.config.method.clone());

        let timeout = match self.config.timeout {
            Some(timeout) => Some(timeout * 1000),
            None => None
        };

        let mut acceptor = match TcpListener::bind(local_addr, local_port).listen() {
            Ok(acpt) => acpt,
            Err(e) => {
                fail!("Error occurs while listening local address: {}", e.to_string());
            }
        };

        info!("Shadowsocks listening on {}:{}", local_addr, local_port);

        loop {
            match acceptor.accept() {
                Ok(mut stream) => {
                    stream.set_timeout(timeout);

                    let server_addr = server_addr.clone();
                    let server_port = server_port.clone();

                    let password = password.clone();
                    let encrypt_method = encrypt_method.clone();

                    spawn(proc() {
                        TcpRelayLocal::do_handshake(&mut stream);

                        let raw_header_part1 = stream.read_exact(3)
                                                        .ok().expect("Error occurs while reading request header");
                        let (sock_ver, cmd) = (raw_header_part1[0], raw_header_part1[1]);

                        if sock_ver != SOCK5_VERSION {
                            fail!("Invalid sock version {}", sock_ver);
                        }

                        let (raw_header, atyp, bind_addr, bind_port)
                                = TcpRelayLocal::parse_request_header(&mut stream);

                        debug!("SockVer {}, CMD {}, atyp {}, bind_addr {}, bind_port {}",
                               sock_ver, cmd, atyp, bind_addr, bind_port);

                        let mut remote_stream = TcpStream::connect(server_addr.as_slice(),
                                                           *server_port.deref())
                                        .ok().expect("Error occurs while connecting to remote server");

                        let mut cipher = cipher::with_name(encrypt_method.as_slice(),
                                                       password.as_slice().as_bytes())
                                                .expect("Unsupported cipher");

                        match cmd {
                            SOCK5_CMD_TCP_CONNECT => {
                                let reply = [SOCK5_VERSION, SOCK5_REPLY_SUCCEEDED,
                                                0x00, SOCK5_CMD_TCP_CONNECT, 0x00, 0x00, 0x00, 0x00, 0x10, 0x10];
                                stream.write(reply).unwrap();

                                let encrypted_header = cipher.encrypt(raw_header.as_slice());
                                remote_stream.write(encrypted_header.as_slice()).unwrap();

                                TcpRelayLocal::async_handle_connect_remote_stream(stream.clone(),
                                                                                  remote_stream.clone(),
                                                                                  cipher.clone());

                                TcpRelayLocal::handle_connect_local_stream(&mut stream,
                                                                           &mut remote_stream,
                                                                           &mut cipher);
                            },
                            SOCK5_CMD_TCP_BIND => {

                            },
                            SOCK5_CMD_UDP_ASSOCIATE => {

                            },
                            _ => {
                                // unsupported CMD
                                TcpRelayLocal::send_error_reply(&mut stream, SOCK5_REPLY_COMMAND_NOT_SUPPORTED);
                                fail!("Unsupported command");
                            }
                        }

                        drop(stream);
                        drop(remote_stream);
                    })
                },
                Err(e) => {
                    fail!("Error occurs while accepting: {}", e.to_string());
                }
            }
        }
    }
}