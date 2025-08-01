// Copyright (C) 2024, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Responsible for creating a [quiche::Connection] and managing I/O.

use std::slice::Iter;
use std::time::Duration;
use std::time::Instant;

use crate::client::QUIC_VERSION;
use crate::frame::H3iFrame;
use crate::quiche;

use crate::actions::h3::Action;
use crate::actions::h3::StreamEventType;
use crate::actions::h3::WaitType;
use crate::actions::h3::WaitingFor;
use crate::client::execute_action;
use crate::client::parse_streams;
use crate::client::ClientError;
use crate::client::ConnectionCloseDetails;
use crate::client::MAX_DATAGRAM_SIZE;
use crate::config::Config;

use super::parse_args;
use super::Client;
use super::CloseTriggerFrames;
use super::ConnectionSummary;
use super::ParsedArgs;
use super::StreamMap;
use super::StreamParserMap;

#[derive(Default)]
struct SyncClient {
    streams: StreamMap,
    stream_parsers: StreamParserMap,
}

impl SyncClient {
    fn new(close_trigger_frames: Option<CloseTriggerFrames>) -> Self {
        Self {
            streams: StreamMap::new(close_trigger_frames),
            ..Default::default()
        }
    }
}

impl Client for SyncClient {
    fn stream_parsers_mut(&mut self) -> &mut StreamParserMap {
        &mut self.stream_parsers
    }

    fn handle_response_frame(&mut self, stream_id: u64, frame: H3iFrame) {
        self.streams.insert(stream_id, frame);
    }
}

fn create_config(args: &Config, should_log_keys: bool) -> quiche::Config {
    // Create the configuration for the QUIC connection.
    let mut config = quiche::Config::new(QUIC_VERSION).unwrap();

    config.verify_peer(args.verify_peer);
    config.set_application_protos(&[b"h3"]).unwrap();
    config.set_max_idle_timeout(args.idle_timeout);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config
        .set_initial_max_stream_data_bidi_local(args.max_stream_data_bidi_local);
    config.set_initial_max_stream_data_bidi_remote(
        args.max_stream_data_bidi_remote,
    );
    config.set_initial_max_stream_data_uni(args.max_stream_data_uni);
    config.set_initial_max_streams_bidi(args.max_streams_bidi);
    config.set_initial_max_streams_uni(args.max_streams_uni);
    config.set_disable_active_migration(true);
    config.set_active_connection_id_limit(0);

    config.set_max_connection_window(args.max_window);
    config.set_max_stream_window(args.max_stream_window);
    config.grease(false);

    if should_log_keys {
        config.log_keys()
    }

    config
}

/// Connect to a server and execute provided actions.
///
/// Constructs a socket and [quiche::Connection] based on the provided `args`,
/// then iterates over `actions`.
///
/// If `close_trigger_frames` is specified, h3i will close the connection
/// immediately upon receiving all of the supplied frames rather than waiting
/// for the idle timeout. See [`CloseTriggerFrames`] for details.
///
/// Returns a [ConnectionSummary] on success, [ClientError] on failure.
pub fn connect(
    args: Config, actions: Vec<Action>,
    close_trigger_frames: Option<CloseTriggerFrames>,
) -> std::result::Result<ConnectionSummary, ClientError> {
    let mut buf = [0; 65535];
    let mut out = [0; MAX_DATAGRAM_SIZE];

    let ParsedArgs {
        connect_url,
        bind_addr,
        peer_addr,
    } = parse_args(&args);

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Create the UDP socket backing the QUIC connection, and register it with
    // the event loop.
    let mut socket = mio::net::UdpSocket::bind(bind_addr).unwrap();
    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();

    let mut keylog = None;
    if let Some(keylog_path) = std::env::var_os("SSLKEYLOGFILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(keylog_path)
            .unwrap();

        keylog = Some(file);
    }

    let mut config = create_config(&args, keylog.is_some());

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut scid);

    let scid = quiche::ConnectionId::from_ref(&scid);

    let Ok(local_addr) = socket.local_addr() else {
        return Err(ClientError::Other("invalid socket".to_string()));
    };

    // Create a QUIC connection and initiate handshake.
    let mut conn =
        quiche::connect(connect_url, &scid, local_addr, peer_addr, &mut config)
            .map_err(|e| ClientError::Other(e.to_string()))?;

    if let Some(keylog) = &mut keylog {
        if let Ok(keylog) = keylog.try_clone() {
            conn.set_keylog(Box::new(keylog));
        }
    }

    log::info!(
        "connecting to {peer_addr:} from {local_addr:} with scid {scid:?}",
    );

    let mut app_proto_selected = false;

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");

    while let Err(e) = socket.send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            log::debug!(
                "{} -> {}: send() would block",
                socket.local_addr().unwrap(),
                send_info.to
            );
            continue;
        }

        return Err(ClientError::Other(format!("send() failed: {e:?}")));
    }

    let app_data_start = std::time::Instant::now();

    let mut action_iter = actions.iter();
    let mut wait_duration = None;
    let mut wait_instant = None;

    let mut client = SyncClient::new(close_trigger_frames);
    let mut waiting_for = WaitingFor::default();

    loop {
        let actual_sleep = match (wait_duration, conn.timeout()) {
            (Some(wait), Some(timeout)) => {
                #[allow(clippy::comparison_chain)]
                if timeout < wait {
                    // shave some off the wait time so it doesn't go longer
                    // than user really wanted.
                    let new = wait - timeout;
                    wait_duration = Some(new);
                    Some(timeout)
                } else if wait < timeout {
                    Some(wait)
                } else {
                    // same, so picking either doesn't matter
                    Some(timeout)
                }
            },
            (None, Some(timeout)) => Some(timeout),
            (Some(wait), None) => Some(wait),
            _ => None,
        };

        log::debug!("actual sleep is {actual_sleep:?}");
        poll.poll(&mut events, actual_sleep).unwrap();

        // If the event loop reported no events, run a belt and braces check on
        // the quiche connection's timeouts.
        if events.is_empty() {
            log::debug!("timed out");

            conn.on_timeout();
        }

        // Read incoming UDP packets from the socket and feed them to quiche,
        // until there are no more packets to read.
        for event in &events {
            let socket = match event.token() {
                mio::Token(0) => &socket,

                _ => unreachable!(),
            };

            let local_addr = socket.local_addr().unwrap();
            'read: loop {
                let (len, from) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,

                    Err(e) => {
                        // There are no more UDP packets to read on this socket.
                        // Process subsequent events.
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            break 'read;
                        }

                        return Err(ClientError::Other(format!(
                            "{local_addr}: recv() failed: {e:?}"
                        )));
                    },
                };

                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from,
                };

                // Process potentially coalesced packets.
                let _read = match conn.recv(&mut buf[..len], recv_info) {
                    Ok(v) => v,

                    Err(e) => {
                        log::debug!("{local_addr}: recv failed: {e:?}");
                        continue 'read;
                    },
                };
            }
        }

        log::debug!("done reading");

        if conn.is_closed() {
            log::info!(
                "connection closed with error={:?} did_idle_timeout={}, stats={:?} path_stats={:?}",
                conn.peer_error(),
                conn.is_timed_out(),
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>(),
            );

            if !conn.is_established() {
                log::info!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            break;
        }

        // Create a new application protocol session once the QUIC connection is
        // established.
        if (conn.is_established() || conn.is_in_early_data()) &&
            !app_proto_selected
        {
            app_proto_selected = true;
        }

        if app_proto_selected {
            check_duration_and_do_actions(
                &mut wait_duration,
                &mut wait_instant,
                &mut action_iter,
                &mut conn,
                &mut waiting_for,
                client.stream_parsers_mut(),
            );

            let mut wait_cleared = false;
            for response in parse_streams(&mut conn, &mut client) {
                let stream_id = response.stream_id;

                if let StreamEventType::Finished = response.event_type {
                    waiting_for.clear_waits_on_stream(stream_id);
                } else {
                    waiting_for.remove_wait(response);
                }

                wait_cleared = true;
            }

            if client.streams.all_close_trigger_frames_seen() {
                client.streams.close_due_to_trigger_frames(&mut conn);
            }

            if wait_cleared {
                check_duration_and_do_actions(
                    &mut wait_duration,
                    &mut wait_instant,
                    &mut action_iter,
                    &mut conn,
                    &mut waiting_for,
                    client.stream_parsers_mut(),
                );
            }
        }

        // Provides as many CIDs as possible.
        while conn.scids_left() > 0 {
            let (scid, reset_token) = generate_cid_and_reset_token();

            if conn.new_scid(&scid, reset_token, false).is_err() {
                break;
            }
        }

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        let sockets = vec![&socket];

        for socket in sockets {
            let local_addr = socket.local_addr().unwrap();

            for peer_addr in conn.paths_iter(local_addr) {
                loop {
                    let (write, send_info) = match conn.send_on_path(
                        &mut out,
                        Some(local_addr),
                        Some(peer_addr),
                    ) {
                        Ok(v) => v,

                        Err(quiche::Error::Done) => {
                            break;
                        },

                        Err(e) => {
                            log::error!(
                                "{local_addr} -> {peer_addr}: send failed: {e:?}"
                            );

                            conn.close(false, 0x1, b"fail").ok();
                            break;
                        },
                    };

                    if let Err(e) = socket.send_to(&out[..write], send_info.to) {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            log::debug!(
                                "{} -> {}: send() would block",
                                local_addr,
                                send_info.to
                            );
                            break;
                        }

                        return Err(ClientError::Other(format!(
                            "{} -> {}: send() failed: {:?}",
                            local_addr, send_info.to, e
                        )));
                    }
                }
            }
        }

        if conn.is_closed() {
            log::info!(
                "connection closed, {:?} {:?}",
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>()
            );

            if !conn.is_established() {
                log::info!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            break;
        }
    }

    Ok(ConnectionSummary {
        stream_map: client.streams,
        stats: Some(conn.stats()),
        path_stats: conn.path_stats().collect(),
        conn_close_details: ConnectionCloseDetails::new(&conn),
    })
}

fn check_duration_and_do_actions(
    wait_duration: &mut Option<Duration>, wait_instant: &mut Option<Instant>,
    action_iter: &mut Iter<Action>, conn: &mut quiche::Connection,
    waiting_for: &mut WaitingFor, stream_parsers: &mut StreamParserMap,
) {
    match wait_duration.as_ref() {
        None => {
            if let Some(idle_wait) =
                handle_actions(action_iter, conn, waiting_for, stream_parsers)
            {
                *wait_duration = Some(idle_wait);
                *wait_instant = Some(Instant::now());

                // TODO: the wait period could still be larger than the
                // negotiated idle timeout.
                // We could in theory check quiche's idle_timeout value if
                // it was public.
                log::info!(
                    "waiting for {idle_wait:?} before executing more actions"
                );
            }
        },

        Some(period) => {
            let now = Instant::now();
            let then = wait_instant.unwrap();
            log::debug!(
                "checking if actions wait period elapsed {:?} > {:?}",
                now.duration_since(then),
                wait_duration
            );
            if now.duration_since(then) >= *period {
                log::debug!("yup!");
                *wait_duration = None;

                if let Some(idle_wait) =
                    handle_actions(action_iter, conn, waiting_for, stream_parsers)
                {
                    *wait_duration = Some(idle_wait);
                }
            }
        },
    }
}

/// Generate a new pair of Source Connection ID and reset token.
pub fn generate_cid_and_reset_token() -> (quiche::ConnectionId<'static>, u128) {
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut scid);
    let scid = scid.to_vec().into();
    let mut reset_token = [0; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut reset_token);
    let reset_token = u128::from_be_bytes(reset_token);
    (scid, reset_token)
}

fn handle_actions<'a, I>(
    iter: &mut I, conn: &mut quiche::Connection, waiting_for: &mut WaitingFor,
    stream_parsers: &mut StreamParserMap,
) -> Option<Duration>
where
    I: Iterator<Item = &'a Action>,
{
    if !waiting_for.is_empty() {
        log::debug!(
            "won't fire an action due to waiting for responses: {waiting_for:?}"
        );
        return None;
    }

    // Send actions
    for action in iter {
        match action {
            Action::FlushPackets => return None,
            Action::Wait { wait_type } => match wait_type {
                WaitType::WaitDuration(period) => return Some(*period),
                WaitType::StreamEvent(response) => {
                    log::info!(
                        "waiting for {response:?} before executing more actions"
                    );
                    waiting_for.add_wait(response);
                    return None;
                },
            },
            action => execute_action(action, conn, stream_parsers),
        }
    }

    None
}
