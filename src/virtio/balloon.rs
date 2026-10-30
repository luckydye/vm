//! Free page reporting only: the guest reports memory it has freed and the host
//! releases it, so a VM's footprint follows what the guest uses instead of its
//! peak. The device never asks the guest to give up memory.
//! https://docs.oasis-open.org/virtio/virtio/v1.3/csd01/virtio-v1.3-csd01.html#x1-3200005
use crate::virtio::{
    common,
    device::{Device, DeviceContext},
};

mod feature {
    pub const PAGE_REPORTING: u64 = 1 << 5;
}

const DEVICE_ID: u32 = 5;

/// The inflate and deflate queues come first and stay unused. Queues of
/// features that are not offered get no index, so reporting is the third.
const REPORTING_QUEUE: usize = 2;

pub struct Balloon;

impl Device for Balloon {
    fn id(&self) -> u32 {
        DEVICE_ID
    }

    fn features(&self) -> u64 {
        common::feature::VERSION_1 | feature::PAGE_REPORTING
    }

    fn num_queues(&self) -> u16 {
        3
    }

    /// `num_pages` (the requested balloon size) and `actual` stay zero.
    fn read_config(&self, _offset: u64, data: &mut [u8]) {
        data.fill(0);
    }

    fn queue_notified(&mut self, queue_idx: usize, ctx: &mut DeviceContext<'_>) {
        assert_eq!(queue_idx, REPORTING_QUEUE, "balloon: unexpected notify on queue {queue_idx}");
        while let Some(chain) = ctx.pop_chain(queue_idx) {
            for seg in &chain.data.writable {
                ctx.mem()
                    .discard(seg.addr, seg.len as usize)
                    .unwrap_or_else(|error| panic!("balloon: reported range outside guest RAM: {error:?}"));
            }
            ctx.complete(chain.token, 0);
        }
    }
}
