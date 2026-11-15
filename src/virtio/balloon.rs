use anyhow::Result;
use crate::{ VirtioDevice, DescChain, GuestError, write_le32, PAGE_SIZE };
use crate::virtio_bindings::*;

/* virtio-balloon with free page reporting only (virtio 1.2, 5.5.6.7):
 * the guest reports blocks of RAM it has freed, 2M and up on x86, and
 * Hearth drops them from its own memory with MADV_DONTNEED. Guest RAM
 * is private anonymous memory, so the host frees those pages, and when
 * the guest uses one again KVM faults in a zeroed page. Without this,
 * every page the guest ever touched stays the host's to keep.
 *
 * The balloon proper, inflating and deflating to a size the host asks
 * for, needs a way to ask, which Hearth doesn't have: the target in the
 * config space stays 0, so the driver keeps the balloon empty. Pages it
 * does put in or take out are acknowledged and left alone. */

const VIRTIO_BALLOON_F_REPORTING: u32 = 5;

/* Queues as the driver numbers them, with only REPORTING offered:
 * statsq and free_page_vq, which other features add, don't exist. */
const INFLATE_QUEUE: u32 = 0;
const DEFLATE_QUEUE: u32 = 1;
const REPORTING_QUEUE: u32 = 2;
const QUEUE_SIZE: u32 = 64;

pub struct VirtioBalloon {
    /* Bytes handed back to the host, for tests and messages. */
    pub reported: u64
}

impl VirtioBalloon {
    pub fn new() -> Self {
        VirtioBalloon { reported: 0 }
    }

    /* Drops one reported block from the host. The guest gives page
     * frames, so a block that isn't whole pages is not one it freed,
     * and stays; so does one madvise refuses. Either way the report is
     * acknowledged, as the guest only waits for that. */
    fn discard(&mut self, block: &mut [u8]) {
        let addr = block.as_mut_ptr() as usize;
        let page = PAGE_SIZE as usize;
        if addr % page != 0 || block.len() % page != 0 || block.is_empty() {
            return;
        }
        let ret = unsafe { libc::madvise(addr as *mut libc::c_void, block.len(), libc::MADV_DONTNEED) };
        if ret == 0 {
            self.reported += block.len() as u64;
        }
    }
}

impl VirtioDevice for VirtioBalloon {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BALLOON
    }

    fn name(&self) -> &'static str {
        "virtio-balloon"
    }

    fn features(&self, sel: u32) -> u32 {
        match sel {
            0 => 1 << VIRTIO_BALLOON_F_REPORTING,
            _ => 0
        }
    }

    fn queue_num_max(&self, queue: u32) -> u32 {
        match queue {
            INFLATE_QUEUE | DEFLATE_QUEUE | REPORTING_QUEUE => QUEUE_SIZE,
            _ => 0
        }
    }

    /* num_pages, the size the host wants the balloon to be, is 0; so is
     * actual, the driver's own count, which it writes and Hearth needn't
     * keep. */
    fn read_config(&mut self, _offset: u32, data: &mut [u8]) -> Result<()> {
        write_le32(data, 0, 0);
        Ok(())
    }

    /* A report is a chain of device-writable buffers, one per free
     * block; the device writes nothing into them. */
    fn process(&mut self, queue: u32, chain: &mut DescChain) -> Result<u32, GuestError> {
        if queue == REPORTING_QUEUE {
            for block in chain.write_buf.iter_mut() {
                self.discard(block);
            }
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Private anonymous pages, as guest RAM is. */
    fn anon_pages(pages: usize) -> &'static mut [u8] {
        let len = pages * PAGE_SIZE as usize;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE,
                       libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) }
    }

    fn chain(blocks: Vec<&'static mut [u8]>) -> DescChain<'static> {
        DescChain { head: 0, read_buf: vec![], write_buf: blocks }
    }

    #[test]
    fn offers_reporting_and_three_queues() {
        let b = VirtioBalloon::new();
        assert_eq!(b.features(0), 1 << 5);
        assert_eq!(b.features(1), 0);
        assert!((0..3).all(|q| b.queue_num_max(q) > 0));
        assert_eq!(b.queue_num_max(3), 0);
    }

    #[test]
    fn reported_blocks_are_dropped() {
        let page = PAGE_SIZE as usize;
        let mem = anon_pages(4);
        let base = mem.as_ptr();
        mem.fill(0xaa);
        let (reported, _kept) = mem.split_at_mut(2 * page);
        let mut b = VirtioBalloon::new();
        assert_eq!(b.process(REPORTING_QUEUE, &mut chain(vec![reported])).ok(), Some(0));
        assert_eq!(b.reported, 2 * page as u64);
        let mem = unsafe { std::slice::from_raw_parts(base, 4 * page) };
        /* Dropped pages come back zeroed; the others keep their data. */
        assert!(mem[..2 * page].iter().all(|&x| x == 0));
        assert!(mem[2 * page..].iter().all(|&x| x == 0xaa));
    }

    #[test]
    fn partial_pages_and_other_queues_are_kept() {
        let page = PAGE_SIZE as usize;
        let mem = anon_pages(2);
        let base = mem.as_ptr();
        mem.fill(0x55);
        let (head, tail) = mem.split_at_mut(page / 2);
        let mut b = VirtioBalloon::new();
        /* tail starts mid-page, so it is no page frame; head goes to the
         * inflate queue, whose pages are left alone. */
        b.process(REPORTING_QUEUE, &mut chain(vec![tail])).unwrap();
        b.process(INFLATE_QUEUE, &mut chain(vec![head])).unwrap();
        assert_eq!(b.reported, 0);
        let mem = unsafe { std::slice::from_raw_parts(base, 2 * page) };
        assert!(mem.iter().all(|&x| x == 0x55));
    }
}
