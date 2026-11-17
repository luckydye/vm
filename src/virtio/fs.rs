//! virtio-fs device: one shared host directory, served by [`crate::fuse::Share`].
//!
//! The device only queues requests; a [`Worker`] thread runs the FUSE server
//! so host file I/O never blocks the vCPU that notified the queue.
//!
//! https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html#x1-49600011

use crate::{
    fuse::Share,
    memory::GuestMemory,
    virtio::{
        MmioTransport,
        chain::ChainData,
        common,
        device::{ChainToken, Device, DeviceContext, ExternalEventHandler},
    },
};
use std::sync::{
    Mutex,
    mpsc::{Receiver, Sender, channel},
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

const DEVICE_ID: u32 = 26;
/// One high-priority queue (FORGET) and one request queue.
const NUM_QUEUES: u16 = 2;
const TAG_LEN: usize = 36;

#[derive(IntoBytes, FromBytes, Immutable)]
#[repr(C, packed)]
struct FsConfig {
    tag: [u8; TAG_LEN],
    num_request_queues: u32,
}

struct Request {
    data: ChainData,
    token: ChainToken,
}

pub struct Fs {
    config: FsConfig,
    requests: Sender<Request>,
}

/// Serves one share's requests in order on its own thread.
pub struct Worker {
    requests: Receiver<Request>,
    share: Share,
}

/// A request the worker has answered, with the reply length.
pub struct Completed {
    token: ChainToken,
    written: u32,
}

pub fn channel_for(tag: &str, share: Share) -> (Fs, Worker) {
    assert!(tag.len() < TAG_LEN, "virtio-fs tag {tag:?} is too long");
    let mut config = FsConfig {
        tag: [0; TAG_LEN],
        num_request_queues: u32::from(NUM_QUEUES) - 1,
    };
    config.tag[..tag.len()].copy_from_slice(tag.as_bytes());

    let (requests, receiver) = channel();
    (
        Fs { config, requests },
        Worker {
            requests: receiver,
            share,
        },
    )
}

impl Worker {
    pub fn run(mut self, transport: &Mutex<MmioTransport<Fs>>, mem: &GuestMemory) {
        while let Ok(Request { data, token }) = self.requests.recv() {
            let mut request = vec![0u8; data.readable_len()];
            let written = if data.read_at(0, &mut request, mem).is_none() {
                eprintln!("virtio-fs: failed to read request from guest memory");
                0
            } else {
                match self.share.handle(&request) {
                    Some(reply) => data.write_response(&reply, mem),
                    None => 0,
                }
            };
            transport
                .lock()
                .unwrap()
                .handle_external_event(Completed { token, written }, mem);
        }
    }
}

impl Device for Fs {
    fn id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        common::feature::VERSION_1
    }

    fn num_queues(&self) -> u16 {
        NUM_QUEUES
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let bytes = self.config.as_bytes();
        let offset = offset as usize;
        let end = (offset + data.len()).min(bytes.len());
        if offset < end {
            data[..end - offset].copy_from_slice(&bytes[offset..end]);
        }
    }

    fn queue_notified(&mut self, queue_idx: usize, ctx: &mut DeviceContext<'_>) {
        while let Some(chain) = ctx.pop_chain(queue_idx) {
            self.requests
                .send(Request {
                    data: chain.data,
                    token: chain.token,
                })
                .expect("virtio-fs worker exited");
        }
    }
}

impl ExternalEventHandler for Fs {
    type Event<'a> = Completed;

    fn on_event(&mut self, completed: Completed, ctx: &mut DeviceContext<'_>) {
        ctx.complete(completed.token, completed.written);
    }
}
