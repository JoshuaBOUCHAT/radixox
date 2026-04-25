use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    task::{Poll, Waker},
};

use monoio::{io::AsyncWriteRentExt, net::tcp::TcpOwnedWriteHalf};
use radixox_lib::{
    gen_arena::{GenArena, Key},
    shared_byte::SharedByte,
    shared_frame::extend_encode,
};

use crate::{Frame, IOResult};

// ── Conn ─────────────────────────────────────────────────────────────────────

pub(crate) struct Conn {
    write: Option<TcpOwnedWriteHalf>,
    pub(crate) cancelation: CancelationFutur,
    io_buffer: Vec<u8>,
    swap_buffer: Option<Vec<u8>>,
    channel_count: usize,
}

impl Conn {
    fn new(write: TcpOwnedWriteHalf, io_buffer: Vec<u8>, cancelation: CancelationFutur) -> Self {
        let swap_buffer = Some(Vec::with_capacity(io_buffer.capacity()));
        Self {
            write: Some(write),
            cancelation,
            io_buffer,
            swap_buffer,
            channel_count: 0,
        }
    }

    /// Swap io_buffer ↔ swap_buffer and take the write half.
    /// Returns None if write_task already holds the write half.
    fn get_conn_writer(&mut self) -> Option<ConnWriter> {
        if self.write.is_none() || self.swap_buffer.is_none() {
            return None;
        }
        std::mem::swap(&mut self.io_buffer, self.swap_buffer.as_mut()?);
        let buffer = self.swap_buffer.take()?;
        let write = self.write.take()?;
        Some(ConnWriter { write, buffer })
    }

    fn restitute_conn_writer(&mut self, cw: ConnWriter) {
        debug_assert!(self.write.is_none() && self.swap_buffer.is_none());
        self.write = Some(cw.write);
        self.swap_buffer = Some(cw.buffer);
    }
}

// ── ConnWriter ────────────────────────────────────────────────────────────────

struct ConnWriter {
    write: TcpOwnedWriteHalf,
    buffer: Vec<u8>,
}

impl ConnWriter {
    async fn write_all(mut self) -> (IOResult<()>, Self) {
        let (res, mut buffer) = self.write.write_all(self.buffer).await;
        buffer.clear();
        (
            res.map(|_| ()),
            Self {
                write: self.write,
                buffer,
            },
        )
    }
}

// ── SubId ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubId(pub(crate) Key);

// ── CancelationFutur ──────────────────────────────────────────────────────────

#[derive(Default)]
enum CancelationState {
    #[default]
    Submit,
    Running(Waker),
    Canceled(SharedByte),
}

#[derive(Default, Clone)]
pub(crate) struct CancelationFutur {
    state: Rc<RefCell<CancelationState>>,
}

impl CancelationFutur {
    /// Signal write-side failure. Wakes the main task if it's polling this future.
    pub(crate) fn cancel(&self, reason: SharedByte) {
        let mut state = self.state.borrow_mut();
        let old = std::mem::replace(&mut *state, CancelationState::Canceled(reason));
        if let CancelationState::Running(waker) = old {
            waker.wake();
        }
    }
}

impl Future for CancelationFutur {
    type Output = SharedByte;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        match &mut *state {
            CancelationState::Submit => {
                *state = CancelationState::Running(cx.waker().clone());
                Poll::Pending
            }
            CancelationState::Running(w) => {
                *w = cx.waker().clone();
                Poll::Pending
            }
            CancelationState::Canceled(r) => Poll::Ready(r.clone()),
        }
    }
}

// ── SubRegistry ───────────────────────────────────────────────────────────────

pub(crate) struct SubRegistry {
    conn_arena: GenArena<Conn>,
    conn_map: HashMap<SharedByte, Vec<SubId>>,
}

impl Default for SubRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubRegistry {
    const DEFAULT_CAPACITY: usize = 64;

    pub(crate) fn new() -> Self {
        Self {
            conn_arena: GenArena::with_capacity(Self::DEFAULT_CAPACITY),
            conn_map: HashMap::with_capacity(Self::DEFAULT_CAPACITY),
        }
    }

    pub(crate) fn get(&self, SubId(key): SubId) -> Option<&Conn> {
        self.conn_arena.get(key)
    }

    pub(crate) fn get_mut(&mut self, SubId(key): SubId) -> Option<&mut Conn> {
        self.conn_arena.get_mut(key)
    }

    /// Normal→PubSub on first channel, or add channel if already PubSub.
    /// Returns (cancelation, sub_id, total_channel_count).
    pub(crate) fn subscribe(
        &mut self,
        conn_state: &mut ConnState,
        channel: SharedByte,
    ) -> (CancelationFutur, SubId, usize) {
        let sub_id = match conn_state {
            ConnState::Normal(_, _) => {
                let old = conn_state.take();
                let ConnState::Normal(write, io_buf) = old else {
                    unreachable!()
                };
                let cancelation = CancelationFutur::default();
                let mut conn = Conn::new(write, io_buf, cancelation.clone());
                conn.channel_count = 1;
                let sub_id = SubId(self.conn_arena.insert(conn));
                *conn_state = ConnState::PubSub(sub_id);
                sub_id
            }
            ConnState::PubSub(sub_id) => {
                let sub_id = *sub_id;
                if let Some(conn) = self.conn_arena.get_mut(sub_id.0) {
                    conn.channel_count += 1;
                }
                sub_id
            }
            _ => panic!("subscribe called on invalid ConnState"),
        };

        self.conn_map.entry(channel).or_default().push(sub_id);

        let count = self.conn_arena.get(sub_id.0).map_or(0, |c| c.channel_count);
        let cancelation = self
            .conn_arena
            .get(sub_id.0)
            .map(|c| c.cancelation.clone())
            .unwrap_or_default();

        (cancelation, sub_id, count)
    }

    /// Remove channels. Transitions to Normal if count reaches 0 and write is free.
    /// Returns RESP confirmation frames to send back.
    pub(crate) fn unsubscribe(
        &mut self,
        conn_state: &mut ConnState,
        channels: &[SharedByte],
    ) -> Vec<Frame> {
        let ConnState::PubSub(sub_id) = *conn_state else {
            return vec![];
        };

        let to_remove: Vec<SharedByte> = if channels.is_empty() {
            self.conn_map
                .iter()
                .filter(|(_, subs)| subs.contains(&sub_id))
                .map(|(ch, _)| ch.clone())
                .collect()
        } else {
            channels.to_vec()
        };

        for ch in &to_remove {
            if let Some(subs) = self.conn_map.get_mut(ch) {
                subs.retain(|&id| id != sub_id);
                if subs.is_empty() {
                    self.conn_map.remove(ch);
                }
            }
            if let Some(conn) = self.conn_arena.get_mut(sub_id.0) {
                conn.channel_count = conn.channel_count.saturating_sub(1);
            }
        }

        let remaining = self.conn_arena.get(sub_id.0).map_or(0, |c| c.channel_count);

        let frames: Vec<Frame> = to_remove
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                Frame::Array(vec![
                    Frame::BulkString(SharedByte::from_str("unsubscribe")),
                    Frame::BulkString(ch.clone()),
                    Frame::Integer(remaining.saturating_sub(to_remove.len() - 1 - i) as i64),
                ])
            })
            .collect();

        // Transition back to Normal if fully unsubscribed and write half is free
        if remaining == 0 {
            let write_and_buf = self.conn_arena.get_mut(sub_id.0).and_then(|conn| {
                conn.write
                    .take()
                    .map(|w| (w, std::mem::take(&mut conn.io_buffer)))
            });
            if let Some((write, io_buf)) = write_and_buf {
                self.conn_arena.remove(sub_id.0);
                *conn_state = ConnState::Normal(write, io_buf);
            }
            // If write is None (write_task running): stay PubSub with 0 channels.
            // TODO: write_done_tx pattern (see CONN_DESIGN §Transition Pub→Normal)
        }

        frames
    }

    /// Write message into all subscriber io_buffers.
    /// Returns (response_frame, sub_ids to flush).
    pub(crate) fn publish_encode(&mut self, args: &[SharedByte]) -> (Frame, Vec<SubId>) {
        if args.len() < 2 {
            return (
                Frame::Error("ERR wrong number of arguments for 'PUBLISH' command".into()),
                vec![],
            );
        }
        let encoded = encode_pubsub_message(&args[0], &args[1]);
        let Some(subs) = self.conn_map.get(&args[0]) else {
            return (Frame::Integer(0), vec![]);
        };
        let subs = subs.clone();
        let count = subs.len() as i64;
        let mut to_flush = Vec::with_capacity(subs.len());
        for sub_id in &subs {
            if let Some(conn) = self.conn_arena.get_mut(sub_id.0) {
                conn.io_buffer.extend_from_slice(&encoded);
                to_flush.push(*sub_id);
            }
        }
        (Frame::Integer(count), to_flush)
    }

    /// Trigger a write_task for sub_id if one isn't already running.
    pub(crate) fn trigger_write(shared: &Rc<RefCell<SubRegistry>>, sub_id: SubId) {
        let maybe = shared
            .borrow_mut()
            .get_mut(sub_id)
            .and_then(|c| c.get_conn_writer());
        let Some(cw) = maybe else { return };
        monoio::spawn(write_task(cw, shared.clone(), sub_id));
    }

    /// Full cleanup on connection close.
    pub(crate) fn cleanup(&mut self, sub_id: SubId) {
        self.conn_map.retain(|_, subs| {
            subs.retain(|&id| id != sub_id);
            !subs.is_empty()
        });
        self.conn_arena.remove(sub_id.0);
    }
}

// ── ConnState ─────────────────────────────────────────────────────────────────

pub(crate) enum ConnState {
    Normal(TcpOwnedWriteHalf, Vec<u8>),
    PubSub(SubId),
    Blocking,
    None,
}

impl ConnState {
    pub(crate) fn take(&mut self) -> ConnState {
        std::mem::replace(self, ConnState::None)
    }

    pub(crate) async fn send(
        &mut self,
        frame: Frame,
        shared_registry: &Rc<RefCell<SubRegistry>>,
    ) -> IOResult<()> {
        let state = self.take();
        let state = match state {
            Self::Normal(mut write, mut buf) => {
                extend_encode(&mut buf, &frame);
                let (res, mut buf) = write.write_all(buf).await;
                buf.clear();
                res?;
                Self::Normal(write, buf)
            }
            Self::PubSub(sub_id) => {
                Self::handle_pubsub_write(sub_id, shared_registry, &frame).await?;
                Self::PubSub(sub_id)
            }
            Self::Blocking => Self::Blocking,
            Self::None => panic!("send called on None ConnState"),
        };
        *self = state;
        Ok(())
    }

    async fn handle_pubsub_write(
        sub_id: SubId,
        shared_registry: &Rc<RefCell<SubRegistry>>,
        frame: &Frame,
    ) -> IOResult<()> {
        let maybe_cw = {
            let mut reg = shared_registry.borrow_mut();
            let Some(conn) = reg.get_mut(sub_id) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "subscriber connection not found",
                ));
            };
            extend_encode(&mut conn.io_buffer, frame);
            conn.get_conn_writer()
        };

        // write_task already running — data queued, it will pick it up
        let Some(cw) = maybe_cw else { return Ok(()) };

        let (res, cw) = cw.write_all().await;

        let mut reg = shared_registry.borrow_mut();
        let Some(conn) = reg.get_mut(sub_id) else {
            return res;
        };

        if res.is_err() {
            conn.cancelation.cancel(SharedByte::from_str("write error"));
            return res;
        }

        if !conn.io_buffer.is_empty() {
            monoio::spawn(write_task(cw, shared_registry.clone(), sub_id));
        } else {
            conn.restitute_conn_writer(cw);
        }
        Ok(())
    }
}

// ── write_task ────────────────────────────────────────────────────────────────

async fn write_task(mut cw: ConnWriter, shared_registry: Rc<RefCell<SubRegistry>>, sub_id: SubId) {
    {
        let mut reg = shared_registry.borrow_mut();
        let Some(conn) = reg.get_mut(sub_id) else {
            return;
        };
        std::mem::swap(&mut cw.buffer, &mut conn.io_buffer);
    }

    let (res, cw) = cw.write_all().await;

    if res.is_err() {
        if let Some(conn) = shared_registry.borrow_mut().get_mut(sub_id) {
            conn.cancelation.cancel(SharedByte::from_str("write error"));
        }
        return;
    }

    let needs_more = shared_registry
        .borrow()
        .get(sub_id)
        .map_or(false, |c| !c.io_buffer.is_empty());

    if needs_more {
        monoio::spawn(write_task(cw, shared_registry, sub_id));
    } else if let Some(conn) = shared_registry.borrow_mut().get_mut(sub_id) {
        conn.restitute_conn_writer(cw);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn encode_pubsub_message(channel: &SharedByte, message: &SharedByte) -> Vec<u8> {
    let frame = Frame::Array(vec![
        Frame::BulkString(SharedByte::from_str("message")),
        Frame::BulkString(channel.clone()),
        Frame::BulkString(message.clone()),
    ]);
    let mut buf = Vec::new();
    extend_encode(&mut buf, &frame);
    buf
}
